//! Headless capture mode.
//!
//! Spins up the same sensor + analysis pipeline as the observatory, but
//! without the ratatui front-end. Every envelope on the bus is mirrored
//! to a session file; the operator gets a one-line periodic status
//! print so they can tell the recording is healthy without watching a
//! dashboard.
//!
//! ## Exit conditions
//!
//! Capture exits when any of these happens, whichever comes first:
//!
//! 1. **Ctrl-C.** Always supported; clean shutdown.
//! 2. **Data window satisfied** (when `--window DURATION` is set).
//!    Every spawned sensor has produced at least two observations
//!    whose timestamps span ≥ DURATION, *or* has gone degraded
//!    (the degraded sensor's data won't show up no matter how
//!    long we wait). This is the operator's natural reading of
//!    "record 30 seconds" — they want 30 seconds of usable data
//!    per source, not 30 wall-clock seconds that might include
//!    a sensor's cold-start gap.
//! 3. **Hard wall-clock cap** (when `--max DURATION` is set).
//!    Belt-and-suspenders against a sensor that publishes neither
//!    observations nor health.

use std::collections::HashSet;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use anyhow::Result;
use signalscope_analysis::AnalysisEngine;
use signalscope_core::{spawn_recorder, EventBus, SessionHeader, SessionWriter};
use signalscope_events::{Envelope, Event, SensorId, SensorState};
use signalscope_sensors::{
    dns::{DnsSensor, DnsSensorConfig},
    gateway::{GatewaySensor, GatewaySensorConfig},
    iface::{InterfaceSensor, InterfaceSensorConfig},
    wifi::{WifiSensor, WifiSensorConfig},
    SensorScheduler,
};
use std::collections::HashMap;
use time::OffsetDateTime;
use tokio::time::interval;
use tracing::info;

pub struct CaptureOptions {
    pub output: PathBuf,
    pub label: Option<String>,
    /// Exit when every polled sensor has produced observations whose
    /// timestamps span at least this much. `None` means run until
    /// Ctrl-C (or `max_duration`).
    pub window: Option<Duration>,
    /// Hard wall-clock ceiling. Always honored if set. `None` means
    /// "no ceiling — Ctrl-C or the window decide."
    pub max_duration: Option<Duration>,
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
        window_seconds = opts.window.map(|d| d.as_secs()),
        max_seconds = opts.max_duration.map(|d| d.as_secs()),
        "session recording started"
    );
    eprintln!(
        "signalscope capture → {}\n  {}",
        opts.output.display(),
        format_exit_hint(&opts),
    );

    let mut scheduler = SensorScheduler::new();
    scheduler.add(bus.clone(), WifiSensor::new(WifiSensorConfig::default()));
    scheduler.add(
        bus.clone(),
        GatewaySensor::new(GatewaySensorConfig::default()),
    );
    scheduler.add(bus.clone(), DnsSensor::new(DnsSensorConfig::default()));
    scheduler.add(
        bus.clone(),
        InterfaceSensor::new(InterfaceSensorConfig::default()),
    );

    let expected_sensors: HashSet<SensorId> = scheduler.ids().cloned().collect();

    let analysis = AnalysisEngine::new(bus.clone()).spawn();

    let status = tokio::spawn(status_loop(bus.clone()));

    let reason = wait_for_exit(&bus, &opts, expected_sensors).await;
    eprintln!();
    info!(reason = ?reason, "capture exiting");

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
        "session closed after {h:02}:{m:02}:{s:02} via {reason:?} → {path}",
        h = elapsed / 3600,
        m = (elapsed % 3600) / 60,
        s = elapsed % 60,
        reason = reason,
        path = opts.output.display(),
    );
    Ok(())
}

fn format_exit_hint(opts: &CaptureOptions) -> String {
    match (opts.window, opts.max_duration) {
        (Some(w), Some(m)) => format!(
            "(exits after {} s of data per sensor, hard cap {} s, Ctrl-C anytime)",
            w.as_secs(),
            m.as_secs()
        ),
        (Some(w), None) => format!(
            "(exits after {} s of data per sensor, Ctrl-C anytime)",
            w.as_secs()
        ),
        (None, Some(m)) => format!("(hard cap {} s, Ctrl-C anytime)", m.as_secs()),
        (None, None) => "(Ctrl-C to stop)".to_string(),
    }
}

#[derive(Debug)]
enum ExitReason {
    CtrlC,
    WindowSatisfied,
    MaxDurationReached,
}

