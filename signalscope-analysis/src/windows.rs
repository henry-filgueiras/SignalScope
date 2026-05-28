//! Rolling state windows used by the correlation rules.
//!
//! There are two categories of windows here, matching the two
//! conceptual layers SignalScope reasons about:
//!
//! * **Connected link**: the currently associated network, treated as a
//!   longitudinal entity with an association lifetime and a signal trend
//!   over that lifetime. See [`WifiSignalWindow`].
//! * **RF environment**: ambient AP activity around the host, treated as
//!   a sparse, probabilistic time series. See [`RfEnvironmentWindow`].
//!
//! Rules consume these windows; they do not poke at raw observations.

use std::collections::{HashMap, VecDeque};
use std::time::Duration;

use signalscope_events::{
    Bssid, DnsLatencyObservation, GatewayLatencyObservation, NeighborAp, RoamDetected,
    ScanResult, Ssid, WifiObservation,
};
use time::OffsetDateTime;

#[derive(Debug, Default)]
pub struct WifiState {
    pub current_ssid: Option<Ssid>,
    pub current_bssid: Option<Bssid>,
    pub current_channel: Option<signalscope_events::Channel>,
    pub last_rssi_dbm: Option<i32>,
    pub last_neighbors: Vec<NeighborAp>,
    pending_roam: Option<RoamDetected>,
}

impl WifiState {
    pub fn record_link(&mut self, obs: &WifiObservation) {
        if let (Some(prev_bssid), Some(new_bssid)) = (&self.current_bssid, &obs.bssid) {
            let same_ssid = self.current_ssid == obs.ssid;
            if prev_bssid != new_bssid && same_ssid {
                self.pending_roam = Some(RoamDetected {
                    ssid: obs.ssid.clone(),
                    from_bssid: prev_bssid.clone(),
                    to_bssid: new_bssid.clone(),
                    from_rssi_dbm: self.last_rssi_dbm,
                    to_rssi_dbm: obs.rssi_dbm,
                });
            }
        }
        self.current_ssid = obs.ssid.clone();
        self.current_bssid = obs.bssid.clone();
        self.current_channel = obs.channel;
        self.last_rssi_dbm = obs.rssi_dbm;
    }

    pub fn record_scan(&mut self, scan: &ScanResult) {
        self.last_neighbors = scan.neighbors.clone();
    }

    pub fn take_pending_roam(&mut self) -> Option<RoamDetected> {
        self.pending_roam.take()
    }

    /// Group neighbor APs by channel number — used by the RF-congestion rule.
    pub fn neighbors_per_channel(&self) -> HashMap<u16, usize> {
        let mut map = HashMap::new();
        for ap in &self.last_neighbors {
            if let Some(ch) = ap.channel {
                *map.entry(ch.number).or_insert(0) += 1;
            }
        }
        map
    }
}

#[derive(Debug, Clone)]
pub struct GatewaySample {
    pub at: OffsetDateTime,
    pub rtt: Duration,
    pub reachable: bool,
}

#[derive(Debug)]
pub struct GatewayWindow {
    span: Duration,
    samples: VecDeque<GatewaySample>,
    /// Target IP / hostname from the most recently recorded observation —
    /// used as a fingerprint discriminator so a gateway swap creates a new
    /// finding instance rather than mutating the old one.
    target: Option<String>,
}

impl GatewayWindow {
    pub fn new(span: Duration) -> Self {
        Self {
            span,
            samples: VecDeque::new(),
            target: None,
        }
    }

    pub fn record(&mut self, obs: &GatewayLatencyObservation) {
        let now = OffsetDateTime::now_utc();
        self.target = Some(obs.target.clone());
        self.samples.push_back(GatewaySample {
            at: now,
            rtt: obs.rtt,
            reachable: obs.reachable,
        });
        self.evict(now);
    }

    pub fn target(&self) -> Option<&str> {
        self.target.as_deref()
    }

    fn evict(&mut self, now: OffsetDateTime) {
        let cutoff = now - self.span;
        while let Some(front) = self.samples.front() {
            if front.at < cutoff {
                self.samples.pop_front();
            } else {
                break;
            }
        }
    }

    pub fn len(&self) -> usize {
        self.samples.len()
    }

    pub fn loss_ratio(&self) -> f32 {
        if self.samples.is_empty() {
            return 0.0;
        }
        let lost = self.samples.iter().filter(|s| !s.reachable).count() as f32;
        lost / self.samples.len() as f32
    }

