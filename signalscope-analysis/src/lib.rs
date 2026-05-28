//! Platform-agnostic correlation engine.
//!
//! The engine subscribes to the event bus, maintains a small amount of
//! rolling state per signal, runs a handful of lightweight rules over that
//! state, and publishes [`signalscope_events::CorrelationFinding`] events
//! back onto the bus. It never inspects platform APIs, never reads files,
//! and never reads sensors directly — its only input is normalized events.
//!
//! Rules deliberately preserve ambiguity: each finding carries a confidence
//! score and a short list of evidence strings so the UI can render the
//! reasoning, not just the conclusion.

#![forbid(unsafe_code)]
#![warn(missing_debug_implementations)]

use std::sync::Arc;
use std::time::Duration;

use signalscope_core::EventBus;
use signalscope_events::{CorrelationFinding, Event, RoamDetected, SensorId};
use tokio::task::JoinHandle;
use tracing::debug;

mod rules;
mod windows;

use windows::{DnsWindow, GatewayWindow, WifiState};

const SOURCE: &str = "analysis";

/// Window length used by the rules. Kept short on purpose: the UI is
/// real-time, and slow-moving rules feel laggy.
const WINDOW: Duration = Duration::from_secs(30);

#[derive(Debug)]
pub struct AnalysisEngine {
    bus: Arc<EventBus>,
}

impl AnalysisEngine {
    pub fn new(bus: Arc<EventBus>) -> Self {
        Self { bus }
    }

    pub fn spawn(self) -> JoinHandle<()> {
        tokio::spawn(async move { self.run().await })
    }

    async fn run(self) {
        let mut sub = self.bus.subscribe();
        let mut wifi = WifiState::default();
        let mut gateway = GatewayWindow::new(WINDOW);
        let mut dns = DnsWindow::new(WINDOW);

        // Seed from backlog so analysis is immediately useful on startup.
        for env in self.bus.recent() {
            ingest(&env.event, &mut wifi, &mut gateway, &mut dns);
        }

        while let Some(env) = sub.recv().await {
            ingest(&env.event, &mut wifi, &mut gateway, &mut dns);

            // Per-event derived signals: roams.
            if let Some(roam) = wifi.take_pending_roam() {
                self.publish_roam(roam);
            }

            // Periodic-ish: run rules on every gateway/DNS/scan tick. This is
            // cheap and avoids a separate scheduler.
            match &env.event {
                Event::GatewayLatency(_)
                | Event::DnsLatency(_)
                | Event::Scan(_) => {
                    for finding in rules::evaluate(&wifi, &gateway, &dns) {
                        self.publish_finding(finding);
                    }
                }
                _ => {}
            }
        }
        debug!("analysis loop terminated");
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

fn ingest(
    event: &Event,
    wifi: &mut WifiState,
    gateway: &mut GatewayWindow,
    dns: &mut DnsWindow,
) {
    match event {
        Event::Wifi(obs) => wifi.record_link(obs),
        Event::Scan(scan) => wifi.record_scan(scan),
        Event::GatewayLatency(o) => gateway.record(o),
        Event::DnsLatency(o) => dns.record(o),
        Event::InterfaceStateChanged(_)
        | Event::RoamDetected(_)
        | Event::Finding(_)
        | Event::SensorHealth(_) => {}
    }
}