async fn wait_for_exit(
    bus: &Arc<EventBus>,
    opts: &CaptureOptions,
    expected_sensors: HashSet<SensorId>,
) -> ExitReason {
    // No window and no cap → classic Ctrl-C only.
    if opts.window.is_none() && opts.max_duration.is_none() {
        let _ = tokio::signal::ctrl_c().await;
        return ExitReason::CtrlC;
    }

    let mut tracker = WindowTracker::new(expected_sensors);
    let mut sub = bus.subscribe();

    let max_sleep = opts
        .max_duration
        .map(tokio::time::sleep)
        .map(|f| Box::pin(f) as std::pin::Pin<Box<dyn std::future::Future<Output = ()> + Send>>);
    tokio::pin!(max_sleep);

    loop {
        tokio::select! {
            biased;
            _ = tokio::signal::ctrl_c() => {
                return ExitReason::CtrlC;
            }
            maybe_env = sub.recv() => {
                let Some(env) = maybe_env else {
                    // Bus dropped — shouldn't happen during normal capture
                    // but safer to bail than spin.
                    return ExitReason::CtrlC;
                };
                tracker.observe(&env);
                if let Some(window) = opts.window {
                    if tracker.satisfied(window) {
                        return ExitReason::WindowSatisfied;
                    }
                }
            }
            _ = async {
                match max_sleep.as_mut().as_pin_mut() {
                    Some(f) => f.await,
                    None => std::future::pending::<()>().await,
                }
            } => {
                return ExitReason::MaxDurationReached;
            }
        }
    }
}

/// Per-sensor "have we seen enough?" state. `first_at` and `last_at`
/// track the time range of *data* observations (anything that isn't
/// `SensorHealth` — health events are status, not data). A sensor
/// whose latest health is non-`Operational` is marked degraded and
/// considered satisfied so we don't hang forever waiting for a Wi-Fi
/// adapter that's turned off.
#[derive(Default, Debug, Clone)]
struct SensorWindow {
    first_at: Option<OffsetDateTime>,
    last_at: Option<OffsetDateTime>,
    degraded: bool,
}

impl SensorWindow {
    fn observe(&mut self, at: OffsetDateTime, event: &Event) {
        match event {
            Event::SensorHealth(h) => {
                self.degraded = h.state != SensorState::Operational;
            }
            _ => {
                if self.first_at.is_none() {
                    self.first_at = Some(at);
                }
                self.last_at = Some(at);
            }
        }
    }

    fn span_secs(&self) -> Option<u64> {
        let first = self.first_at?;
        let last = self.last_at?;
        let secs = (last - first).whole_seconds().max(0);
        Some(secs as u64)
    }

    fn satisfied(&self, window: Duration) -> bool {
        if self.degraded {
            return true;
        }
        self.span_secs().map_or(false, |s| s >= window.as_secs())
    }
}

/// Aggregates `SensorWindow` state across all the sensors we're
/// waiting on. Envelopes from unknown sources (e.g. `analysis`) are
/// ignored — we only track the polling sources the scheduler spawned.
#[derive(Debug)]
struct WindowTracker {
    per_sensor: HashMap<SensorId, SensorWindow>,
}

impl WindowTracker {
    fn new(expected: HashSet<SensorId>) -> Self {
        Self {
            per_sensor: expected
                .into_iter()
                .map(|id| (id, SensorWindow::default()))
                .collect(),
        }
    }

    fn observe(&mut self, env: &Envelope) {
        if let Some(s) = self.per_sensor.get_mut(&env.source) {
            s.observe(env.at, &env.event);
        }
    }

    fn satisfied(&self, window: Duration) -> bool {
        self.per_sensor.values().all(|s| s.satisfied(window))
    }
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
                    "  wifi={} scan={} gw={} dns={} iface={} find={} health={}",
                    tally.wifi, tally.scan, tally.gateway, tally.dns,
                    tally.iface, tally.findings, tally.health,
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
    iface: u64,
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
            Event::InterfaceCounters(_) => self.iface += 1,
            Event::Finding(_) => self.findings += 1,
            Event::SensorHealth(_) => self.health += 1,
            Event::InterfaceStateChanged(_) | Event::RoamDetected(_) => {}
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use signalscope_events::{
        EventId, GatewayLatencyObservation, SensorHealth,
    };