    /// Median RTT (in millis) for reachable samples.
    pub fn median_rtt_ms(&self) -> Option<f64> {
        let mut v: Vec<f64> = self
            .samples
            .iter()
            .filter(|s| s.reachable)
            .map(|s| s.rtt.as_secs_f64() * 1000.0)
            .collect();
        if v.is_empty() {
            return None;
        }
        v.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
        Some(v[v.len() / 2])
    }

    pub fn p95_rtt_ms(&self) -> Option<f64> {
        let mut v: Vec<f64> = self
            .samples
            .iter()
            .filter(|s| s.reachable)
            .map(|s| s.rtt.as_secs_f64() * 1000.0)
            .collect();
        if v.is_empty() {
            return None;
        }
        v.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
        let idx = ((v.len() as f64) * 0.95).floor() as usize;
        Some(v[idx.min(v.len() - 1)])
    }
}

#[derive(Debug, Clone)]
pub struct DnsSample {
    pub at: OffsetDateTime,
    pub rtt: Duration,
    pub answered: bool,
}

#[derive(Debug)]
pub struct DnsWindow {
    span: Duration,
    samples: VecDeque<DnsSample>,
}

impl DnsWindow {
    pub fn new(span: Duration) -> Self {
        Self {
            span,
            samples: VecDeque::new(),
        }
    }

    pub fn record(&mut self, obs: &DnsLatencyObservation) {
        let now = OffsetDateTime::now_utc();
        self.samples.push_back(DnsSample {
            at: now,
            rtt: obs.rtt,
            answered: obs.answered,
        });
        let cutoff = now - self.span;
        while let Some(front) = self.samples.front() {
            if front.at < cutoff {
                self.samples.pop_front();
            } else {
                break;
            }
        }
    }

    pub fn failure_ratio(&self) -> f32 {
        if self.samples.is_empty() {
            return 0.0;
        }
        let bad = self.samples.iter().filter(|s| !s.answered).count() as f32;
        bad / self.samples.len() as f32
    }

    pub fn median_rtt_ms(&self) -> Option<f64> {
        let mut v: Vec<f64> = self
            .samples
            .iter()
            .filter(|s| s.answered)
            .map(|s| s.rtt.as_secs_f64() * 1000.0)
            .collect();
        if v.is_empty() {
            return None;
        }
        v.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
        Some(v[v.len() / 2])
    }

    pub fn len(&self) -> usize {
        self.samples.len()
    }
}

// ============================================================================
// Connected-link signal window
// ============================================================================

/// Identity of an associated network. Either component may be `None` on
/// macOS without Location Services. Two observations with the *same*
/// identity belong to the same association streak; identity changes wipe
/// the trend window because the new connection has its own story.
type AssociationIdentity = (Option<Ssid>, Option<Bssid>);

#[derive(Debug, Clone)]
pub struct WifiSignalSample {
    pub at: OffsetDateTime,
    pub rssi_dbm: i32,
}

#[derive(Debug)]
pub struct WifiSignalWindow {
    span: Duration,
    samples: VecDeque<WifiSignalSample>,
    identity: Option<AssociationIdentity>,
    associated_since: Option<OffsetDateTime>,
}

impl WifiSignalWindow {
    pub fn new(span: Duration) -> Self {
        Self {
            span,
            samples: VecDeque::new(),
            identity: None,
            associated_since: None,
        }
    }

    /// Record a connected-link observation. The window resets when the
    /// association identity (ssid, bssid) changes — a new connection has
    /// its own clock.
    pub fn record(&mut self, obs: &WifiObservation, at: OffsetDateTime) {
        let new_identity: AssociationIdentity = (obs.ssid.clone(), obs.bssid.clone());
        if self.identity.as_ref() != Some(&new_identity) {
            self.identity = Some(new_identity);
            self.associated_since = Some(at);
            self.samples.clear();
        }
        if let Some(rssi) = obs.rssi_dbm {
            self.samples.push_back(WifiSignalSample {
                at,
                rssi_dbm: rssi,
            });
            self.evict(at);
        }
    }

    /// Forget the association entirely. Call this when health goes to
    /// `HardwareDisabled` or `BackendUnavailable` so duration doesn't
    /// keep accumulating against a connection we know is gone.
    pub fn forget(&mut self) {
        self.identity = None;
        self.associated_since = None;
        self.samples.clear();
    }

