//! Wi-Fi sensor — single semantic surface over potentially multiple
//! per-platform acquisition backends.
//!
//! From the outside this is just a `Sensor` that emits `WifiObservation`,
//! `ScanResult`, and `SensorHealth` events on the bus. Which backend
//! produced any given observation is invisible to analysis and to the
//! TUI's data flow.

use std::sync::Arc;
use std::time::Duration;

use signalscope_core::EventBus;
use signalscope_events::{Event, SensorHealth, SensorId, SensorState};
use tokio::task::JoinHandle;
use tracing::{info, warn};

use crate::Sensor;

#[cfg(target_os = "macos")]
pub mod macos;

#[derive(Debug, Clone)]
pub struct WifiSensorConfig {
    /// Interface name to query (default `en0` on macOS, `wlan0` elsewhere).
    pub interface: String,
    /// How often to acquire a full Wi-Fi snapshot. On macOS this drives a
    /// `system_profiler` invocation, which is multi-second — keep this
    /// conservative.
    pub snapshot_interval: Duration,
}

impl Default for WifiSensorConfig {
    fn default() -> Self {
        Self {
            interface: default_interface(),
            snapshot_interval: Duration::from_secs(10),
        }
    }
}

#[cfg(target_os = "macos")]
fn default_interface() -> String {
    "en0".to_string()
}

#[cfg(not(target_os = "macos"))]
fn default_interface() -> String {
    "wlan0".to_string()
}

#[cfg(target_os = "macos")]
pub type WifiSensor = MacosWifiSensor;

#[cfg(target_os = "macos")]
#[derive(Debug)]
pub struct MacosWifiSensor {
    cfg: WifiSensorConfig,
}

#[cfg(target_os = "macos")]
impl MacosWifiSensor {
    pub fn new(cfg: WifiSensorConfig) -> Self {
        Self { cfg }
    }
}

#[cfg(target_os = "macos")]
impl Sensor for MacosWifiSensor {
    fn id(&self) -> SensorId {
        SensorId::new("wifi")
    }

    fn spawn(self, bus: Arc<EventBus>) -> JoinHandle<()> {
        let id = self.id();
        let cfg = self.cfg;
        tokio::spawn(async move { run_macos(id, cfg, bus).await })
    }
}

#[cfg(target_os = "macos")]
async fn run_macos(id: SensorId, cfg: WifiSensorConfig, bus: Arc<EventBus>) {
    use tokio::time::{interval, MissedTickBehavior};

    let backend = macos::detect_backend().await;
    let backend_name = backend.as_ref().map(|b| b.name().to_string());

    let mut tracker = HealthTracker::new(id.clone(), backend_name.clone());

    let Some(backend) = backend else {
        // No usable backend at all. Emit health *once* and park; the rest
        // of the dashboard still functions against gateway / DNS.
        publish_health(
            &bus,
            &mut tracker,
            SensorState::BackendUnavailable,
            Some("no Wi-Fi acquisition backend available on this host"),
        );
        std::future::pending::<()>().await;
        return;
    };

    info!(sensor = %id, backend = %backend.name(), "wifi sensor running");
    publish_health(&bus, &mut tracker, SensorState::Operational, None);

    let mut tick = interval(cfg.snapshot_interval);
    tick.set_missed_tick_behavior(MissedTickBehavior::Skip);

    loop {
        tick.tick().await;
        match backend.snapshot(&cfg.interface).await {
            Ok(snap) => {
                if let Some(link) = snap.link {
                    bus.publish(id.clone(), Event::Wifi(link));
                }
                if let Some(scan) = snap.scan {
                    bus.publish(id.clone(), Event::Scan(scan));
                }
                publish_health(&bus, &mut tracker, SensorState::Operational, None);
            }
            Err(macos::BackendError::HardwareDisabled) => {
                publish_health(
                    &bus,
                    &mut tracker,
                    SensorState::HardwareDisabled,
                    Some("Wi-Fi reported off"),
                );
            }
            Err(macos::BackendError::PermissionDenied(detail)) => {
                publish_health(
                    &bus,
                    &mut tracker,
                    SensorState::PermissionDenied,
                    Some(&detail),
                );
            }
            Err(macos::BackendError::Parse(detail)) => {
                warn!(sensor = %id, error = %detail, "wifi snapshot parse failed");
                publish_health(&bus, &mut tracker, SensorState::ParseFailed, Some(&detail));
            }
            Err(macos::BackendError::Timeout) => {
                warn!(sensor = %id, "wifi snapshot timed out");
                publish_health(&bus, &mut tracker, SensorState::Stale, Some("timeout"));
            }
            Err(e) => {
                warn!(sensor = %id, error = %e, "wifi snapshot failed");
                publish_health(&bus, &mut tracker, SensorState::Stale, Some(&e.to_string()));
            }
        }
    }
}

#[cfg(not(target_os = "macos"))]
pub type WifiSensor = NoOpWifiSensor;

#[cfg(not(target_os = "macos"))]
#[derive(Debug)]
pub struct NoOpWifiSensor {
    cfg: WifiSensorConfig,
}

#[cfg(not(target_os = "macos"))]
impl NoOpWifiSensor {
    pub fn new(cfg: WifiSensorConfig) -> Self {
        Self { cfg }
    }
}

#[cfg(not(target_os = "macos"))]
impl Sensor for NoOpWifiSensor {
    fn id(&self) -> SensorId {
        SensorId::new("wifi")
    }

    fn spawn(self, bus: Arc<EventBus>) -> JoinHandle<()> {
        let id = self.id();
        let _ = self.cfg;
        tokio::spawn(async move {
            let mut tracker = HealthTracker::new(id.clone(), None);
            publish_health(
                &bus,
                &mut tracker,
                SensorState::BackendUnavailable,
                Some("no Wi-Fi backend implemented for this platform yet"),
            );
            std::future::pending::<()>().await;
        })
    }
}

/// Tracks the last-emitted state so we only publish `SensorHealth` on
/// transitions. Without this the bus would carry one health event per
/// snapshot cycle, which is noisy and uninteresting.
#[derive(Debug)]
struct HealthTracker {
    id: SensorId,
    backend: Option<String>,
    last_state: Option<SensorState>,
}

impl HealthTracker {
    fn new(id: SensorId, backend: Option<String>) -> Self {
        Self {
            id,
            backend,
            last_state: None,
        }
    }
}

fn publish_health(
    bus: &Arc<EventBus>,
    tracker: &mut HealthTracker,
    state: SensorState,
    detail: Option<&str>,
) {
    if tracker.last_state == Some(state) {
        return;
    }
    tracker.last_state = Some(state);
    bus.publish(
        tracker.id.clone(),
        Event::SensorHealth(SensorHealth {
            sensor: tracker.id.clone(),
            state,
            backend: tracker.backend.clone(),
            detail: detail.map(|s| s.to_string()),
        }),
    );
}