    fn ts(off: i64) -> OffsetDateTime {
        OffsetDateTime::from_unix_timestamp(1_700_000_000 + off).unwrap()
    }

    fn env(source: &str, at: OffsetDateTime, event: Event) -> Envelope {
        Envelope::with_time(EventId(0), at, SensorId::new(source), event)
    }

    fn gw(rtt_ms: u64) -> Event {
        Event::GatewayLatency(GatewayLatencyObservation {
            target: "192.168.1.1".into(),
            rtt: Duration::from_millis(rtt_ms),
            reachable: true,
            probe: "icmp".into(),
        })
    }

    fn health(state: SensorState) -> Event {
        Event::SensorHealth(SensorHealth {
            sensor: SensorId::new("wifi"),
            state,
            backend: None,
            detail: None,
        })
    }

    fn tracker_of(sensors: &[&str]) -> WindowTracker {
        WindowTracker::new(sensors.iter().map(|s| SensorId::new(*s)).collect())
    }

    #[test]
    fn empty_tracker_is_never_satisfied() {
        let mut t = tracker_of(&["wifi", "gateway"]);
        assert!(!t.satisfied(Duration::from_secs(30)));
        // One observation isn't enough — span is zero.
        t.observe(&env("wifi", ts(0), gw(5)));
        assert!(!t.satisfied(Duration::from_secs(30)));
    }

    #[test]
    fn satisfied_when_every_sensor_spans_the_window() {
        let mut t = tracker_of(&["wifi", "gateway"]);
        t.observe(&env("wifi", ts(0), gw(5)));
        t.observe(&env("wifi", ts(30), gw(5)));
        t.observe(&env("gateway", ts(0), gw(5)));
        // gateway only has one point so not satisfied yet
        assert!(!t.satisfied(Duration::from_secs(30)));
        t.observe(&env("gateway", ts(30), gw(5)));
        assert!(t.satisfied(Duration::from_secs(30)));
    }

    #[test]
    fn slowest_sensor_dominates_the_wait() {
        let mut t = tracker_of(&["wifi", "gateway"]);
        // Gateway spans 30 s quickly, but Wi-Fi only spans 20 s so far.
        t.observe(&env("gateway", ts(0), gw(5)));
        t.observe(&env("gateway", ts(30), gw(5)));
        t.observe(&env("wifi", ts(10), gw(5)));
        t.observe(&env("wifi", ts(30), gw(5)));
        assert!(!t.satisfied(Duration::from_secs(30)));
        // Once Wi-Fi spans 30 s, we're done.
        t.observe(&env("wifi", ts(40), gw(5)));
        assert!(t.satisfied(Duration::from_secs(30)));
    }

    #[test]
    fn degraded_sensor_short_circuits_to_satisfied() {
        let mut t = tracker_of(&["wifi", "gateway"]);
        t.observe(&env("wifi", ts(0), health(SensorState::HardwareDisabled)));
        // wifi never produced data, but it's degraded → treated as satisfied
        t.observe(&env("gateway", ts(0), gw(5)));
        t.observe(&env("gateway", ts(30), gw(5)));
        assert!(t.satisfied(Duration::from_secs(30)));
    }

    #[test]
    fn recovering_to_operational_re_requires_data_window() {
        // A sensor that flapped: degraded, then Operational again. Once
        // it's back to Operational the degraded short-circuit lifts and
        // we need real data spanning the window.
        let mut t = tracker_of(&["wifi"]);
        t.observe(&env("wifi", ts(0), health(SensorState::Stale)));
        assert!(t.satisfied(Duration::from_secs(30)), "degraded => satisfied");
        t.observe(&env("wifi", ts(5), health(SensorState::Operational)));
        // Now we need actual data spanning the window.
        assert!(!t.satisfied(Duration::from_secs(30)));
        t.observe(&env("wifi", ts(10), gw(5)));
        t.observe(&env("wifi", ts(45), gw(5)));
        assert!(t.satisfied(Duration::from_secs(30)));
    }

    #[test]
    fn observations_from_unknown_sources_are_ignored() {
        // The analysis crate publishes Findings under source "analysis";
        // we don't track that — only the sensors the scheduler spawned.
        let mut t = tracker_of(&["wifi"]);
        t.observe(&env("analysis", ts(0), gw(5)));
        t.observe(&env("analysis", ts(60), gw(5)));
        assert!(!t.satisfied(Duration::from_secs(30)), "analysis events shouldn't satisfy wifi");
    }
}
