//! Sensor abstraction + adapters that emit normalized observations.
//!
//! ## Design
//!
//! A `Sensor` is anything that can be spawned with a handle to the event bus
//! and run as a long-lived background task. Sensors emit
//! [`signalscope_events::Event`]s. They are intentionally:
//!
//! * narrow — one source, one cadence;
//! * thin — they parse, normalize, and publish, nothing else;
//! * isolated — they never inspect each other's output (that's analysis's
//!   job).
//!
//! ## Platform adapters
//!
//! Currently macOS-focused. The `wifi::macos` adapter shells out to the
//! `airport` CLI for AP info and scans, which is fragile on modern macOS but
//! adequate for bootstrap. Linux netlink (`nl80211`) and pcap integrations
//! are intentional future work — see `docs/sensor-model.md`.

#![forbid(unsafe_code)]
#![warn(missing_debug_implementations)]

use std::sync::Arc;

use signalscope_core::EventBus;
use signalscope_events::SensorId;
use tokio::task::JoinHandle;

pub mod dns;
pub mod gateway;
pub mod iface;
pub mod scheduler;
pub mod wifi;

pub use scheduler::{SensorScheduler, SensorSpec};

/// A lightweight sensor abstraction. Implementors own their cadence; the
/// scheduler simply spawns them.
pub trait Sensor: Send + 'static {
    fn id(&self) -> SensorId;
    fn spawn(self, bus: Arc<EventBus>) -> JoinHandle<()>;
}
