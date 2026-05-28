//! `signalscope` — terminal observability for local network quality.

#![forbid(unsafe_code)]

use anyhow::Result;
use signalscope_analysis::AnalysisEngine;
use signalscope_core::EventBus;
use signalscope_sensors::{
    dns::{DnsSensor, DnsSensorConfig},
    gateway::{GatewaySensor, GatewaySensorConfig},
    wifi::{WifiSensor, WifiSensorConfig},
    SensorScheduler,
};
use tracing::info;

mod app;
mod theme;
mod ui;

#[tokio::main(flavor = "multi_thread", worker_threads = 4)]
async fn main() -> Result<()> {
    init_logging();

    info!("signalscope starting");

    let bus = EventBus::new();

    let mut scheduler = SensorScheduler::new();
    scheduler.add(bus.clone(), WifiSensor::new(WifiSensorConfig::default()));
    scheduler.add(
        bus.clone(),
        GatewaySensor::new(GatewaySensorConfig::default()),
    );
    scheduler.add(bus.clone(), DnsSensor::new(DnsSensorConfig::default()));

    let analysis = AnalysisEngine::new(bus.clone()).spawn();

    let outcome = app::run(bus.clone()).await;

    analysis.abort();
    let _ = analysis.await;
    scheduler.shutdown().await;

    outcome
}

fn init_logging() {
    // The TUI owns the terminal, so we send logs to a file. Honor
    // `SIGNALSCOPE_LOG_DIR` for the destination directory, falling back to
    // `./logs`. The non-blocking guard must live for the program duration;
    // we leak it intentionally because the process owns it.
    let dir = std::env::var("SIGNALSCOPE_LOG_DIR").unwrap_or_else(|_| "logs".into());
    if let Err(e) = std::fs::create_dir_all(&dir) {
        eprintln!("warning: could not create log directory {dir}: {e}");
        signalscope_core::logging::init_stderr();
        return;
    }
    let appender = tracing_appender::rolling::daily(&dir, "signalscope.log");
    let (writer, guard) = tracing_appender::non_blocking(appender);
    // Intentional leak — process lifetime ownership.
    Box::leak(Box::new(guard));
    signalscope_core::logging::init_with_writer(writer);
}

