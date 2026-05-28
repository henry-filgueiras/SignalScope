//! Lightweight correlation rules.
//!
//! Each rule returns at most one finding per evaluation. Confidence is a
//! coarse hand-tuned score, intentionally cautious — the system should never
//! claim certainty it doesn't have.

use signalscope_events::{Confidence, CorrelationFinding, FindingKind};

use crate::windows::{DnsWindow, GatewayWindow, WifiState};

pub fn evaluate(
    wifi: &WifiState,
    gateway: &GatewayWindow,
    dns: &DnsWindow,
) -> Vec<CorrelationFinding> {
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
    out
}

/// Many neighbors on the same channel as the associated AP. Suggests RF
/// congestion / airtime contention.
fn rf_congestion(wifi: &WifiState) -> Option<CorrelationFinding> {
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

    Some(CorrelationFinding {
        kind: FindingKind::RfCongestion,
        headline: format!(
            "Likely RF congestion on channel {busiest_ch} ({busiest_count} APs visible)"
        ),
        confidence: Confidence::new(confidence),
        evidence: vec![
            format!("Neighbor APs on channel {busiest_ch}: {busiest_count}"),
            format!("Total visible APs: {}", wifi.last_neighbors.len()),
        ],
    })
}

/// Loss or wildly variable RTT on the gateway probe. Could be RF, could be
/// the gateway itself — we flag instability without naming a cause.
fn gateway_instability(gateway: &GatewayWindow) -> Option<CorrelationFinding> {
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

    let confidence = (loss * 1.5 + (jitter_ratio - 1.0).min(5.0) * 0.10)
        .clamp(0.0, 0.95);

    Some(CorrelationFinding {
        kind: FindingKind::GatewayInstability,
        headline: format!(
            "Gateway unstable: {:.0}% loss, p95 {:.0} ms vs median {:.0} ms",
            loss * 100.0,
            p95,
            median
        ),
        confidence: Confidence::new(confidence),
        evidence: vec![
            format!("Loss ratio: {:.2}", loss),
            format!("Median RTT: {:.1} ms", median),
            format!("p95 RTT: {:.1} ms", p95),
            format!("Samples in window: {}", gateway.len()),
        ],
    })
}

/// DNS failing or slow while the gateway is fine. Strongly suggests resolver
/// pathology rather than RF — that's what makes this a useful correlation.
fn dns_pathology(dns: &DnsWindow, gateway: &GatewayWindow) -> Option<CorrelationFinding> {
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

    let confidence = if gateway_is_healthy {
        (fail * 1.5 + (dns_median / 1000.0)).clamp(0.3, 0.9)
    } else {
        // Gateway also looks bad — DNS may just be downstream of the network
        // issue. Lower confidence.
        (fail + (dns_median / 2000.0)).clamp(0.15, 0.55)
    };

    Some(CorrelationFinding {
        kind: FindingKind::DnsPathology,
        headline: format!(
            "DNS pathology: {:.0}% failures, median {:.0} ms",
            fail * 100.0,
            dns_median
        ),
        confidence: Confidence::new(confidence),
        evidence: vec![
            format!("DNS failure ratio: {:.2}", fail),
            format!("DNS median RTT: {:.1} ms", dns_median),
            format!("Gateway looks healthy: {gateway_is_healthy}"),
            format!("DNS samples in window: {}", dns.len()),
        ],
    })
}

/// Very weak associated RSSI while a much stronger neighbor on the same SSID
/// is available. Classic sticky-client behavior.
fn sticky_client(wifi: &WifiState) -> Option<CorrelationFinding> {
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
        if &ap.bssid == bssid {
            continue;
        }
        if best.map_or(true, |(_, r)| ap.rssi_dbm > r) {
            best = Some((&ap.bssid, ap.rssi_dbm));
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

    Some(CorrelationFinding {
        kind: FindingKind::StickyClient,
        headline: format!(
            "Sticky client suspected: holding {rssi} dBm while {alt_bssid} is {alt_rssi} dBm"
        ),
        confidence: Confidence::new(confidence),
        evidence: vec![
            format!("Current AP RSSI: {rssi} dBm"),
            format!("Strongest same-SSID neighbor RSSI: {alt_rssi} dBm"),
            format!("Delta: {delta} dB"),
        ],
    })
}
