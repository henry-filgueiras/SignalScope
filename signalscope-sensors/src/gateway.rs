//! Gateway latency sensor.
//!
//! Discovers the default gateway and probes its reachability/RTT on a fixed
//! cadence. The current adapter shells out to `ping(8)` for portability;
//! moving to a raw-socket or `socket2` ICMP path is intentional future work
//! once we want to avoid the subprocess overhead.

use std::sync::Arc;
use std::time::Duration;

use signalscope_core::EventBus;
use signalscope_events::{Event, GatewayLatencyObservation, SensorId};
use tokio::process::Command;
use tokio::task::JoinHandle;
use tracing::{debug, warn};

use crate::Sensor;

#[derive(Debug, Clone)]
pub struct GatewaySensorConfig {
    pub interval: Duration,
    pub probe_timeout: Duration,
    /// Optional override; when `None` we discover the default gateway.
    pub target_override: Option<String>,
}

impl Default for GatewaySensorConfig {
    fn default() -> Self {
        Self {
            interval: Duration::from_secs(1),
            probe_timeout: Duration::from_millis(900),
            target_override: None,
        }
    }
}

#[derive(Debug)]
pub struct GatewaySensor {
    cfg: GatewaySensorConfig,
}

impl GatewaySensor {
    pub fn new(cfg: GatewaySensorConfig) -> Self {
        Self { cfg }
    }
}

impl Sensor for GatewaySensor {
    fn id(&self) -> SensorId {
        SensorId::new("gateway")
    }

    fn spawn(self, bus: Arc<EventBus>) -> JoinHandle<()> {
        let id = self.id();
        let cfg = self.cfg;
        tokio::spawn(async move { run(id, cfg, bus).await })
    }
}

async fn run(id: SensorId, cfg: GatewaySensorConfig, bus: Arc<EventBus>) {
    use tokio::time::{interval, MissedTickBehavior};

    let mut tick = interval(cfg.interval);
    tick.set_missed_tick_behavior(MissedTickBehavior::Skip);

    // Re-resolve the gateway every N ticks so we handle DHCP renewals and
    // network changes without restarting.
    let mut cached_target: Option<String> = cfg.target_override.clone();
    let mut ticks_since_resolve = 0u32;

    loop {
        tick.tick().await;

        if cached_target.is_none() || ticks_since_resolve > 30 {
            ticks_since_resolve = 0;
            match discover_default_gateway().await {
                Ok(Some(t)) => cached_target = Some(t),
                Ok(None) => debug!("no default gateway"),
                Err(e) => warn!(error = %e, "gateway discovery failed"),
            }
        }
        ticks_since_resolve += 1;

        let Some(target) = cached_target.clone() else {
            continue;
        };

        match probe(&target, cfg.probe_timeout).await {
            Ok(obs) => {
                bus.publish(id.clone(), Event::GatewayLatency(obs));
            }
            Err(e) => debug!(error = %e, "gateway probe failed"),
        }
    }
}

/// Discover the default gateway IP. macOS implementation parses
/// `route -n get default`. Linux implementation parses `ip route show
/// default`. Returns `None` when no default route is configured.
async fn discover_default_gateway() -> anyhow::Result<Option<String>> {
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
        for line in text.lines() {
            let line = line.trim();
            if let Some(rest) = line.strip_prefix("gateway:") {
                return Ok(Some(rest.trim().to_string()));
            }
        }
        Ok(None)
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
        // Example: "default via 192.168.1.1 dev wlan0 ..."
        for line in text.lines() {
            let mut toks = line.split_whitespace();
            if toks.next() == Some("default") && toks.next() == Some("via") {
                if let Some(ip) = toks.next() {
                    return Ok(Some(ip.to_string()));
                }
            }
        }
        Ok(None)
    }

    #[cfg(not(any(target_os = "macos", target_os = "linux")))]
    {
        Ok(None)
    }
}

async fn probe(target: &str, timeout: Duration) -> anyhow::Result<GatewayLatencyObservation> {
    // `ping -c 1` sends one echo. macOS `-W` is wait-time in ms; Linux `-W`
    // is in seconds. We use `-c 1` and trust the OS-side timeout to be
    // comparable, but clamp the subprocess with a tokio timeout as a backstop.
    #[cfg(target_os = "macos")]
    let args: Vec<String> = vec![
        "-c".into(),
        "1".into(),
        "-W".into(),
        format!("{}", timeout.as_millis()),
        target.into(),
    ];

    #[cfg(target_os = "linux")]
    let args: Vec<String> = vec![
        "-c".into(),
        "1".into(),
        "-W".into(),
        format!("{}", timeout.as_secs().max(1)),
        target.into(),
    ];

    #[cfg(not(any(target_os = "macos", target_os = "linux")))]
    let args: Vec<String> = vec!["-c".into(), "1".into(), target.into()];

    let fut = Command::new("ping").args(&args).output();
    let output = tokio::time::timeout(timeout + Duration::from_millis(500), fut).await??;
    let text = String::from_utf8_lossy(&output.stdout);

    let rtt = parse_ping_rtt(&text);
    let reachable = output.status.success() && rtt.is_some();
    let rtt = rtt.unwrap_or(timeout);

    Ok(GatewayLatencyObservation {
        target: target.to_string(),
        rtt,
        reachable,
        probe: "icmp".into(),
    })
}

/// Extract `time=X ms` from a `ping` output. Returns `None` when no reply
/// was reported.
fn parse_ping_rtt(text: &str) -> Option<Duration> {
    for token in text.split_whitespace() {
        if let Some(rest) = token.strip_prefix("time=") {
            if let Ok(ms) = rest.parse::<f64>() {
                return Some(Duration::from_secs_f64(ms / 1000.0));
            }
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_ping_rtt_macos_format() {
        let sample = "64 bytes from 192.168.1.1: icmp_seq=0 ttl=64 time=1.234 ms";
        assert!(parse_ping_rtt(sample).unwrap() < Duration::from_millis(2));
    }

    #[test]
    fn missing_reply_returns_none() {
        let sample = "Request timeout for icmp_seq 0\n";
        assert!(parse_ping_rtt(sample).is_none());
    }
}
