//! Clock abstraction — wall time for now, replay-time later.

use std::sync::Arc;

use time::OffsetDateTime;

pub trait Clock: Send + Sync + std::fmt::Debug {
    fn now(&self) -> OffsetDateTime;
}

#[derive(Debug, Default, Clone, Copy)]
pub struct SystemClock;

impl Clock for SystemClock {
    fn now(&self) -> OffsetDateTime {
        OffsetDateTime::now_utc()
    }
}

/// Convenience: an `Arc<dyn Clock>` so downstream callers don't need generics.
pub fn system() -> Arc<dyn Clock> {
    Arc::new(SystemClock)
}
