//! Interface counters sensor.
//!
//! Periodically samples the host's per-interface byte / packet / error
//! counters and publishes one [`InterfaceCountersObservation`] per tick for
//! the *primary* interface (the one carrying the default route). Throughput,
//! loss rates, and any derivations are the analysis layer's job — this
//! sensor is a thin counter pump.
//!
//! ## Backend choice
//!
//! Uses the [`sysinfo`] crate as a portable safe wrapper around
//! `getifaddrs(3)` / `if_data` (macOS) and `/proc/net/dev` (Linux). That
//! satisfies the CLAUDE.md `#![forbid(unsafe_code)]` invariant while still
//! reading the native counter surfaces — no shelling out to `ifconfig` or
//! `netstat`.
//!
//! ## Primary interface
//!
//! We follow the default route, the same trick the gateway sensor uses, so
//! the counters track the path the operator's actually using. If the
//! default route disappears we emit a `Stale` health event and silently
//! stop publishing observations until it comes back — better than
//! confidently reporting numbers from a loopback or a sleeping interface.
//!
//! `retry_count` and `*_dropped` fields are left `None` from this backend
//! by design — they live behind richer integrations (Linux nl80211,
//! monitor mode) and the event model is already shaped to carry them.

use std::sync::Arc;
use std::time::Duration;

use signalscope_core::EventBus;
use signalscope_events::{
    Event, InterfaceCountersObservation, SensorHealth, SensorId, SensorState,
};
use sysinfo::Networks;
use tokio::process::Command;
use tokio::task::JoinHandle;
use tracing::{debug, warn};

use crate::Sensor;

const BACKEND: &str = "sysinfo";

#[derive(Debug, Clone)]
pub struct InterfaceSensorConfig {
    pub interval: Duration,
    /// How often to re-resolve the default-route interface. Counters are
    /// cheap; resolving requires a subprocess so we don't do it every tick.
    pub rediscover_every: Duration,
    /// Explicit interface override. When `None`, the sensor follows the
    /// default route.
    pub interface_override: Option<String>,
}

impl Default for InterfaceSensorConfig {
    fn default() -> Self {
        Self {
            interval: Duration::from_secs(2),
            rediscover_every: Duration::from_secs(30),
            interface_override: None,
        }
    }
}

#[derive(Debug)]
pub struct InterfaceSensor {
    cfg: InterfaceSensorConfig,
}

impl InterfaceSensor {
    pub fn new(cfg: InterfaceSensorConfig) -> Self {
        Self { cfg }
    }
}

impl Sensor for InterfaceSensor {
    fn id(&self) -> SensorId {
        SensorId::new("iface")
    }

    fn spawn(self, bus: Arc<EventBus>) -> JoinHandle<()> {
        let id = self.id();
        let cfg = self.cfg;
        tokio::spawn(async move { run(id, cfg, bus).await })
    }
}

async fn run(id: SensorId, cfg: InterfaceSensorConfig, bus: Arc<EventBus>) {
    use tokio::time::{interval, MissedTickBehavior};

    let mut tick = interval(cfg.interval);
    tick.set_missed_tick_behavior(MissedTickBehavior::Skip);

    let mut networks = Networks::new_with_refreshed_list();
    let mut last_health: Option<SensorState> = None;

    let mut primary: Option<String> = cfg.interface_override.clone();
    let mut last_rediscover = std::time::Instant::now()
        - cfg.rediscover_every
        - Duration::from_secs(1);

    loop {
        tick.tick().await;

        // Re-discover periodically so DHCP renewals / Wi-Fi/Ethernet
        // handoffs don't strand us on a stale interface.
        if cfg.interface_override.is_none()
            && last_rediscover.elapsed() >= cfg.rediscover_every
        {
            last_rediscover = std::time::Instant::now();
            match discover_primary_interface().await {
                Ok(Some(name)) => {
                    if primary.as_deref() != Some(name.as_str()) {
                        debug!(interface = %name, "primary interface changed");
                    }
                    primary = Some(name);
                }
                Ok(None) => {
                    primary = None;
                    publish_health(
                        &id,
                        &bus,
                        SensorState::Stale,
                        Some("no default route".into()),
                        &mut last_health,
                    );
                    continue;
                }
                Err(e) => {
                    warn!(error = %e, "primary interface discovery failed");
                }
            }
        }

        let Some(iface) = primary.clone() else {
            continue;
        };

        // Refresh counter snapshot. `refresh` keeps interface set stable;
        // we use `refresh_list` so newly-arrived interfaces (Wi-Fi up after
        // a sleep) are picked up without restarting the sensor.
        networks.refresh_list();
        networks.refresh();

        let Some(data) = networks.iter().find(|(n, _)| *n == &iface) else {
            publish_health(
                &id,
                &bus,
                SensorState::Stale,
                Some(format!("interface {iface} not visible to sysinfo")),
                &mut last_health,
            );
            continue;
        };
        let (_, net) = data;

        let obs = InterfaceCountersObservation {
            interface: iface.clone(),
            rx_bytes_total: net.total_received(),
            tx_bytes_total: net.total_transmitted(),
            rx_packets_total: net.total_packets_received(),
            tx_packets_total: net.total_packets_transmitted(),
            rx_errors_total: net.total_errors_on_received(),
            tx_errors_total: net.total_errors_on_transmitted(),
            // Userspace counter path doesn't surface these — see module
            // doc. Optional fields, left `None`.
            rx_dropped_total: None,
            tx_dropped_total: None,
            retry_count: None,
        };

        bus.publish(id.clone(), Event::InterfaceCounters(obs));
        publish_health(&id, &bus, SensorState::Operational, None, &mut last_health);
    }
}

