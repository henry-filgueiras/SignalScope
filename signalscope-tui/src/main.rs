//! `signalscope` — terminal observability for local network quality.

#![forbid(unsafe_code)]

use std::path::PathBuf;

use anyhow::Result;
use signalscope_analysis::AnalysisEngine;
use signalscope_core::{spawn_recorder, EventBus, SessionHeader, SessionWriter};
use signalscope_sensors::{
    dns::{DnsSensor, DnsSensorConfig},
    gateway::{GatewaySensor, GatewaySensorConfig},
    iface::{InterfaceSensor, InterfaceSensorConfig},
    wifi::{WifiSensor, WifiSensorConfig},
    SensorScheduler,
};
use tokio::task::JoinHandle;
use tracing::info;

mod app;
mod capture;
mod inspect;
mod theme;
mod ui;

#[tokio::main(flavor = "multi_thread", worker_threads = 4)]
async fn main() -> Result<()> {
    let cmd = match parse_args() {
        Ok(c) => c,
        Err(msg) => {
            eprintln!("{msg}");
            eprintln!();
            print_usage();
            std::process::exit(2);
        }
    };

    match cmd {
        Command::Help => {
            print_usage();
            Ok(())
        }
        Command::Observe(opts) => {
            init_logging_file();
            info!("signalscope observe starting");
            run_observe(opts).await
        }
        Command::Capture(opts) => {
            init_logging_file();
            info!("signalscope capture starting");
            capture::run(opts).await
        }
        Command::Inspect(opts) => {
            // No log file — inspect is a one-shot, runs in any terminal.
            signalscope_core::logging::init_stderr();
            inspect::run(opts).await
        }
    }
}

async fn run_observe(opts: ObserveOptions) -> Result<()> {
    let bus = EventBus::new();

    // Optional session recording in observe mode — same JSONL format as
    // capture mode, so the operator can promote a live observation into a
    // permanent artifact without restarting.
    let recorder: Option<JoinHandle<()>> = if let Some(path) = opts.record.as_ref() {
        let header = SessionHeader::new(opts.label.clone());
        let writer = SessionWriter::create(path, header)?;
        info!(path = %path.display(), "session recording started");
        Some(spawn_recorder(bus.clone(), writer))
    } else {
        None
    };

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

    let analysis = AnalysisEngine::new(bus.clone()).spawn();

    let outcome = app::run(bus.clone()).await;

    analysis.abort();
    let _ = analysis.await;
    if let Some(rec) = recorder {
        rec.abort();
        let _ = rec.await;
    }
    scheduler.shutdown().await;

    outcome
}

enum Command {
    Help,
    Observe(ObserveOptions),
    Capture(capture::CaptureOptions),
    Inspect(inspect::InspectOptions),
}

#[derive(Default)]
struct ObserveOptions {
    record: Option<PathBuf>,
    label: Option<String>,
}

fn parse_args() -> Result<Command, String> {
    let mut args = std::env::args().skip(1);
    let head = args.next();
    match head.as_deref() {
        None => Ok(Command::Observe(ObserveOptions::default())),
        Some("-h") | Some("--help") | Some("help") => Ok(Command::Help),
        Some("observe") => Ok(Command::Observe(parse_observe(&mut args)?)),
        Some("capture") => Ok(Command::Capture(parse_capture(&mut args)?)),
        Some("inspect") => Ok(Command::Inspect(parse_inspect(&mut args)?)),
        // Backward compat: bare flags after the program name behave like
        // `observe <flags>` so existing invocations keep working.
        Some(s) if s.starts_with('-') => {
            let rest = std::iter::once(s.to_string()).chain(args.by_ref());
            let mut peekable = rest.into_iter();
            Ok(Command::Observe(parse_observe(&mut peekable)?))
        }
        Some(other) => Err(format!("unknown subcommand: {other}")),
    }
}

fn parse_observe<I: Iterator<Item = String>>(args: &mut I) -> Result<ObserveOptions, String> {
    let mut opts = ObserveOptions::default();
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--record" => {
                opts.record = Some(PathBuf::from(
                    args.next().ok_or("--record requires a path")?,
                ));
            }
            "--label" => {
                opts.label = Some(args.next().ok_or("--label requires a value")?);
            }
            "-h" | "--help" => return Err("usage: signalscope observe [--record PATH] [--label TEXT]".into()),
            other => return Err(format!("unknown observe option: {other}")),
        }
    }
    if opts.label.is_some() && opts.record.is_none() {
        return Err("--label requires --record".into());
    }
    Ok(opts)
}

fn parse_inspect<I: Iterator<Item = String>>(args: &mut I) -> Result<inspect::InspectOptions, String> {
    let mut path: Option<PathBuf> = None;
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "-h" | "--help" => return Err("usage: signalscope inspect PATH".into()),
            other if other.starts_with('-') => {
                return Err(format!("unknown inspect option: {other}"))
            }
            other => {
                if path.is_some() {
                    return Err(format!("unexpected extra argument: {other}"));
                }
                path = Some(PathBuf::from(other));
            }
        }
    }
    let path = path.ok_or_else(|| "inspect requires a PATH argument".to_string())?;
    Ok(inspect::InspectOptions { path })
}

fn parse_capture<I: Iterator<Item = String>>(args: &mut I) -> Result<capture::CaptureOptions, String> {
    let mut output: Option<PathBuf> = None;
    let mut label: Option<String> = None;
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "-o" | "--output" => {
                output = Some(PathBuf::from(
                    args.next().ok_or("--output requires a path")?,
                ));
            }
            "--label" => {
                label = Some(args.next().ok_or("--label requires a value")?);
            }
            "-h" | "--help" => {
                return Err("usage: signalscope capture --output PATH [--label TEXT]".into())
            }
            other => return Err(format!("unknown capture option: {other}")),
        }
    }
    let output = output.ok_or_else(|| "capture requires --output PATH".to_string())?;
    Ok(capture::CaptureOptions { output, label })
}

fn print_usage() {
    eprintln!(
        "signalscope — terminal observability for local network quality\n\
         \n\
         USAGE:\n  \
             signalscope [observe] [--record PATH] [--label TEXT]\n  \
             signalscope capture --output PATH [--label TEXT]\n  \
             signalscope inspect PATH\n  \
             signalscope help\n\
         \n\
         observe    Run the live TUI dashboard. Default subcommand.\n           \
                    --record  also writes every event to PATH as a JSONL session.\n  \
         capture    Headless recording — sensors run, events stream to PATH,\n           \
                    periodic stderr status. No TUI. Ctrl-C to stop.\n  \
         inspect    Print a one-screen summary of a recorded session — kind,\n           \
                    format version, span, per-category event tally. Verifies\n           \
                    a handed-off `.signalscope-session` file end-to-end.\n\
         \n\
         The session file (`.signalscope-session`) is an append-only newline-\n\
         delimited JSON stream: first line is a version header, every later\n\
         line is one envelope as published by the bus. Timestamps are RFC 3339\n\
         strings; inspect with `jq -r '.at'` on any modern shell."
    );
}

fn init_logging_file() {
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

