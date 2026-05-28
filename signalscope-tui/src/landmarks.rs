//! Replay-time *landmark* derivation: compress a long recording into
//! the small set of moments worth investigating.
//!
//! A [`TimelineLandmark`] is **derived**, never persisted. The session
//! file format remains canonical; landmarks are produced on demand by
//! walking the recorded envelope stream and naming the transitions
//! that are already first-class in the event model:
//!
//! * **Findings.** Every `Event::Finding` carries a lifecycle
//!   transition (`Active` / `Escalating` / `Recovering` / `Resolved`).
//!   The bus only emits findings on those transitions, so each one is
//!   already a moment of operational change — a landmark per finding
//!   event is the lossless choice.
//! * **Sensor health.** Each `Event::SensorHealth` represents a real
//!   state change (the sensors suppress duplicates). We skip the
//!   initial `Operational` per sensor — that's "the rig powered on,"
//!   not a moment worth jumping to — but every other state change is
//!   surfaced.
//! * **Stance changes.** Gateway / DNS / throughput don't emit a
//!   dedicated event when their regime changes; the dashboard derives
//!   the regime on the fly. Landmarks do the same derivation as a
//!   *forward* sweep across the recording and emit on every regime
//!   flip.
//!
//! The deriver is a **pure function** of the envelope vec. Given the
//! same recording it produces an identical Vec<TimelineLandmark>
//! every run. No real-clock reads, no random ordering, no scoring,
//! no LLMs — landmarks are the git-commit list of the recording, not
//! a summary.

use std::collections::{HashMap, VecDeque};
use std::sync::Arc;

use signalscope_events::{Envelope, Event, FindingLifecycle, SensorId, SensorState};
use time::OffsetDateTime;

/// One moment worth investigating. Anchored on the envelope that
/// caused the transition so navigation lands the playhead exactly on
/// the event the operator would want to inspect.
#[derive(Debug, Clone)]
pub struct TimelineLandmark {
    pub at: OffsetDateTime,
    /// Index into the `Playback` envelope vec — the playhead position
    /// `seek_to_landmark` should set.
    pub event_index: usize,
    pub category: LandmarkCategory,
    pub severity: LandmarkSeverity,
    /// One-line description, suitable for a compact list row.
    pub headline: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LandmarkCategory {
    Finding,
    Health,
    Throughput,
    Gateway,
    Dns,
}

impl LandmarkCategory {
    pub fn short_tag(self) -> &'static str {
        match self {
            LandmarkCategory::Finding => "FIND",
            LandmarkCategory::Health => "HEAL",
            LandmarkCategory::Throughput => "TPUT",
            LandmarkCategory::Gateway => "GW  ",
            LandmarkCategory::Dns => "DNS ",
        }
    }
}

/// Visual stance — drives the panel's coloring. The classification is
/// deliberately coarse: an operator scanning a list of 30 landmarks
/// wants "is this a problem or a recovery?" at a glance.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LandmarkSeverity {
    /// Active fault / escalation / degraded health. The reason you
    /// look at this landmark first.
    Alarm,
    /// Return to baseline — recovery, resolution, sensor came back.
    Recovery,
    /// Notable but not alarming (e.g. throughput went from idle to
    /// bursting on a healthy link — interesting context, not a
    /// problem).
    Notable,
}

