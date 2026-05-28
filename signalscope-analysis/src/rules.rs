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

use crate::windows::{DnsWindow, GatewayWindow, RfEnvironmentWindow, WifiSignalWindow, WifiState};

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
    now: OffsetDateTime,
) -> Vec<CandidateFinding> {
    let mut out = Vec::new();
    if let Some(f) = rf_congestion(wifi) {
        out.push(f);
    }
    if let Some(f) = gateway_instability(gateway) {
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

fn rf_congestion(wifi: &WifiState) -> Option<CandidateFinding> {
    let counts = wifi.neighbors_per_channel();
    if counts.is_empty() {
        return None;
    }
    let (busiest_ch, busiest_count) = counts.iter().max_by_key(|(_, n)| **n)?;
    if *busiest_count < 6 {
        return None;
    }

    let confidence = match busiest_count {
        6..=8 => 0.45,
        9..=12 => 0.65,
        _ => 0.8,
    };

    Some(CandidateFinding {
        kind: FindingKind::RfCongestion,
        fingerprint: format!("rf_congestion:ch{busiest_ch}"),
        headline: format!(
            "RF congestion on channel {busiest_ch} ({busiest_count} APs visible)"
        ),
        confidence,
        evidence: vec![
            format!("Neighbor APs on channel {busiest_ch}: {busiest_count}"),
            format!("Total visible APs: {}", wifi.last_neighbors.len()),
        ],
    })
}

fn gateway_instability(gateway: &GatewayWindow) -> Option<CandidateFinding> {
    if gateway.len() < 5 {
        return None;
    }
    let loss = gateway.loss_ratio();
    let median = gateway.median_rtt_ms()?;
    let p95 = gateway.p95_rtt_ms()?;
    let jitter_ratio = if median > 0.0 { p95 / median } else { 1.0 };

    if loss < 0.05 && jitter_ratio < 4.0 {
        return None;
    }

    let confidence = ((loss as f64) * 1.5 + (jitter_ratio - 1.0).min(5.0) * 0.10)
        .clamp(0.0, 0.95) as f32;

    let target = gateway.target().unwrap_or("gateway");

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
        evidence: vec![
            format!("Loss ratio: {:.2}", loss),
            format!("Median RTT: {:.1} ms", median),
            format!("p95 RTT: {:.1} ms", p95),
            format!("Samples in window: {}", gateway.len()),
        ],
    })
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
