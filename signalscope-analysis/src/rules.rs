//! Lightweight correlation rules.
//!
//! Rules are *stateless*: they look at the current rolling state and
//! produce a [`CandidateFinding`] when they fire. They do NOT decide
//! whether to actually emit anything onto the bus — that decision belongs
//! to the [`LifecycleTracker`](crate::lifecycle::LifecycleTracker), which
//! sees candidates across cycles and only forwards transitions.
//!
//! Each rule attaches a stable *fingerprint* string so the tracker can
//! recognise the same operational condition over time. Two findings with
//! the same fingerprint refer to the same thing, even if other fields
//! drift.

use std::time::Duration;

use signalscope_events::FindingKind;
use time::OffsetDateTime;

use crate::windows::{
    DnsWindow, GatewayWindow, InterfaceThroughputWindow, RfEnvironmentWindow, WifiSignalWindow,
    WifiState,
};

/// Lookback window for the connected-link signal trend. Long enough to
/// average out per-sample noise, short enough to feel responsive.
const SIGNAL_LOOKBACK: Duration = Duration::from_secs(90);

/// Lookback window for the RF density trend. Scans run at ~10 s cadence,
/// so this gives us roughly 12 samples to compare halves of.
const DENSITY_LOOKBACK: Duration = Duration::from_secs(120);

/// Minimum RSSI delta (dB) that counts as a real trend rather than
/// per-sample wobble.
const SIGNAL_TREND_DB: f64 = 5.0;

/// Minimum AP-count delta that counts as a real density shift.
const DENSITY_TREND_APS: f64 = 3.0;

/// A gateway RTT sample counts as "elevated" when it sits above this
/// multiple of the window's median. Tuned to catch real outliers
/// without firing on per-sample noise.
const GW_ELEVATED_MULT: f64 = 2.0;

/// Maximum elevated-sample count that we'll treat as an *isolated*
/// outlier (potential wakeup latency) rather than sustained instability.
const GW_ISOLATED_MAX: usize = 1;

/// Peak rolling RX/TX throughput below which we consider the link to
/// have been idle. Matches the dashboard/landmark idle floor so the
/// rule and the visual story agree.
const LINK_IDLE_BPS: f64 = 50_000.0;

/// A finding as produced by a rule, before the lifecycle layer has
/// decided what (if anything) to publish.
#[derive(Debug, Clone)]
pub struct CandidateFinding {
    pub kind: FindingKind,
    /// Stable identity for this condition — e.g. `"rf_congestion:ch11"`.
    pub fingerprint: String,
    pub headline: String,
    pub confidence: f32,
    pub evidence: Vec<String>,
}

pub fn evaluate(
    wifi: &WifiState,
    gateway: &GatewayWindow,
    dns: &DnsWindow,
    signal: &WifiSignalWindow,
    env: &RfEnvironmentWindow,
    throughput: &InterfaceThroughputWindow,
    now: OffsetDateTime,
) -> Vec<CandidateFinding> {
    let mut out = Vec::new();
    if let Some(f) = rf_congestion(wifi) {
        out.push(f);
    }
    if let Some(f) = gateway_instability(gateway, throughput) {
        out.push(f);
    }
    if let Some(f) = dns_pathology(dns, gateway) {
        out.push(f);
    }
    if let Some(f) = sticky_client(wifi) {
        out.push(f);
    }
    if let Some(f) = signal_trend(signal, now) {
        out.push(f);
    }
    if let Some(f) = rf_density_trend(env, now) {
        out.push(f);
    }
    out
}

/// Local connected-channel congestion. The brief is "how hostile is the
/// airspace around my current connection?" — not "what channel is busiest
/// globally?" When unassociated, the rule stays silent rather than
/// claiming congestion against a channel nobody is sitting on.
fn rf_congestion(wifi: &WifiState) -> Option<CandidateFinding> {
    let connected_channel = wifi.current_channel?;
    let counts = wifi.neighbors_per_channel();
    let local_count = counts.get(&connected_channel.number).copied().unwrap_or(0);

    let tier = pressure_tier(local_count);
    // We only emit a finding from elevated upward — moderate / low live in
    // the panel header and don't deserve their own lifecycle entry.
    if tier < PressureTier::Elevated {
        return None;
    }

    let confidence = match tier {
        PressureTier::Elevated => 0.55,
        PressureTier::Severe => 0.8,
        _ => return None,
    };

    let label = tier.headline_label();
    Some(CandidateFinding {
        kind: FindingKind::RfCongestion,
        fingerprint: format!("rf_congestion:ch{}", connected_channel.number),
        headline: format!(
            "Local RF congestion {label} on channel {} ({local_count} APs share the channel)",
            connected_channel.number
        ),
        confidence,
        evidence: vec![
            format!(
                "APs on connected channel ({}): {local_count}",
                connected_channel.number
            ),
            format!("Total visible APs: {}", wifi.last_neighbors.len()),
            format!("Local pressure tier: {label}"),
        ],
    })
}