/// Walk the recorded envelope stream and produce the ordered list of
/// landmarks. Pure function — same input always yields the same
/// output, by construction.
pub fn derive(events: &[Arc<Envelope>]) -> Vec<TimelineLandmark> {
    let mut out = Vec::new();
    let mut health_prev: HashMap<SensorId, SensorState> = HashMap::new();
    let mut gw_window: VecDeque<f64> = VecDeque::with_capacity(GW_MEDIAN_WINDOW);
    let mut gw_prev: Option<GwStance> = None;
    let mut dns_prev: Option<DnsStance> = None;
    let mut tput_prev: Option<ThroughputRegime> = None;
    let mut prev_counters: Option<(u64, u64, OffsetDateTime)> = None;

    for (idx, env) in events.iter().enumerate() {
        match &env.event {
            Event::Finding(f) => {
                out.push(landmark_for_finding(env.at, idx, f));
            }
            Event::SensorHealth(h) => {
                let prev = health_prev.get(&h.sensor).copied();
                let should_emit = match (prev, h.state) {
                    // The very first health event for a sensor is the
                    // startup heartbeat — only landmark it if the
                    // sensor came up degraded.
                    (None, SensorState::Operational) => false,
                    (Some(p), new) => p != new,
                    (None, _) => true,
                };
                if should_emit {
                    out.push(landmark_for_health(env.at, idx, h, prev));
                }
                health_prev.insert(h.sensor.clone(), h.state);
            }
            Event::GatewayLatency(o) => {
                let ms = o.rtt.as_secs_f64() * 1000.0;
                gw_window.push_back(ms);
                if gw_window.len() > GW_MEDIAN_WINDOW {
                    gw_window.pop_front();
                }
                let median = median_ms(&gw_window);
                let stance = classify_gateway(o.reachable, ms, median);
                if let Some(prev) = gw_prev {
                    if prev != stance {
                        out.push(landmark_for_gateway(env.at, idx, prev, stance, o.target.as_str(), ms));
                    }
                }
                gw_prev = Some(stance);
            }
            Event::DnsLatency(o) => {
                let stance = if o.answered {
                    DnsStance::Answering
                } else {
                    DnsStance::Failing
                };
                if let Some(prev) = dns_prev {
                    if prev != stance {
                        out.push(landmark_for_dns(env.at, idx, prev, stance, o));
                    }
                }
                dns_prev = Some(stance);
            }
            Event::InterfaceCounters(c) => {
                // Per-step throughput: a regime change is meaningful
                // only after we have two consecutive samples to compute
                // a rate from.
                if let Some((prev_rx, prev_tx, prev_at)) = prev_counters {
                    let dt = (env.at - prev_at).as_seconds_f64();
                    if dt > 0.0
                        && c.rx_bytes_total >= prev_rx
                        && c.tx_bytes_total >= prev_tx
                    {
                        let rx_bps =
                            ((c.rx_bytes_total - prev_rx) as f64 / dt) * 8.0;
                        let tx_bps =
                            ((c.tx_bytes_total - prev_tx) as f64 / dt) * 8.0;
                        let regime = classify_throughput(rx_bps, tx_bps);
                        if let Some(prev) = tput_prev {
                            if prev != regime {
                                out.push(landmark_for_throughput(
                                    env.at, idx, prev, regime, rx_bps, tx_bps,
                                ));
                            }
                        }
                        tput_prev = Some(regime);
                    }
                }
                prev_counters = Some((c.rx_bytes_total, c.tx_bytes_total, env.at));
            }
            // Scans / wifi observations / interface state changes /
            // roams aren't landmarks today. Roams might earn it
            // later; left out for now to keep the list focused.
            _ => {}
        }
    }

    out
}

// ---------- finding ----------

fn landmark_for_finding(
    at: OffsetDateTime,
    event_index: usize,
    f: &signalscope_events::CorrelationFinding,
) -> TimelineLandmark {
    let (severity, lifecycle_word) = match f.lifecycle {
        FindingLifecycle::Active => (LandmarkSeverity::Alarm, "Active"),
        FindingLifecycle::Escalating => (LandmarkSeverity::Alarm, "Escalating"),
        FindingLifecycle::Recovering => (LandmarkSeverity::Recovery, "Recovering"),
        FindingLifecycle::Resolved => (LandmarkSeverity::Recovery, "Resolved"),
    };
    TimelineLandmark {
        at,
        event_index,
        category: LandmarkCategory::Finding,
        severity,
        headline: format!("{lifecycle_word} · {}", f.headline),
    }
}

// ---------- sensor health ----------