    fn evict(&mut self, now: OffsetDateTime) {
        let cutoff = now - self.span;
        while let Some(front) = self.samples.front() {
            if front.at < cutoff {
                self.samples.pop_front();
            } else {
                break;
            }
        }
    }

    pub fn associated_duration(&self, now: OffsetDateTime) -> Option<Duration> {
        let since = self.associated_since?;
        let secs = (now - since).whole_seconds().max(0);
        Some(Duration::from_secs(secs as u64))
    }

    /// A stable per-association identifier suitable for embedding in a
    /// finding fingerprint. Prefers BSSID, falls back to SSID, then to
    /// a generic "current" token when both are redacted.
    pub fn identity_key(&self) -> Option<String> {
        let (ssid, bssid) = self.identity.as_ref()?;
        if let Some(b) = bssid {
            return Some(b.as_str().to_string());
        }
        if let Some(s) = ssid {
            return Some(s.as_str().to_string());
        }
        Some("current".to_string())
    }

    pub fn sample_count(&self) -> usize {
        self.samples.len()
    }

    /// Difference of mean RSSI between the recent half and the prior
    /// half of the lookback window. Positive = improving. Returns
    /// `None` if either half has fewer than 2 samples — we refuse to
    /// claim a trend from a single reading.
    pub fn rssi_delta(&self, lookback: Duration, now: OffsetDateTime) -> Option<f64> {
        let secs = lookback.as_secs();
        if secs == 0 {
            return None;
        }
        let recent_start = now - time::Duration::seconds(secs as i64 / 2);
        let prior_start = now - time::Duration::seconds(secs as i64);

        let mut recent_sum = 0i64;
        let mut recent_count = 0i64;
        let mut prior_sum = 0i64;
        let mut prior_count = 0i64;

        for s in &self.samples {
            if s.at >= recent_start {
                recent_sum += s.rssi_dbm as i64;
                recent_count += 1;
            } else if s.at >= prior_start {
                prior_sum += s.rssi_dbm as i64;
                prior_count += 1;
            }
        }

        if recent_count < 2 || prior_count < 2 {
            return None;
        }
        let recent_avg = recent_sum as f64 / recent_count as f64;
        let prior_avg = prior_sum as f64 / prior_count as f64;
        Some(recent_avg - prior_avg)
    }
}

// ============================================================================
// RF environment window
// ============================================================================

#[derive(Debug, Clone)]
pub struct RfEnvironmentSample {
    pub at: OffsetDateTime,
    pub ap_count: usize,
}

#[derive(Debug)]
pub struct RfEnvironmentWindow {
    span: Duration,
    samples: VecDeque<RfEnvironmentSample>,
}

impl RfEnvironmentWindow {
    pub fn new(span: Duration) -> Self {
        Self {
            span,
            samples: VecDeque::new(),
        }
    }

    pub fn record(&mut self, scan: &ScanResult, at: OffsetDateTime) {
        self.samples.push_back(RfEnvironmentSample {
            at,
            ap_count: scan.neighbors.len(),
        });
        self.evict(at);
    }

    fn evict(&mut self, now: OffsetDateTime) {
        let cutoff = now - self.span;
        while let Some(front) = self.samples.front() {
            if front.at < cutoff {
                self.samples.pop_front();
            } else {
                break;
            }
        }
    }

    pub fn sample_count(&self) -> usize {
        self.samples.len()
    }

    pub fn latest(&self) -> Option<&RfEnvironmentSample> {
        self.samples.back()
    }

