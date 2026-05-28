//! Rolling state windows used by the correlation rules.

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
}

impl GatewayWindow {
    pub fn new(span: Duration) -> Self {
        Self {
            span,
            samples: VecDeque::new(),
        }
    }

    pub fn record(&mut self, obs: &GatewayLatencyObservation) {
        let now = OffsetDateTime::now_utc();
        self.samples.push_back(GatewaySample {
            at: now,
            rtt: obs.rtt,
            reachable: obs.reachable,
        });
        self.evict(now);
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

    pub fn samples(&self) -> impl Iterator<Item = &GatewaySample> {
        self.samples.iter()
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

    pub fn samples(&self) -> impl Iterator<Item = &DnsSample> {
        self.samples.iter()
    }

    pub fn len(&self) -> usize {
        self.samples.len()
    }
}
