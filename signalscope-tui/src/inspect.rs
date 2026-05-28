//! `signalscope inspect <path>` — confirm a recorded session is what
//! the recipient thinks it is.
//!
//! Prints, in one screen:
//!
//! * file metadata (format version, tool version, label, recording
//!   start, recording span, envelope count);
//! * a per-category event tally (observations, gateway/DNS probes,
//!   interface counters, findings, sensor health);
//! * the first and last wall-clock timestamps inside the session.
//!
//! No TUI, no replay, no analysis. Just the canonical "yes this is a
//! signalscope recording and here is what's in it" verifier the
//! success-criteria flow needs.

use std::path::PathBuf;

use anyhow::Result;
use signalscope_core::{summarize_session, SessionStats};

pub struct InspectOptions {
    pub path: PathBuf,
}

pub async fn run(opts: InspectOptions) -> Result<()> {
    let (header, stats) = summarize_session(&opts.path)?;

    let span = stats
        .duration()
        .map(humanize_duration)
        .unwrap_or_else(|| "—".to_string());
    let first = stats
        .first_at
        .map(|t| {
            t.format(&time::format_description::well_known::Rfc3339)
                .unwrap_or_else(|_| "—".into())
        })
        .unwrap_or_else(|| "—".to_string());
    let last = stats
        .last_at
        .map(|t| {
            t.format(&time::format_description::well_known::Rfc3339)
                .unwrap_or_else(|_| "—".into())
        })
        .unwrap_or_else(|| "—".to_string());

    println!("signalscope session  ·  {}", opts.path.display());
    println!("─");
    println!("  kind             {}", header.kind);
    println!("  format_version   {}", header.format_version);
    println!("  tool_version     {}", header.tool_version);
    if let Some(label) = header.label.as_deref() {
        println!("  label            {label}");
    }
    println!(
        "  created_at       {}",
        header
            .created_at
            .format(&time::format_description::well_known::Rfc3339)
            .unwrap_or_else(|_| "—".into())
    );
    println!("─");
    println!("  envelopes        {}", stats.envelope_count);
    println!("  span             {span}");
    println!("  first event      {first}");
    println!("  last event       {last}");
    println!("─");
    println!("  by category");
    for (label, count) in category_rows(&stats) {
        if count > 0 {
            println!("    {label:<14}  {count}");
        }
    }
    Ok(())
}

fn category_rows(s: &SessionStats) -> [(&'static str, u64); 9] {
    [
        ("wifi", s.wifi),
        ("scan", s.scan),
        ("gateway", s.gateway),
        ("dns", s.dns),
        ("iface_counter", s.iface),
        ("iface_state", s.iface_state),
        ("roam", s.roam),
        ("findings", s.findings),
        ("sensor_health", s.health),
    ]
}

fn humanize_duration(d: std::time::Duration) -> String {
    let secs = d.as_secs();
    let h = secs / 3600;
    let m = (secs % 3600) / 60;
    let s = secs % 60;
    if h > 0 {
        format!("{h}h{m:02}m{s:02}s")
    } else if m > 0 {
        format!("{m}m{s:02}s")
    } else {
        format!("{s}s")
    }
}