fn landmark_for_health(
    at: OffsetDateTime,
    event_index: usize,
    h: &signalscope_events::SensorHealth,
    prev: Option<SensorState>,
) -> TimelineLandmark {
    let severity = match h.state {
        SensorState::Operational => LandmarkSeverity::Recovery,
        SensorState::Stale => LandmarkSeverity::Alarm,
        SensorState::BackendUnavailable
        | SensorState::HardwareDisabled
        | SensorState::PermissionDenied
        | SensorState::ParseFailed => LandmarkSeverity::Alarm,
    };
    let headline = match (prev, h.state) {
        (Some(p), SensorState::Operational) => {
            format!("{} recovered (was {:?})", h.sensor, p)
        }
        (_, state) => format!("{} → {:?}", h.sensor, state),
    };
    TimelineLandmark {
        at,
        event_index,
        category: LandmarkCategory::Health,
        severity,
        headline,
    }
}

// ---------- gateway ----------

const GW_MEDIAN_WINDOW: usize = 30;
const GW_SPIKE_MULT: f64 = 1.5;
const GW_SPIKE_OFFSET_MS: f64 = 5.0;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum GwStance {
    Stable,
    Spiking,
    Unreachable,
}

fn classify_gateway(reachable: bool, ms: f64, median: Option<f64>) -> GwStance {
    if !reachable {
        return GwStance::Unreachable;
    }
    let threshold = median.unwrap_or(0.0) * GW_SPIKE_MULT + GW_SPIKE_OFFSET_MS;
    if ms > threshold {
        GwStance::Spiking
    } else {
        GwStance::Stable
    }
}

fn landmark_for_gateway(
    at: OffsetDateTime,
    event_index: usize,
    prev: GwStance,
    new: GwStance,
    target: &str,
    ms: f64,
) -> TimelineLandmark {
    let severity = match new {
        GwStance::Stable => LandmarkSeverity::Recovery,
        GwStance::Spiking => LandmarkSeverity::Alarm,
        GwStance::Unreachable => LandmarkSeverity::Alarm,
    };
    let headline = match new {
        GwStance::Stable => format!("Gateway recovered · {target} {ms:.1} ms"),
        GwStance::Spiking => format!("Gateway spiking · {target} {ms:.1} ms"),
        GwStance::Unreachable => format!("Gateway unreachable · {target}"),
    };
    // Reference prev so the unused-binding lint stays quiet in case we
    // grow more directional phrasing later.
    let _ = prev;
    TimelineLandmark {
        at,
        event_index,
        category: LandmarkCategory::Gateway,
        severity,
        headline,
    }
}

// ---------- dns ----------

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum DnsStance {
    Answering,
    Failing,
}

fn landmark_for_dns(
    at: OffsetDateTime,
    event_index: usize,
    _prev: DnsStance,
    new: DnsStance,
    o: &signalscope_events::DnsLatencyObservation,
) -> TimelineLandmark {
    let (severity, headline) = match new {
        DnsStance::Failing => (
            LandmarkSeverity::Alarm,
            format!(
                "DNS failing · {} via {}{}",
                o.query,
                o.resolver,
                o.error.as_deref().map(|e| format!(" ({e})")).unwrap_or_default(),
            ),
        ),
        DnsStance::Answering => (
            LandmarkSeverity::Recovery,
            format!("DNS recovered · {} via {}", o.query, o.resolver),
        ),
    };
    TimelineLandmark {
        at,
        event_index,
        category: LandmarkCategory::Dns,
        severity,
        headline,
    }
}

// ---------- throughput ----------

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ThroughputRegime {
    Idle,
    Trickle,
    Sustained,
    Bursting,
}

fn classify_throughput(rx_bps: f64, tx_bps: f64) -> ThroughputRegime {
    let peak = rx_bps.max(tx_bps);
    if peak < 50_000.0 {
        ThroughputRegime::Idle
    } else if peak < 500_000.0 {
        ThroughputRegime::Trickle
    } else if peak < 25_000_000.0 {
        ThroughputRegime::Sustained
    } else {
        ThroughputRegime::Bursting
    }
}

