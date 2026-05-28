//! Normalized, append-only event/observation model for SignalScope.
//!
//! The types in this crate are intentionally platform-agnostic. They describe
//! what was observed about the network environment in semantic terms — never
//! in CoreWLAN, nl80211, NetworkManager, or any other OS-specific vocabulary.
//!
//! Sensor adapters translate platform-native readings into these types.
//! Analysis consumes these types. The TUI renders these types. Nothing else.
//!
//! All payloads are constructed once and then read-only by convention; the
//! event bus stores them in an append-only buffer (see `signalscope-core`).

#![forbid(unsafe_code)]
#![warn(missing_debug_implementations)]

use std::time::Duration;

use serde::{Deserialize, Serialize};
use time::OffsetDateTime;

pub mod ids;
pub mod wifi;

pub use ids::{EventId, SensorId};
pub use wifi::{
    BandClass, Bssid, Channel, ChannelWidth, NeighborAp, ScanResult, Security, Ssid,
    WifiObservation,
};

/// Wall-clock timestamp for an event. We use wall time (not `Instant`) so
/// events remain meaningful after process restart and can be persisted /
/// replayed.
pub type Timestamp = OffsetDateTime;

/// Bounded confidence score in `0.0..=1.0` used by correlation findings.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct Confidence(f32);

impl Confidence {
    pub fn new(v: f32) -> Self {
        Self(v.clamp(0.0, 1.0))
    }
    pub fn value(self) -> f32 {
        self.0
    }
}

impl From<f32> for Confidence {
    fn from(v: f32) -> Self {
        Self::new(v)
    }
}

/// Latency sample with optional jitter/loss context.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GatewayLatencyObservation {
    pub target: String,
    pub rtt: Duration,
    /// True iff a reply was received within the probe budget.
    pub reachable: bool,
    /// Probe method used (e.g. `"icmp"`, `"tcp:53"`, `"udp:53"`).
    pub probe: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DnsLatencyObservation {
    pub resolver: String,
    pub query: String,
    pub rtt: Duration,
    pub answered: bool,
    pub error: Option<String>,
}

/// A network interface transitioned between states.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct InterfaceStateChanged {
    pub interface: String,
    pub previous: InterfaceState,
    pub current: InterfaceState,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum InterfaceState {
    Unknown,
    Down,
    Up,
    Associated,
    Disassociated,
}

/// A roam was detected: the associated AP changed BSSID while the SSID stayed
/// the same. This is a derived event emitted by analysis, not a sensor.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RoamDetected {
    pub ssid: Option<Ssid>,
    pub from_bssid: Bssid,
    pub to_bssid: Bssid,
    pub from_rssi_dbm: Option<i32>,
    pub to_rssi_dbm: Option<i32>,
}

/// A correlation finding — analysis's best interpretation of recent events.
/// Findings deliberately preserve ambiguity via `confidence` and `evidence`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CorrelationFinding {
    pub kind: FindingKind,
    pub headline: String,
    pub confidence: Confidence,
    pub evidence: Vec<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum FindingKind {
    RfCongestion,
    ApOverload,
    GatewayInstability,
    WanCongestion,
    DnsPathology,
    RoamingInstability,
    StickyClient,
}

/// The full set of payloads carried by the event bus.
///
/// New variants should describe semantic observations or derived findings —
/// not raw platform readings. Sensor adapters translate first.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type")]
pub enum Event {
    Wifi(WifiObservation),
    Scan(ScanResult),
    GatewayLatency(GatewayLatencyObservation),
    DnsLatency(DnsLatencyObservation),
    InterfaceStateChanged(InterfaceStateChanged),
    RoamDetected(RoamDetected),
    Finding(CorrelationFinding),
}

impl Event {
    pub fn category(&self) -> EventCategory {
        match self {
            Event::Wifi(_) | Event::Scan(_) => EventCategory::Wifi,
            Event::GatewayLatency(_) => EventCategory::Gateway,
            Event::DnsLatency(_) => EventCategory::Dns,
            Event::InterfaceStateChanged(_) => EventCategory::Interface,
            Event::RoamDetected(_) => EventCategory::Roam,
            Event::Finding(_) => EventCategory::Finding,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum EventCategory {
    Wifi,
    Gateway,
    Dns,
    Interface,
    Roam,
    Finding,
}

/// Envelope written to the event bus. Once published the envelope is treated
/// as immutable — consumers receive cheap `Arc<Envelope>` clones.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Envelope {
    pub id: EventId,
    pub at: Timestamp,
    pub source: SensorId,
    pub event: Event,
}

impl Envelope {
    pub fn new(id: EventId, source: SensorId, event: Event) -> Self {
        Self {
            id,
            at: OffsetDateTime::now_utc(),
            source,
            event,
        }
    }

    pub fn with_time(id: EventId, at: Timestamp, source: SensorId, event: Event) -> Self {
        Self {
            id,
            at,
            source,
            event,
        }
    }
}
