//! Wi-Fi sensor: emits `WifiObservation` (current AP) and `ScanResult`
//! (neighbor list) on a fixed cadence.
//!
//! The cross-platform body lives here; the actual data acquisition lives in
//! one of the platform adapters under this module. Adapters return semantic
//! types — they never leak OS terminology upward.

use std::sync::Arc;
use std::time::Duration;

use signalscope_core::EventBus;
use signalscope_events::{Event, SensorId};
use tokio::task::JoinHandle;
use tracing::{debug, warn};

use crate::Sensor;

#[cfg(target_os = "macos")]
pub mod macos;

/// Platform-agnostic Wi-Fi sensor configuration.
#[derive(Debug, Clone)]
pub struct WifiSensorConfig {
    /// Interface name to query (default `en0` on macOS).
    pub interface: String,
    /// How often to refresh associated-link info.
    pub link_interval: Duration,
    /// How often to perform a neighbor scan (scans are expensive — keep this
    /// substantially larger than `link_interval`).
    pub scan_interval: Duration,
}

impl Default for WifiSensorConfig {
    fn default() -> Self {
        Self {
            interface: default_interface(),
            link_interval: Duration::from_secs(2),
            scan_interval: Duration::from_secs(15),
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

#[derive(Debug)]
pub struct WifiSensor {
    cfg: WifiSensorConfig,
}

impl WifiSensor {
    pub fn new(cfg: WifiSensorConfig) -> Self {
        Self { cfg }
    }
}

impl Sensor for WifiSensor {
    fn id(&self) -> SensorId {
        SensorId::new("wifi")
    }

    fn spawn(self, bus: Arc<EventBus>) -> JoinHandle<()> {
        let cfg = self.cfg;
        let id = self.id();
        tokio::spawn(async move {
            run(id, cfg, bus).await;
        })
    }
}

#[cfg(target_os = "macos")]
async fn run(id: SensorId, cfg: WifiSensorConfig, bus: Arc<EventBus>) {
    use tokio::time::{interval, MissedTickBehavior};

    let mut link_tick = interval(cfg.link_interval);
    link_tick.set_missed_tick_behavior(MissedTickBehavior::Skip);

    let mut scan_tick = interval(cfg.scan_interval);
    scan_tick.set_missed_tick_behavior(MissedTickBehavior::Skip);

    loop {
        tokio::select! {
            _ = link_tick.tick() => {
                match macos::current_link(&cfg.interface).await {
                    Ok(Some(obs)) => { bus.publish(id.clone(), Event::Wifi(obs)); }
                    Ok(None) => debug!("wifi link: not associated"),
                    Err(e) => warn!(error = %e, "wifi link query failed"),
                }
            }
            _ = scan_tick.tick() => {
                match macos::scan(&cfg.interface).await {
                    Ok(scan) => { bus.publish(id.clone(), Event::Scan(scan)); }
                    Err(e) => warn!(error = %e, "wifi scan failed"),
                }
            }
        }
    }
}

#[cfg(not(target_os = "macos"))]
async fn run(_id: SensorId, _cfg: WifiSensorConfig, _bus: Arc<EventBus>) {
    // TODO(linux): nl80211/netlink adapter. Bootstrap target is macOS-only,
    // so non-macOS builds get a no-op Wi-Fi sensor (the rest of the dashboard
    // still works against gateway/DNS sensors).
    warn!("wifi sensor: no adapter available on this platform — see docs/sensor-model.md");
    // Park the task so the scheduler still has a handle to abort.
    std::future::pending::<()>().await;
}