fn landmark_for_throughput(
    at: OffsetDateTime,
    event_index: usize,
    prev: ThroughputRegime,
    new: ThroughputRegime,
    rx_bps: f64,
    tx_bps: f64,
) -> TimelineLandmark {
    let severity = match new {
        ThroughputRegime::Idle => LandmarkSeverity::Recovery,
        ThroughputRegime::Trickle => LandmarkSeverity::Recovery,
        ThroughputRegime::Sustained | ThroughputRegime::Bursting => LandmarkSeverity::Notable,
    };
    let new_word = match new {
        ThroughputRegime::Idle => "idle",
        ThroughputRegime::Trickle => "trickling",
        ThroughputRegime::Sustained => "sustained",
        ThroughputRegime::Bursting => "bursting",
    };
    let prev_word = match prev {
        ThroughputRegime::Idle => "idle",
        ThroughputRegime::Trickle => "trickling",
        ThroughputRegime::Sustained => "sustained",
        ThroughputRegime::Bursting => "bursting",
    };
    let headline = format!(
        "Throughput {prev_word} → {new_word} · RX {} / TX {}",
        fmt_rate(rx_bps),
        fmt_rate(tx_bps),
    );
    TimelineLandmark {
        at,
        event_index,
        category: LandmarkCategory::Throughput,
        severity,
        headline,
    }
}

fn fmt_rate(bps: f64) -> String {
    if bps >= 1.0e9 {
        format!("{:.2} Gbps", bps / 1.0e9)
    } else if bps >= 1.0e6 {
        format!("{:.1} Mbps", bps / 1.0e6)
    } else if bps >= 1.0e3 {
        format!("{:.0} Kbps", bps / 1.0e3)
    } else if bps > 0.0 {
        format!("{:.0} bps", bps)
    } else {
        "idle".into()
    }
}

// ---------- shared helpers ----------

fn median_ms(samples: &VecDeque<f64>) -> Option<f64> {
    if samples.is_empty() {
        return None;
    }
    let mut v: Vec<f64> = samples.iter().copied().collect();
    v.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
    Some(v[v.len() / 2])
}

#[cfg(test)]
mod tests {
    use super::*;
    use signalscope_events::{
        Confidence, CorrelationFinding, DnsLatencyObservation, EventId, FindingKind,
        GatewayLatencyObservation, InterfaceCountersObservation, SensorHealth,
    };

    fn env(id: u64, secs_offset: i64, ev: Event) -> Arc<Envelope> {
        Arc::new(Envelope::with_time(
            EventId(id),
            OffsetDateTime::from_unix_timestamp(1_700_000_000 + secs_offset).unwrap(),
            SensorId::new("test"),
            ev,
        ))
    }

    fn finding(lifecycle: FindingLifecycle, fingerprint: &str, headline: &str) -> Event {
        Event::Finding(CorrelationFinding {
            kind: FindingKind::GatewayInstability,
            fingerprint: fingerprint.into(),
            headline: headline.into(),
            confidence: Confidence::new(0.7),
            peak_confidence: Confidence::new(0.7),
            evidence: vec![],
            lifecycle,
            first_seen: OffsetDateTime::from_unix_timestamp(1_700_000_000).unwrap(),
            last_seen: OffsetDateTime::from_unix_timestamp(1_700_000_000).unwrap(),
        })
    }

    fn gateway(rtt_ms: u64, reachable: bool) -> Event {
        Event::GatewayLatency(GatewayLatencyObservation {
            target: "192.168.1.1".into(),
            rtt: std::time::Duration::from_millis(rtt_ms),
            reachable,
            probe: "icmp".into(),
        })
    }

    fn dns(answered: bool) -> Event {
        Event::DnsLatency(DnsLatencyObservation {
            resolver: "1.1.1.1".into(),
            query: "example.com".into(),
            rtt: std::time::Duration::from_millis(12),
            answered,
            error: if answered { None } else { Some("timeout".into()) },
        })
    }

