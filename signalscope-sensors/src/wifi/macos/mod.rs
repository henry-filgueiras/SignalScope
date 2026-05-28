//! macOS Wi-Fi acquisition: backend selection + dispatch.
//!
//! The sensor body (in `wifi/mod.rs`) treats macOS Wi-Fi as a single
//! capability. This module is the implementation detail: it picks the best
//! backend available on the host and produces a normalized [`WifiSnapshot`].
//!
//! ## Backend strategy
//!
//! 1. **`system_profiler -xml SPAirPortDataType`** — primary. Works on every
//!    modern macOS, no special privileges. Heavy (multi-second invocation),
//!    so the sensor polls it conservatively.
//! 2. **`airport -I` / `airport -s`** — legacy compatibility. Useful on
//!    older macOS hosts where `airport` still ships and offers fields that
//!    `system_profiler` redacts (e.g. BSSID without Location Services).
//!
//! `wdutil info` was considered but it requires root and overlaps heavily
//! with `system_profiler`; the cost/benefit doesn't justify it in this
//! phase.

use std::path::Path;

use signalscope_events::{ScanResult, WifiObservation};
use thiserror::Error;
use tokio::process::Command;
use tracing::{debug, info};

pub mod airport;
pub mod system_profiler;

const AIRPORT_BIN: &str =
    "/System/Library/PrivateFrameworks/Apple80211.framework/Versions/Current/Resources/airport";

const SYSTEM_PROFILER_BIN: &str = "/usr/sbin/system_profiler";

/// A point-in-time capture of Wi-Fi state. Either field may be `None` if
/// the backend couldn't observe it this cycle.
#[derive(Debug, Clone)]
pub struct WifiSnapshot {
    pub link: Option<WifiObservation>,
    pub scan: Option<ScanResult>,
}

/// Backend chosen at sensor startup. Once selected we stick with it; we
/// don't re-shop on every tick. If a transient failure occurs, the sensor
/// surfaces a `SensorHealth` event rather than thrashing between backends.
#[derive(Debug)]
pub enum WifiBackend {
    SystemProfiler,
    Airport,
}

impl WifiBackend {
    pub fn name(&self) -> &'static str {
        match self {
            WifiBackend::SystemProfiler => "system_profiler",
            WifiBackend::Airport => "airport",
        }
    }

    /// Try a single acquisition cycle.
    pub async fn snapshot(&self, interface: &str) -> Result<WifiSnapshot, BackendError> {
        match self {
            WifiBackend::SystemProfiler => system_profiler::snapshot(interface).await,
            WifiBackend::Airport => airport::snapshot(interface).await,
        }
    }
}

/// Errors a backend may report on a single cycle. Map these to
/// `SensorState` at the sensor layer; do not bake state semantics into
/// the parser modules.
#[derive(Debug, Error)]
pub enum BackendError {
    #[error("backend executable not found: {0}")]
    BinaryMissing(String),
    #[error("Wi-Fi hardware reports off / disabled")]
    HardwareDisabled,
    #[error("permission denied: {0}")]
    PermissionDenied(String),
    #[error("backend output could not be parsed: {0}")]
    Parse(String),
    #[error("backend timed out")]
    Timeout,
    #[error("io error: {0}")]
    Io(#[from] std::io::Error),
    #[error("{0}")]
    Other(String),
}

/// Outcome of [`detect_backend`]. Carries the chosen backend plus, for
/// `system_profiler`, the raw output we captured during the probe — so
/// the sensor can emit its first observation without paying
/// `system_profiler`'s cold-start cost a second time. On this host the
/// cold start is ~12 s; before this optimization the sensor ran
/// `system_profiler` twice serially at startup (probe + first snapshot)
/// and operators saw a ~26 s gap before any Wi-Fi data appeared.
#[derive(Debug)]
pub struct DetectedBackend {
    pub backend: WifiBackend,
    /// Raw bytes from the probe invocation. `Some` for backends where the
    /// probe naturally produces a usable snapshot (today: `system_profiler`'s
    /// XML output). The sensor should hand these to the backend's parser
    /// once before entering the normal interval cycle.
    pub primed_bytes: Option<Vec<u8>>,
}

/// Choose the best backend available on this host. Heuristic: prefer
/// `system_profiler` (works on every modern macOS); fall back to legacy
/// `airport` only when present. Returns `None` if neither is usable, in
/// which case the sensor should surface
/// [`signalscope_events::SensorState::BackendUnavailable`].
pub async fn detect_backend() -> Option<DetectedBackend> {
    if Path::new(SYSTEM_PROFILER_BIN).exists() {
        if let Some(bytes) = system_profiler_probe_with_output().await {
            info!(backend = "system_profiler", "wifi backend selected");
            return Some(DetectedBackend {
                backend: WifiBackend::SystemProfiler,
                primed_bytes: Some(bytes),
            });
        }
    }
    if Path::new(AIRPORT_BIN).exists() {
        info!(backend = "airport", "wifi backend selected (legacy)");
        return Some(DetectedBackend {
            backend: WifiBackend::Airport,
            primed_bytes: None,
        });
    }
    debug!("no wifi backend available");
    None
}

/// Probe `system_profiler` and capture its output. If the invocation
/// succeeds with non-empty stdout we both (a) commit to this backend
/// and (b) return the bytes so the sensor can use them as the primed
/// first snapshot. We deliberately keep the probe and the first
/// snapshot the same call — there's no point paying for two cold
/// starts back-to-back when one invocation produces both signals.
async fn system_profiler_probe_with_output() -> Option<Vec<u8>> {
    let out = Command::new(SYSTEM_PROFILER_BIN)
        .args(["-xml", "SPAirPortDataType"])
        .output()
        .await
        .ok()?;
    if out.status.success() && !out.stdout.is_empty() {
        Some(out.stdout)
    } else {
        None
    }
}
