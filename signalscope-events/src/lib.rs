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
    BandClass, Bssid, Channel, ChannelWidth, NeighborAp, ObservationConfidence, ScanResult,
    Security, Ssid, WifiObservation,
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
///
/// Findings carry both *judgement* (`confidence`, `evidence`) and
/// *lifecycle* (`fingerprint`, `lifecycle`, `first_seen`, `last_seen`,
/// `peak_confidence`). The lifecycle is what lets the UI behave like a
/// systems observatory rather than a printf loop — analysis emits exactly
/// when state *transitions*, not every time a rule re-fires.
///
/// Two findings with the same `fingerprint` refer to the same operational
/// condition over time, e.g. `"rf_congestion:ch11"` or
/// `"gateway_instability:192.168.1.1"`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CorrelationFinding {
    pub kind: FindingKind,
    pub fingerprint: String,
    pub headline: String,
    pub confidence: Confidence,
    /// Highest confidence observed during this active streak. Resets to
    /// the current value whenever a Resolved → Active edge happens.
    pub peak_confidence: Confidence,
    pub evidence: Vec<String>,
    pub lifecycle: FindingLifecycle,
    pub first_seen: Timestamp,
    pub last_seen: Timestamp,
}

impl CorrelationFinding {
    /// Time elapsed between the first and most recent positive observation
    /// of this finding fingerprint.
    pub fn active_duration(&self) -> std::time::Duration {
        let secs = (self.last_seen - self.first_seen).whole_seconds().max(0);
        std::time::Duration::from_secs(secs as u64)
    }
}

/// Lifecycle state of a finding. The bus only ever carries findings at
/// these transition points — quiescent re-evaluations are suppressed.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum FindingLifecycle {
    /// First emission of this fingerprint in the current streak.
    Active,
    /// Re-emitted because confidence rose materially.
    Escalating,
    /// Re-emitted because confidence fell materially but the condition is
    /// still active.
    Recovering,
    /// Condition no longer observed. The finding is being retired.
    Resolved,
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

/// A sensor reporting on its own operational state. Use this to communicate
/// degraded conditions (backend missing, permission denied, parse failure,
/// hardware disabled) without inventing synthetic observations.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SensorHealth {
    pub sensor: SensorId,
    pub state: SensorState,
    /// Human-readable backend identifier when relevant (e.g.
    /// `"system_profiler"`).
    pub backend: Option<String>,
    /// Free-form detail for the UI / logs. Keep short.
    pub detail: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum SensorState {
    /// Sensor is emitting fresh observations.
    Operational,
    /// No acquisition backend is available on this host.
    BackendUnavailable,
    /// The platform reports the underlying hardware as off / disabled.
    HardwareDisabled,
    /// Permission required to read telemetry was denied.
    PermissionDenied,
    /// Backend returned data that the parser could not interpret.
    ParseFailed,
    /// Backend timed out or temporarily failed; older data is now stale.
    Stale,
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
    SensorHealth(SensorHealth),
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
            Event::SensorHealth(_) => EventCategory::Health,
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
    Health,
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