fn publish_health(
    id: &SensorId,
    bus: &Arc<EventBus>,
    state: SensorState,
    detail: Option<String>,
    last: &mut Option<SensorState>,
) {
    if Some(state) == *last {
        return;
    }
    *last = Some(state);
    bus.publish(
        id.clone(),
        Event::SensorHealth(SensorHealth {
            sensor: id.clone(),
            state,
            backend: Some(BACKEND.into()),
            detail,
        }),
    );
}

/// Resolve the interface carrying the default route. macOS and Linux both
/// expose this through their userland routing CLIs; we accept the
/// subprocess cost because rediscovery runs every 30 s, not every tick.
async fn discover_primary_interface() -> anyhow::Result<Option<String>> {
    #[cfg(target_os = "macos")]
    {
        let output = Command::new("route")
            .args(["-n", "get", "default"])
            .output()
            .await?;
        if !output.status.success() {
            return Ok(None);
        }
        let text = String::from_utf8_lossy(&output.stdout);
        Ok(parse_macos_route_interface(&text))
    }

    #[cfg(target_os = "linux")]
    {
        let output = Command::new("ip")
            .args(["route", "show", "default"])
            .output()
            .await?;
        if !output.status.success() {
            return Ok(None);
        }
        let text = String::from_utf8_lossy(&output.stdout);
        Ok(parse_linux_route_interface(&text))
    }

    #[cfg(not(any(target_os = "macos", target_os = "linux")))]
    {
        Ok(None)
    }
}

#[cfg(any(target_os = "macos", test))]
fn parse_macos_route_interface(text: &str) -> Option<String> {
    for line in text.lines() {
        if let Some(rest) = line.trim().strip_prefix("interface:") {
            return Some(rest.trim().to_string());
        }
    }
    None
}

#[cfg(any(target_os = "linux", test))]
fn parse_linux_route_interface(text: &str) -> Option<String> {
    // "default via 192.168.1.1 dev wlan0 proto dhcp metric 600"
    for line in text.lines() {
        let mut toks = line.split_whitespace();
        let mut last_was_dev = false;
        while let Some(tok) = toks.next() {
            if last_was_dev {
                return Some(tok.to_string());
            }
            last_was_dev = tok == "dev";
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn macos_route_extracts_interface_field() {
        let sample = "\
   route to: default
destination: default
       mask: default
    gateway: 192.168.50.1
  interface: en0
      flags: <UP,GATEWAY,DONE,STATIC,PRCLONING,GLOBAL>
";
        assert_eq!(
            parse_macos_route_interface(sample),
            Some("en0".to_string())
        );
    }

    #[test]
    fn linux_route_extracts_dev_token() {
        let sample =
            "default via 192.168.1.1 dev wlan0 proto dhcp src 192.168.1.42 metric 600\n";
        assert_eq!(
            parse_linux_route_interface(sample),
            Some("wlan0".to_string())
        );
    }

    #[test]
    fn linux_route_handles_missing_default() {
        assert_eq!(parse_linux_route_interface(""), None);
    }
}