    /// Difference in mean AP count between the recent half and prior
    /// half of the lookback window. Positive = density rising.
    pub fn density_delta(&self, lookback: Duration, now: OffsetDateTime) -> Option<f64> {
        let secs = lookback.as_secs();
        if secs == 0 {
            return None;
        }
        let recent_start = now - time::Duration::seconds(secs as i64 / 2);
        let prior_start = now - time::Duration::seconds(secs as i64);

        let mut recent_sum = 0usize;
        let mut recent_count = 0usize;
        let mut prior_sum = 0usize;
        let mut prior_count = 0usize;

        for s in &self.samples {
            if s.at >= recent_start {
                recent_sum += s.ap_count;
                recent_count += 1;
            } else if s.at >= prior_start {
                prior_sum += s.ap_count;
                prior_count += 1;
            }
        }

        // Scan cadence is ~10 s so 2 samples per half is the lowest count
        // we accept — anything less is too noisy to call a trend on.
        if recent_count < 2 || prior_count < 2 {
            return None;
        }
        let recent_avg = recent_sum as f64 / recent_count as f64;
        let prior_avg = prior_sum as f64 / prior_count as f64;
        Some(recent_avg - prior_avg)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use signalscope_events::{NeighborAp, ObservationConfidence};

    fn ts(offset_secs: i64) -> OffsetDateTime {
        OffsetDateTime::from_unix_timestamp(1_700_000_000 + offset_secs).unwrap()
    }

    fn wifi_obs(ssid: Option<&str>, bssid: Option<&str>, rssi: i32) -> WifiObservation {
        WifiObservation {
            interface: "en0".into(),
            ssid: ssid.map(Ssid::new),
            bssid: bssid.map(Bssid::new),
            rssi_dbm: Some(rssi),
            noise_dbm: Some(-95),
            tx_rate_mbps: None,
            channel: None,
            security: None,
            phy_mode: None,
            confidence: ObservationConfidence::Direct,
        }
    }

    #[test]
    fn signal_window_resets_on_association_change() {
        let mut w = WifiSignalWindow::new(Duration::from_secs(300));
        w.record(&wifi_obs(Some("HomeAP"), Some("aa:bb:cc:dd:ee:01"), -50), ts(0));
        w.record(&wifi_obs(Some("HomeAP"), Some("aa:bb:cc:dd:ee:01"), -52), ts(10));
        assert_eq!(w.sample_count(), 2);
        // Roam to a different BSSID — window resets.
        w.record(&wifi_obs(Some("HomeAP"), Some("aa:bb:cc:dd:ee:02"), -48), ts(20));
        assert_eq!(w.sample_count(), 1);
        assert_eq!(w.associated_duration(ts(30)), Some(Duration::from_secs(10)));
    }

    #[test]
    fn signal_delta_returns_none_until_enough_samples() {
        let mut w = WifiSignalWindow::new(Duration::from_secs(300));
        w.record(&wifi_obs(Some("HomeAP"), Some("aa:bb:cc:dd:ee:01"), -50), ts(0));
        // Only one sample → no delta.
        assert!(w.rssi_delta(Duration::from_secs(60), ts(10)).is_none());
    }

    #[test]
    fn signal_delta_detects_drift() {
        let mut w = WifiSignalWindow::new(Duration::from_secs(300));
        // 0-30s: -50, -50, -52, -50  (recent half)... wait — we want
        // recent and prior. Order the samples chronologically: prior half
        // -50s, recent half -55s.
        // Lookback = 60s, halved at 30s.
        for (offset, rssi) in [(-60, -50), (-50, -49), (-40, -50), (-20, -55), (-10, -56), (-5, -57)] {
            w.record(
                &wifi_obs(Some("HomeAP"), Some("aa:bb:cc:dd:ee:01"), rssi),
                ts(offset),
            );
        }
        let delta = w
            .rssi_delta(Duration::from_secs(60), ts(0))
            .expect("delta");
        // recent avg = (-55 + -56 + -57) / 3 = -56;  prior avg = -49.66...
        // → delta ≈ -6.33  (signal got *worse*)
        assert!(delta < -5.0, "expected degradation, got {delta}");
    }

    fn synthetic_scan(n: usize) -> ScanResult {
        ScanResult {
            interface: "en0".into(),
            neighbors: (0..n)
                .map(|_| NeighborAp {
                    bssid: None,
                    ssid: None,
                    rssi_dbm: None,
                    channel: None,
                    security: None,
                    phy_mode: None,
                    confidence: ObservationConfidence::Inferred,
                })
                .collect(),
        }
    }

    #[test]
    fn env_density_delta_detects_rise() {
        let mut w = RfEnvironmentWindow::new(Duration::from_secs(300));
        w.record(&synthetic_scan(5), ts(-60));
        w.record(&synthetic_scan(5), ts(-50));
        w.record(&synthetic_scan(12), ts(-20));
        w.record(&synthetic_scan(13), ts(-10));
        let delta = w.density_delta(Duration::from_secs(60), ts(0)).expect("delta");
        assert!(delta > 5.0, "expected density rise, got {delta}");
    }

    #[test]
    fn env_density_delta_returns_none_with_too_few_samples() {
        let mut w = RfEnvironmentWindow::new(Duration::from_secs(300));
        w.record(&synthetic_scan(5), ts(-10));
        assert!(w.density_delta(Duration::from_secs(60), ts(0)).is_none());
    }
}
