//! Platform-agnostic correlation engine.
//!
//! Two stages:
//!
//! 1. **Rules** (`rules.rs`) are stateless. They look at the current
//!    rolling state and produce a flat set of `CandidateFinding`s for
//!    whatever conditions are currently firing.
//! 2. **Lifecycle** (`lifecycle.rs`) is stateful. It compares the current
//!    candidate set against the previous one and emits
//!    `CorrelationFinding`s only on *transitions* —
//!    new conditions, material confidence changes, resolutions.
//!
//! That split is why the dashboard stays calm: rules can fire as often as
//! they like, but the bus only carries findings when something actually
//! changes operationally.

#![forbid(unsafe_code)]
#![warn(missing_debug_implementations)]

use std::sync::Arc;
use std::time::Duration;

use signalscope_core::EventBus;
use signalscope_events::{CorrelationFinding, Event, RoamDetected, SensorId, SensorState};
use time::OffsetDateTime;
use tokio::task::JoinHandle;
use tokio::time::MissedTickBehavior;
use tracing::debug;

mod lifecycle;
mod rules;
mod windows;

pub use lifecycle::LifecycleConfig;
use lifecycle::LifecycleTracker;
use windows::{
    DnsWindow, GatewayWindow, RfEnvironmentWindow, WifiSignalWindow, WifiState,
};

const SOURCE: &str = "analysis";

/// Window length used by the latency-oriented rules (gateway, DNS).
/// Kept short on purpose: the UI is real-time, and slow-moving rules feel
/// laggy.
const WINDOW: Duration = Duration::from_secs(30);

/// Window length used by the longitudinal connected-link / RF environment
/// trend windows. Long enough that a 60–120 s lookback inside the rules
/// has plenty of history to compare halves of.
const TREND_WINDOW: Duration = Duration::from_secs(300);

/// Cadence of the safety-net evaluation tick. Driven by a timer rather
/// than incoming events so resolutions still fire when all sensors are
/// quiet (e.g. Wi-Fi off, gateway unreachable).
const LIFECYCLE_TICK: Duration = Duration::from_secs(2);

#[derive(Debug)]
pub struct AnalysisEngine {
    bus: Arc<EventBus>,
    lifecycle_config: LifecycleConfig,
}

impl AnalysisEngine {
    pub fn new(bus: Arc<EventBus>) -> Self {
        Self {
            bus,
            lifecycle_config: LifecycleConfig::default(),
        }
    }

    pub fn with_lifecycle_config(mut self, config: LifecycleConfig) -> Self {
        self.lifecycle_config = config;
        self
    }

    pub fn spawn(self) -> JoinHandle<()> {
        tokio::spawn(async move { self.run().await })
    }

    async fn run(self) {
        let mut sub = self.bus.subscribe();
        let mut wifi = WifiState::default();
        let mut gateway = GatewayWindow::new(WINDOW);
        let mut dns = DnsWindow::new(WINDOW);
        let mut signal = WifiSignalWindow::new(TREND_WINDOW);
        let mut env_window = RfEnvironmentWindow::new(TREND_WINDOW);
        let mut tracker = LifecycleTracker::new(self.lifecycle_config.clone());

        let mut tick = tokio::time::interval(LIFECYCLE_TICK);
        tick.set_missed_tick_behavior(MissedTickBehavior::Skip);

        // Seed from backlog so analysis is immediately useful on startup.
        // Use each envelope's wall-clock `at` so the rolling windows hold
        // accurate temporal positions instead of compressing the whole
        // backlog to "now."
        for envelope in self.bus.recent() {
            ingest(
                &envelope.event,
                envelope.at,
                &mut wifi,
                &mut gateway,
                &mut dns,
                &mut signal,
                &mut env_window,
            );
        }

        loop {
            tokio::select! {
                maybe_env = sub.recv() => {
                    let Some(envelope) = maybe_env else {
                        debug!("event bus closed; analysis loop terminating");
                        break;
                    };
                    ingest(
                        &envelope.event,
                        envelope.at,
                        &mut wifi,
                        &mut gateway,
                        &mut dns,
                        &mut signal,
                        &mut env_window,
                    );

                    if let Some(roam) = wifi.take_pending_roam() {
                        self.publish_roam(roam);
                    }

                    if triggers_rule_evaluation(&envelope.event) {
                        self.evaluate(&wifi, &gateway, &dns, &signal, &env_window, &mut tracker);
                    }
                }
                _ = tick.tick() => {
                    // Periodic safety net: even with no incoming events,
                    // re-evaluate so resolutions and quiescent transitions
                    // get a chance to fire.
                    self.evaluate(&wifi, &gateway, &dns, &signal, &env_window, &mut tracker);
                }
            }
        }
    }

    fn evaluate(
        &self,
        wifi: &WifiState,
        gateway: &GatewayWindow,
        dns: &DnsWindow,
        signal: &WifiSignalWindow,
        env: &RfEnvironmentWindow,
        tracker: &mut LifecycleTracker,
    ) {
        let now = OffsetDateTime::now_utc();
        let candidates = rules::evaluate(wifi, gateway, dns, signal, env, now);
        for finding in tracker.step(candidates, now) {
            self.publish_finding(finding);
        }
    }

    fn publish_finding(&self, finding: CorrelationFinding) {
        self.bus
            .publish(SensorId::new(SOURCE), Event::Finding(finding));
    }

    fn publish_roam(&self, roam: RoamDetected) {
        self.bus
            .publish(SensorId::new(SOURCE), Event::RoamDetected(roam));
    }
}

fn triggers_rule_evaluation(event: &Event) -> bool {
    matches!(
        event,
        Event::GatewayLatency(_) | Event::DnsLatency(_) | Event::Scan(_) | Event::Wifi(_)
    )
}

fn ingest(
    event: &Event,
    at: OffsetDateTime,
    wifi: &mut WifiState,
    gateway: &mut GatewayWindow,
    dns: &mut DnsWindow,
    signal: &mut WifiSignalWindow,
    env: &mut RfEnvironmentWindow,
) {
    match event {
        Event::Wifi(obs) => {
            wifi.record_link(obs);
            signal.record(obs, at);
        }
        Event::Scan(scan) => {
            wifi.record_scan(scan);
            env.record(scan, at);
        }
        Event::GatewayLatency(o) => gateway.record(o),
        Event::DnsLatency(o) => dns.record(o),
        Event::SensorHealth(h) => {
            // Forget the connected-link trend when Wi-Fi acquisition stops
            // — otherwise associated_duration keeps ticking against a
            // connection we've lost visibility into.
            if h.sensor.as_str() == "wifi"
                && matches!(
                    h.state,
                    SensorState::HardwareDisabled
                        | SensorState::BackendUnavailable
                        | SensorState::PermissionDenied
                )
            {
                signal.forget();
            }
        }
        Event::InterfaceStateChanged(_)
        | Event::RoamDetected(_)
        | Event::Finding(_) => {}
    }
}