    fn counters(rx: u64, tx: u64) -> Event {
        Event::InterfaceCounters(InterfaceCountersObservation {
            interface: "en0".into(),
            rx_bytes_total: rx,
            tx_bytes_total: tx,
            rx_packets_total: 0,
            tx_packets_total: 0,
            rx_errors_total: 0,
            tx_errors_total: 0,
            rx_dropped_total: None,
            tx_dropped_total: None,
            retry_count: None,
        })
    }

    fn health(sensor: &str, state: SensorState) -> Event {
        Event::SensorHealth(SensorHealth {
            sensor: SensorId::new(sensor),
            state,
            backend: Some("test".into()),
            detail: None,
        })
    }

    #[test]
    fn empty_recording_yields_no_landmarks() {
        let landmarks = derive(&[]);
        assert!(landmarks.is_empty());
    }

    #[test]
    fn every_finding_event_is_a_landmark() {
        let events = vec![
            env(1, 0, finding(FindingLifecycle::Active, "gw_inst:1.1.1.1", "gateway flapping")),
            env(2, 30, finding(FindingLifecycle::Escalating, "gw_inst:1.1.1.1", "gateway loss 30%")),
            env(3, 60, finding(FindingLifecycle::Recovering, "gw_inst:1.1.1.1", "gateway recovering")),
            env(4, 90, finding(FindingLifecycle::Resolved, "gw_inst:1.1.1.1", "gateway resolved")),
        ];
        let landmarks = derive(&events);
        assert_eq!(landmarks.len(), 4);
        assert_eq!(landmarks[0].severity, LandmarkSeverity::Alarm);
        assert_eq!(landmarks[1].severity, LandmarkSeverity::Alarm);
        assert_eq!(landmarks[2].severity, LandmarkSeverity::Recovery);
        assert_eq!(landmarks[3].severity, LandmarkSeverity::Recovery);
        for l in &landmarks {
            assert_eq!(l.category, LandmarkCategory::Finding);
        }
    }

    #[test]
    fn initial_operational_health_is_not_a_landmark() {
        let events = vec![env(1, 0, health("iface", SensorState::Operational))];
        assert!(derive(&events).is_empty());
    }

    #[test]
    fn initial_degraded_health_is_a_landmark() {
        let events = vec![env(1, 0, health("wifi", SensorState::PermissionDenied))];
        let landmarks = derive(&events);
        assert_eq!(landmarks.len(), 1);
        assert_eq!(landmarks[0].category, LandmarkCategory::Health);
        assert_eq!(landmarks[0].severity, LandmarkSeverity::Alarm);
    }

    #[test]
    fn health_recovery_is_a_landmark_with_recovery_severity() {
        let events = vec![
            env(1, 0, health("wifi", SensorState::Operational)),
            env(2, 30, health("wifi", SensorState::Stale)),
            env(3, 60, health("wifi", SensorState::Operational)),
        ];
        let landmarks = derive(&events);
        assert_eq!(landmarks.len(), 2);
        assert_eq!(landmarks[0].severity, LandmarkSeverity::Alarm);
        assert!(landmarks[0].headline.contains("Stale"));
        assert_eq!(landmarks[1].severity, LandmarkSeverity::Recovery);
        assert!(landmarks[1].headline.contains("recovered"));
    }

    #[test]
    fn gateway_stance_change_to_unreachable_emits_landmark() {
        let events = vec![
            env(1, 0, gateway(2, true)),
            env(2, 1, gateway(3, true)),
            env(3, 2, gateway(2, true)),
            env(4, 3, gateway(0, false)),
            env(5, 4, gateway(0, false)),
            env(6, 5, gateway(2, true)),
        ];
        let landmarks = derive(&events);
        // Transitions: Stable → Unreachable, Unreachable → Stable. Two
        // landmarks; the two reachable-and-fine and two consecutive
        // unreachables in the middle don't flip.
        let gw: Vec<_> = landmarks
            .iter()
            .filter(|l| l.category == LandmarkCategory::Gateway)
            .collect();
        assert_eq!(gw.len(), 2, "got: {gw:?}");
        assert!(gw[0].headline.contains("unreachable"));
        assert!(gw[1].headline.contains("recovered"));
    }

