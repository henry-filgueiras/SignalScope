//! Headless capture mode.
//!
//! Spins up the same sensor + analysis pipeline as the observatory, but
//! without the ratatui front-end. Every envelope on the bus is mirrored to a
//! session file; the operator gets a one-line periodic status print so they
//! can tell the recording is healthy without watching a dashboard.
//!
//! Capture is intentionally minimalist:
//!
//! * No interactive UI.
//! * No replay controls.
//! * One file per session — same JSONL format the observatory's
//!   `--record` flag writes.
//!
//! The intended use is "capture now, analyze later": run on a flaky
//! laptop, ship the resulting `.signalscope-session` somewhere, replay
//! it offline.

use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use anyhow::Result;
use signalscope_analysis::AnalysisEngine;
use signalscope_core::{spawn_recorder, EventBus, SessionHeader, SessionWriter};
use signalscope_events::Event;
use signalscope_sensors::{
    dns::{DnsSensor, DnsSensorConfig},
    gateway::{GatewaySensor, GatewaySensorConfig},
    wifi::{WifiSensor, WifiSensorConfig},
    SensorScheduler,
};
use tokio::time::interval;
use tracing::info;

pub struct CaptureOptions {
    pub output: PathBuf,
    pub label: Option<String>,
}

pub async fn run(opts: CaptureOptions) -> Result<()> {
    let bus = EventBus::new();

    let header = SessionHeader::new(opts.label.clone());
    let created_at = header.created_at;
    let writer = SessionWriter::create(&opts.output, header)?;
    let recorder = spawn_recorder(bus.clone(), writer.clone());

    info!(
        path = %opts.output.display(),
        label = opts.label.as_deref().unwrap_or("-"),
        "session recording started"
    );
    eprintln!(
        "signalscope capture → {}\n  (Ctrl-C to stop)",
        opts.output.display()
    );

    let mut scheduler = SensorScheduler::new();
    scheduler.add(bus.clone(), WifiSensor::new(WifiSensorConfig::default()));
    scheduler.add(
        bus.clone(),
        GatewaySensor::new(GatewaySensorConfig::default()),
    );
    scheduler.add(bus.clone(), DnsSensor::new(DnsSensorConfig::default()));

    let analysis = AnalysisEngine::new(bus.clone()).spawn();

    let status = tokio::spawn(status_loop(bus.clone()));

    tokio::select! {
        _ = tokio::signal::ctrl_c() => {
            eprintln!();
            info!("ctrl-c received; stopping capture");
        }
    }

    analysis.abort();
    let _ = analysis.await;
    status.abort();
    let _ = status.await;
    recorder.abort();
    let _ = recorder.await;
    scheduler.shutdown().await;

    let elapsed = (time::OffsetDateTime::now_utc() - created_at)
        .whole_seconds()
        .max(0);
    eprintln!(
        "session closed after {h:02}:{m:02}:{s:02} → {path}",
        h = elapsed / 3600,
        m = (elapsed % 3600) / 60,
        s = elapsed % 60,
        path = opts.output.display(),
    );
    Ok(())
}

/// One-line periodic status to stderr so the operator can see the run is
/// healthy without a TUI.
async fn status_loop(bus: Arc<EventBus>) {
    let mut sub = bus.subscribe();
    let mut ticker = interval(Duration::from_secs(5));
    ticker.tick().await; // skip the immediate fire

    let mut tally = Tally::default();

    loop {
        tokio::select! {
            maybe_env = sub.recv() => {
                let Some(env) = maybe_env else { break };
                tally.observe(&env.event);
            }
            _ = ticker.tick() => {
                eprintln!(
                    "  wifi={} scan={} gw={} dns={} find={} health={}",
                    tally.wifi, tally.scan, tally.gateway, tally.dns,
                    tally.findings, tally.health,
                );
            }
        }
    }
}

#[derive(Default)]
struct Tally {
    wifi: u64,
    scan: u64,
    gateway: u64,
    dns: u64,
    findings: u64,
    health: u64,
}

impl Tally {
    fn observe(&mut self, event: &Event) {
        match event {
            Event::Wifi(_) => self.wifi += 1,
            Event::Scan(_) => self.scan += 1,
            Event::GatewayLatency(_) => self.gateway += 1,
            Event::DnsLatency(_) => self.dns += 1,
            Event::Finding(_) => self.findings += 1,
            Event::SensorHealth(_) => self.health += 1,
            Event::InterfaceStateChanged(_) | Event::RoamDetected(_) => {}
        }
    }
}