/// Coarse interpretation of local channel pressure. Deliberately a small
/// ladder, deliberately not exposed as a percentage — the brief is
/// "preserve epistemic humility." Used by both the analysis rule (which
/// fires from `Elevated` upward) and the TUI (which surfaces every tier
/// in the panel header).
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum PressureTier {
    Low,
    Moderate,
    Elevated,
    Severe,
}

impl PressureTier {
    pub fn headline_label(self) -> &'static str {
        match self {
            PressureTier::Low => "low",
            PressureTier::Moderate => "moderate",
            PressureTier::Elevated => "elevated",
            PressureTier::Severe => "severe",
        }
    }
}

/// Map AP-on-channel count to a pressure tier. The thresholds are coarse
/// on purpose — finer gradations would over-claim certainty against
/// inherently sparse / probabilistic scan data.
pub fn pressure_tier(local_count: usize) -> PressureTier {
    match local_count {
        0..=2 => PressureTier::Low,
        3..=5 => PressureTier::Moderate,
        6..=8 => PressureTier::Elevated,
        _ => PressureTier::Severe,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use signalscope_events::{
        BandClass, Bssid, Channel, NeighborAp, ObservationConfidence, Ssid,
    };

    fn neighbor_on(channel_num: u16) -> NeighborAp {
        NeighborAp {
            bssid: None,
            ssid: None,
            rssi_dbm: None,
            channel: Some(Channel::new(
                channel_num,
                BandClass::from_channel_number(channel_num),
                None,
            )),
            security: None,
            phy_mode: None,
            confidence: ObservationConfidence::Inferred,
        }
    }

    fn wifi_state(connected_ch: Option<u16>, neighbors: Vec<NeighborAp>) -> WifiState {
        let mut s = WifiState::default();
        s.current_ssid = Some(Ssid::new("HomeAP"));
        s.current_bssid = Some(Bssid::new("aa:bb:cc:dd:ee:01"));
        s.current_channel = connected_ch.map(|n| {
            Channel::new(n, BandClass::from_channel_number(n), None)
        });
        s.last_neighbors = neighbors;
        s
    }

    #[test]
    fn pressure_tier_ladder() {
        assert_eq!(pressure_tier(0), PressureTier::Low);
        assert_eq!(pressure_tier(2), PressureTier::Low);
        assert_eq!(pressure_tier(3), PressureTier::Moderate);
        assert_eq!(pressure_tier(5), PressureTier::Moderate);
        assert_eq!(pressure_tier(6), PressureTier::Elevated);
        assert_eq!(pressure_tier(8), PressureTier::Elevated);
        assert_eq!(pressure_tier(9), PressureTier::Severe);
        assert_eq!(pressure_tier(50), PressureTier::Severe);
    }

    #[test]
    fn congestion_stays_silent_when_unassociated() {
        let neighbors: Vec<NeighborAp> = (0..10).map(|_| neighbor_on(11)).collect();
        let state = wifi_state(None, neighbors);
        assert!(
            rf_congestion(&state).is_none(),
            "no connected channel → no local-congestion claim"
        );
    }

    #[test]
    fn congestion_stays_silent_when_connected_channel_quiet() {
        // Channel 6 is crowded but we're on 36 (which has nothing).
        let mut neighbors: Vec<NeighborAp> = (0..10).map(|_| neighbor_on(6)).collect();
        neighbors.push(neighbor_on(36)); // self
        let state = wifi_state(Some(36), neighbors);
        assert!(
            rf_congestion(&state).is_none(),
            "global busy channel ≠ local pressure"
        );
    }

    #[test]
    fn congestion_fires_only_from_elevated() {
        // 5 neighbors on the connected channel → moderate, no finding.
        let neighbors: Vec<NeighborAp> = (0..5).map(|_| neighbor_on(11)).collect();
        let state = wifi_state(Some(11), neighbors);
        assert!(rf_congestion(&state).is_none(), "moderate tier shouldn't fire");

        // 7 neighbors → elevated → fires.
        let neighbors: Vec<NeighborAp> = (0..7).map(|_| neighbor_on(11)).collect();
        let state = wifi_state(Some(11), neighbors);
        let f = rf_congestion(&state).expect("elevated should fire");
        assert!(f.fingerprint.ends_with(":ch11"));
        assert!(f.headline.contains("elevated"));
    }

    // ---------- gateway_instability ----------

    fn gw_sample(rtt_ms: u64, reachable: bool) -> signalscope_events::GatewayLatencyObservation {
        signalscope_events::GatewayLatencyObservation {
            target: "192.168.1.1".into(),
            rtt: std::time::Duration::from_millis(rtt_ms),
            reachable,
            probe: "icmp".into(),
        }
    }

    fn gw_window(rtts_ms: &[u64]) -> GatewayWindow {
        let mut w = GatewayWindow::new(std::time::Duration::from_secs(60));
        for ms in rtts_ms {
            w.record(&gw_sample(*ms, true));
        }
        w
    }

    fn gw_window_with_loss(rtts_ms: &[(u64, bool)]) -> GatewayWindow {
        let mut w = GatewayWindow::new(std::time::Duration::from_secs(60));
        for (ms, reachable) in rtts_ms {
            w.record(&gw_sample(*ms, *reachable));
        }
        w
    }

    fn empty_throughput() -> InterfaceThroughputWindow {
        InterfaceThroughputWindow::new(std::time::Duration::from_secs(15))
    }

    fn active_throughput() -> InterfaceThroughputWindow {
        use signalscope_events::InterfaceCountersObservation;
        let mut w = InterfaceThroughputWindow::new(std::time::Duration::from_secs(15));
        let now = OffsetDateTime::from_unix_timestamp(1_700_000_000).unwrap();
        // 0 → 10 MB over 10 seconds = 8 Mbps. Well above the 50 Kbps idle floor.
        let mk = |rx: u64| InterfaceCountersObservation {
            interface: "en0".into(),
            rx_bytes_total: rx,
            tx_bytes_total: 0,
            rx_packets_total: 0,
            tx_packets_total: 0,
            rx_errors_total: 0,
            tx_errors_total: 0,
            rx_dropped_total: None,
            tx_dropped_total: None,
            retry_count: None,
        };
        w.record(&mk(0), now);
        w.record(&mk(10_000_000), now + time::Duration::seconds(10));
        w
    }

    #[test]
    fn gateway_stays_silent_with_too_few_samples() {
        let gw = gw_window(&[5, 5, 5, 5]); // only 4
        let tput = empty_throughput();
        assert!(gateway_instability(&gw, &tput).is_none());
    }

    #[test]
    fn gateway_stays_silent_on_clean_baseline() {
        let gw = gw_window(&[5, 6, 5, 5, 6, 5, 5, 6, 5, 5]);
        let tput = empty_throughput();
        assert!(gateway_instability(&gw, &tput).is_none());
    }

    #[test]
    fn isolated_spike_during_idle_does_not_fire() {
        // 9 samples at 5ms, one wakeup spike at 80ms. Jitter ratio is
        // huge (p95 ≈ 80, median 5 → 16×) but only ONE elevated sample,
        // and the link is idle → suppress as likely wakeup.
        let gw = gw_window(&[5, 5, 5, 5, 5, 5, 5, 5, 5, 80]);
        let tput = empty_throughput(); // idle
        assert!(
            gateway_instability(&gw, &tput).is_none(),
            "isolated p95 spike during idle should be silent"
        );
    }

    #[test]
    fn isolated_spike_during_activity_still_fires() {
        // Same outlier, but the link is busy. The wakeup-latency
        // explanation doesn't apply, so we still surface the spike.
        let gw = gw_window(&[5, 5, 5, 5, 5, 5, 5, 5, 5, 80]);
        let tput = active_throughput();
        assert!(
            gateway_instability(&gw, &tput).is_some(),
            "spike during active link doesn't get the wakeup pass"
        );
    }

    #[test]
    fn sustained_elevation_fires_regardless_of_link_activity() {
        // Majority of samples at baseline (so median stays low), with
        // multiple elevated outliers — that's "no longer isolated",
        // regardless of whether the link was idle. This is the
        // genuine-instability path we never want to silence.
        let gw = gw_window(&[5, 5, 5, 5, 5, 5, 5, 5, 80, 90]);
        let tput = empty_throughput(); // even idle
        let finding = gateway_instability(&gw, &tput).expect("should fire");
        assert!(finding
            .evidence
            .iter()
            .any(|e| e.contains("Elevated samples")));
    }

    #[test]
    fn loss_alone_fires_even_during_idle() {
        // Loss is a hard failure mode regardless of link activity.
        // 4/10 = 40% loss → above the 5% threshold.
        let gw = gw_window_with_loss(&[
            (5, true), (5, true), (0, false), (5, true),
            (0, false), (5, true), (0, false), (5, true),
            (0, false), (5, true),
        ]);
        let tput = empty_throughput();
        let finding = gateway_instability(&gw, &tput).expect("loss must fire");
        assert!(finding.confidence > 0.3);
    }

    #[test]
    fn evidence_mentions_link_activity_state() {
        // Same shape as the sustained-elevation test — baseline median
        // with two elevated samples — so we get past the isolated-spike
        // suppression and into the firing path where evidence is built.
        let gw = gw_window(&[5, 5, 5, 5, 5, 5, 5, 5, 80, 90]);
        let idle_finding = gateway_instability(&gw, &empty_throughput()).unwrap();
        assert!(
            idle_finding.evidence.iter().any(|e| e.contains("idle")),
            "evidence should name the idle context"
        );
        let active_finding = gateway_instability(&gw, &active_throughput()).unwrap();
        assert!(
            active_finding.evidence.iter().any(|e| e.contains("active")),
            "evidence should name the active context"
        );
    }

    #[test]
    fn congestion_fingerprint_follows_connected_channel() {
        let neighbors_a: Vec<NeighborAp> = (0..7).map(|_| neighbor_on(11)).collect();
        let state_a = wifi_state(Some(11), neighbors_a);
        let neighbors_b: Vec<NeighborAp> = (0..7).map(|_| neighbor_on(36)).collect();
        let state_b = wifi_state(Some(36), neighbors_b);
        let a = rf_congestion(&state_a).unwrap();
        let b = rf_congestion(&state_b).unwrap();
        assert_ne!(
            a.fingerprint, b.fingerprint,
            "different connected channels → different fingerprints, so a roam triggers Resolved + new Active"
        );
    }
}

/// Gateway-instability rule. Fires when loss is meaningful OR when
/// multiple samples sit well above the window's median.
///
/// The second condition used to be `p95 / median ≥ 4×`, which fired
/// on a *single* big outlier — a pattern that often corresponds to
/// Wi-Fi power-save wakeup latency rather than network instability.
/// The rule now requires either real loss or **more than one elevated
/// sample**, and additionally suppresses isolated p95 spikes when the
/// link was idle in the window's lead-up. That preserves explainability
/// (the operator gets an `idle link` evidence line when relevant) while
/// removing the most common false-alarm pattern in replay.
fn gateway_instability(
    gateway: &GatewayWindow,
    throughput: &InterfaceThroughputWindow,
) -> Option<CandidateFinding> {
    if gateway.len() < 5 {
        return None;
    }
    let loss = gateway.loss_ratio();
    let median = gateway.median_rtt_ms()?;
    let p95 = gateway.p95_rtt_ms()?;
    let jitter_ratio = if median > 0.0 { p95 / median } else { 1.0 };

    // Cheap reject: nothing looks elevated at all.
    if loss < 0.05 && jitter_ratio < 4.0 {
        return None;
    }

    let elevated = gateway.samples_above_ms(median * GW_ELEVATED_MULT);
    let link_idle = link_is_idle(throughput);
    let isolated_spike = elevated <= GW_ISOLATED_MAX;

    // The wakeup-latency suppression. If the only signal is a single
    // elevated sample and the link was idle in the window, this almost
    // certainly is a radio waking up, not gateway instability. Stay
    // silent rather than emit a finding the operator would have to
    // investigate and dismiss.
    if isolated_spike && link_idle && loss < 0.05 {
        return None;
    }

    let confidence = ((loss as f64) * 1.5 + (jitter_ratio - 1.0).min(5.0) * 0.10)
        .clamp(0.0, 0.95) as f32;

    let target = gateway.target().unwrap_or("gateway");
    let mut evidence = vec![
        format!("Loss ratio: {:.2}", loss),
        format!("Median RTT: {:.1} ms", median),
        format!("p95 RTT: {:.1} ms", p95),
        format!(
            "Elevated samples (>{:.1}× median): {}/{}",
            GW_ELEVATED_MULT,
            elevated,
            gateway.len()
        ),
    ];
    // Surface link-activity context so the operator can judge — this
    // rule's noisiest false-positive class is "elevated RTT after the
    // radio woke from power-save."
    evidence.push(format!(
        "Link recently {} (rolling RX/TX peak {})",
        if link_idle { "idle" } else { "active" },
        link_peak_label(throughput),
    ));

    Some(CandidateFinding {
        kind: FindingKind::GatewayInstability,
        fingerprint: format!("gateway_instability:{target}"),
        headline: format!(
            "Gateway {target} unstable: {:.0}% loss, p95 {:.0} ms vs median {:.0} ms",
            loss * 100.0,
            p95,
            median
        ),
        confidence,
        evidence,
    })
}

/// Whether the link's recent rolling throughput peak (RX or TX) sits
/// below the idle floor. We use the rolling-average view rather than
/// the per-step view: a single burst sample shouldn't flip this from
/// "idle" to "active" — sustained traffic should.
fn link_is_idle(throughput: &InterfaceThroughputWindow) -> bool {
    match throughput.throughput_bps() {
        Some(t) => t.rx_bps.max(t.tx_bps) < LINK_IDLE_BPS,
        None => true,
    }
}

fn link_peak_label(throughput: &InterfaceThroughputWindow) -> String {
    match throughput.throughput_bps() {
        Some(t) => {
            let bps = t.rx_bps.max(t.tx_bps);
            if bps >= 1.0e6 {
                format!("{:.1} Mbps", bps / 1.0e6)
            } else if bps >= 1.0e3 {
                format!("{:.0} Kbps", bps / 1.0e3)
            } else {
                "idle".into()
            }
        }
        None => "no data".into(),
    }
}

fn dns_pathology(dns: &DnsWindow, gateway: &GatewayWindow) -> Option<CandidateFinding> {
    if dns.len() < 4 {
        return None;
    }
    let fail = dns.failure_ratio();
    let dns_median = dns.median_rtt_ms().unwrap_or(0.0);
    let gw_loss = gateway.loss_ratio();
    let gw_median = gateway.median_rtt_ms().unwrap_or(0.0);

    let gateway_is_healthy = gw_loss < 0.10 && gw_median < 80.0 && gateway.len() >= 3;

    if fail < 0.25 && dns_median < 150.0 {
        return None;
    }

    let fail64 = fail as f64;
    let confidence = if gateway_is_healthy {
        (fail64 * 1.5 + dns_median / 1000.0).clamp(0.3, 0.9) as f32
    } else {
        (fail64 + dns_median / 2000.0).clamp(0.15, 0.55) as f32
    };

    Some(CandidateFinding {
        kind: FindingKind::DnsPathology,
        fingerprint: "dns_pathology".to_string(),
        headline: format!(
            "DNS pathology: {:.0}% failures, median {:.0} ms",
            fail * 100.0,
            dns_median
        ),
        confidence,
        evidence: vec![
            format!("DNS failure ratio: {:.2}", fail),
            format!("DNS median RTT: {:.1} ms", dns_median),
            format!("Gateway looks healthy: {gateway_is_healthy}"),
            format!("DNS samples in window: {}", dns.len()),
        ],
    })
}

fn sticky_client(wifi: &WifiState) -> Option<CandidateFinding> {
    let rssi = wifi.last_rssi_dbm?;
    let ssid = wifi.current_ssid.as_ref()?;
    let bssid = wifi.current_bssid.as_ref()?;
    if rssi > -70 {
        return None;
    }

    let mut best: Option<(&signalscope_events::Bssid, i32)> = None;
    for ap in &wifi.last_neighbors {
        if ap.ssid.as_ref() != Some(ssid) {
            continue;
        }
        let Some(ap_bssid) = ap.bssid.as_ref() else {
            continue;
        };
        let Some(ap_rssi) = ap.rssi_dbm else {
            continue;
        };
        if ap_bssid == bssid {
            continue;
        }
        if best.map_or(true, |(_, r)| ap_rssi > r) {
            best = Some((ap_bssid, ap_rssi));
        }
    }

    let (alt_bssid, alt_rssi) = best?;
    let delta = alt_rssi - rssi;
    if delta < 10 {
        return None;
    }

    let confidence = match delta {
        10..=14 => 0.4,
        15..=19 => 0.6,
        _ => 0.8,
    };

    Some(CandidateFinding {
        kind: FindingKind::StickyClient,
        fingerprint: format!("sticky_client:{ssid}"),
        headline: format!(
            "Sticky client suspected: holding {rssi} dBm while {alt_bssid} is {alt_rssi} dBm"
        ),
        confidence,
        evidence: vec![
            format!("Current AP RSSI: {rssi} dBm"),
            format!("Strongest same-SSID neighbor RSSI: {alt_rssi} dBm"),
            format!("Delta: {delta} dB"),
        ],
    })
}

/// Connected-link signal-quality trend. Positive delta = improving.
/// Direction is encoded in the fingerprint so a degradation that flips
/// to a recovery doesn't quietly mutate the same lifecycle entry — it
/// resolves cleanly and a new finding takes its place.
fn signal_trend(signal: &WifiSignalWindow, now: OffsetDateTime) -> Option<CandidateFinding> {
    let delta = signal.rssi_delta(SIGNAL_LOOKBACK, now)?;
    if delta.abs() < SIGNAL_TREND_DB {
        return None;
    }
    let key = signal.identity_key().unwrap_or_else(|| "current".into());
    let lookback_secs = SIGNAL_LOOKBACK.as_secs();

    let mut evidence = vec![
        format!("RSSI Δ over {lookback_secs}s: {delta:+.1} dB"),
        format!("Samples in window: {}", signal.sample_count()),
    ];
    if let Some(d) = signal.associated_duration(now) {
        evidence.push(format!("Connected for: {}s", d.as_secs()));
    }

    if delta < 0.0 {
        let magnitude = -delta;
        let confidence = (magnitude / 15.0).clamp(0.3, 0.85) as f32;
        Some(CandidateFinding {
            kind: FindingKind::SignalTrend,
            fingerprint: format!("signal_trend:{key}:degrading"),
            headline: format!(
                "Signal quality deteriorating ({delta:+.0} dB over {lookback_secs}s)"
            ),
            confidence,
            evidence,
        })
    } else {
        let confidence = (delta / 15.0).clamp(0.3, 0.85) as f32;
        Some(CandidateFinding {
            kind: FindingKind::SignalTrend,
            fingerprint: format!("signal_trend:{key}:recovering"),
            headline: format!(
                "Signal quality recovering (+{delta:.0} dB over {lookback_secs}s)"
            ),
            confidence,
            evidence,
        })
    }
}

/// RF-environment density trend. The headline frames the *change*, not
/// the absolute level — "weather is shifting" rather than "it is cloudy."
fn rf_density_trend(env: &RfEnvironmentWindow, now: OffsetDateTime) -> Option<CandidateFinding> {
    let delta = env.density_delta(DENSITY_LOOKBACK, now)?;
    if delta.abs() < DENSITY_TREND_APS {
        return None;
    }
    let lookback_secs = DENSITY_LOOKBACK.as_secs();
    let current = env.latest().map(|s| s.ap_count).unwrap_or(0);

    if delta > 0.0 {
        let confidence = (delta / 10.0).clamp(0.3, 0.8) as f32;
        Some(CandidateFinding {
            kind: FindingKind::RfDensityTrend,
            fingerprint: "rf_density_trend:rising".into(),
            headline: format!(
                "Ambient RF density rising (+{delta:.0} APs over {lookback_secs}s, now {current})"
            ),
            confidence,
            evidence: vec![
                format!("Mean AP-count Δ: {delta:+.1}"),
                format!("Current AP count: {current}"),
                format!("Scan samples in window: {}", env.sample_count()),
            ],
        })
    } else {
        let magnitude = -delta;
        let confidence = (magnitude / 10.0).clamp(0.3, 0.8) as f32;
        Some(CandidateFinding {
            kind: FindingKind::RfDensityTrend,
            fingerprint: "rf_density_trend:falling".into(),
            headline: format!(
                "Ambient RF density falling ({delta:+.0} APs over {lookback_secs}s, now {current})"
            ),
            confidence,
            evidence: vec![
                format!("Mean AP-count Δ: {delta:+.1}"),
                format!("Current AP count: {current}"),
                format!("Scan samples in window: {}", env.sample_count()),
            ],
        })
    }
}