    #[test]
    fn dns_stance_flips_on_answered_change() {
        let events = vec![
            env(1, 0, dns(true)),
            env(2, 10, dns(true)),
            env(3, 20, dns(false)),
            env(4, 30, dns(false)),
            env(5, 40, dns(true)),
        ];
        let landmarks = derive(&events);
        let d: Vec<_> = landmarks
            .iter()
            .filter(|l| l.category == LandmarkCategory::Dns)
            .collect();
        assert_eq!(d.len(), 2);
        assert!(d[0].headline.contains("failing"));
        assert_eq!(d[0].severity, LandmarkSeverity::Alarm);
        assert!(d[1].headline.contains("recovered"));
        assert_eq!(d[1].severity, LandmarkSeverity::Recovery);
    }

    #[test]
    fn throughput_regime_change_emits_landmark() {
        // Two quiet samples then a burst.
        // 25 MB jump over 1 s = 200 Mbps → Bursting (≥ 25 Mbps threshold).
        let events = vec![
            env(1, 0, counters(1_000, 0)),
            env(2, 1, counters(1_500, 0)),
            env(3, 2, counters(26_000_000, 0)),
        ];
        let landmarks = derive(&events);
        let t: Vec<_> = landmarks
            .iter()
            .filter(|l| l.category == LandmarkCategory::Throughput)
            .collect();
        assert_eq!(t.len(), 1, "expected one regime flip, got: {t:?}");
        assert!(t[0].headline.contains("bursting"));
        assert_eq!(t[0].severity, LandmarkSeverity::Notable);
    }

    #[test]
    fn throughput_ignores_counter_reset() {
        let events = vec![
            env(1, 0, counters(1_000_000, 100_000)),
            env(2, 1, counters(2_000_000, 200_000)),
            // Reset to lower — sysinfo rebaseline. Shouldn't produce
            // a negative-rate landmark.
            env(3, 2, counters(50, 50)),
            env(4, 3, counters(100, 100)),
        ];
        let landmarks = derive(&events);
        // The two pre-reset samples bracket a Trickle → Sustained
        // transition (1 MB in 1 s = 8 Mbps, Sustained), so one
        // landmark is expected from that side.
        // Post-reset, counters go monotonically up tiny amounts → Idle.
        // The reset itself should produce no landmark.
        for l in &landmarks {
            assert!(
                !l.headline.contains("bursting") || l.event_index < 3,
                "spurious bursting landmark from reset: {l:?}"
            );
        }
    }

    #[test]
    fn deriver_is_deterministic() {
        let events = vec![
            env(1, 0, gateway(2, true)),
            env(2, 5, finding(FindingLifecycle::Active, "x", "x")),
            env(3, 10, dns(false)),
            env(4, 15, health("wifi", SensorState::Stale)),
            env(5, 20, counters(0, 0)),
            env(6, 22, counters(30_000_000, 0)),
        ];
        let a = derive(&events);
        let b = derive(&events);
        let c = derive(&events);
        assert_eq!(a.len(), b.len());
        assert_eq!(a.len(), c.len());
        for ((la, lb), lc) in a.iter().zip(b.iter()).zip(c.iter()) {
            assert_eq!(la.at, lb.at);
            assert_eq!(la.at, lc.at);
            assert_eq!(la.event_index, lb.event_index);
            assert_eq!(la.category, lb.category);
            assert_eq!(la.headline, lb.headline);
        }
    }

    #[test]
    fn landmarks_are_in_chronological_order() {
        let events = vec![
            env(1, 0, finding(FindingLifecycle::Active, "a", "a")),
            env(2, 10, gateway(0, false)),
            env(3, 20, finding(FindingLifecycle::Resolved, "a", "a")),
            env(4, 30, gateway(2, true)),
        ];
        let landmarks = derive(&events);
        for w in landmarks.windows(2) {
            assert!(
                w[0].at <= w[1].at,
                "landmarks out of order: {} then {}",
                w[0].at,
                w[1].at
            );
        }
    }
}
