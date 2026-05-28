//! Minimal sensor scheduler.
//!
//! Spawns each registered sensor on a tokio task, retains its `JoinHandle` so
//! the application can await graceful shutdown, and exposes a
//! [`SensorSpec`] description for the UI's "sensor health" surface (future
//! work).

use std::sync::Arc;

use signalscope_core::EventBus;
use signalscope_events::SensorId;
use tokio::task::JoinHandle;
use tracing::{info, warn};

use crate::Sensor;

#[derive(Debug)]
pub struct SensorSpec {
    pub id: SensorId,
    pub handle: JoinHandle<()>,
}

#[derive(Debug, Default)]
pub struct SensorScheduler {
    specs: Vec<SensorSpec>,
}

impl SensorScheduler {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn add<S: Sensor>(&mut self, bus: Arc<EventBus>, sensor: S) -> &mut Self {
        let id = sensor.id();
        info!(sensor = %id, "starting sensor");
        let handle = sensor.spawn(bus);
        self.specs.push(SensorSpec { id, handle });
        self
    }

    pub fn ids(&self) -> impl Iterator<Item = &SensorId> {
        self.specs.iter().map(|s| &s.id)
    }

    /// Abort all sensor tasks. Used during graceful shutdown.
    pub async fn shutdown(self) {
        for spec in self.specs {
            spec.handle.abort();
            // Awaiting a JoinHandle after abort returns a JoinError; we only
            // care that the task is no longer running.
            if let Err(e) = spec.handle.await {
                if !e.is_cancelled() {
                    warn!(error = %e, "sensor task ended with error");
                }
            }
        }
    }
}
