//! Structured logging setup.
//!
//! The TUI takes over the terminal, so logs must go to a file (or be muted).
//! This module sets up a `tracing` subscriber that respects `SIGNALSCOPE_LOG`
//! (defaulting to `info`) and writes either to stderr (non-TUI tooling) or to
//! a caller-provided writer (file appender from the TUI binary).

use std::io;

use tracing_subscriber::{fmt, prelude::*, EnvFilter};

const FILTER_ENV: &str = "SIGNALSCOPE_LOG";

fn env_filter() -> EnvFilter {
    EnvFilter::try_from_env(FILTER_ENV).unwrap_or_else(|_| EnvFilter::new("info"))
}

/// Initialize logging to stderr. Suitable for CLI tools and tests, but NOT
/// for the TUI binary (the TUI must redirect logs away from the terminal).
pub fn init_stderr() {
    let _ = tracing_subscriber::registry()
        .with(env_filter())
        .with(fmt::layer().with_writer(io::stderr).with_target(false))
        .try_init();
}

/// Initialize logging to the supplied writer factory. The TUI passes a
/// non-blocking file appender here so terminal output stays clean.
pub fn init_with_writer<W>(make_writer: W)
where
    W: for<'a> fmt::MakeWriter<'a> + Send + Sync + 'static,
{
    let _ = tracing_subscriber::registry()
        .with(env_filter())
        .with(
            fmt::layer()
                .with_writer(make_writer)
                .with_target(false)
                .with_ansi(false),
        )
        .try_init();
}
