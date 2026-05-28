//! Shared runtime for SignalScope: clock, event bus, logging setup.
//!
//! The event bus is the backbone of the system. Sensors publish into it;
//! analysis and the TUI subscribe from it. It is *append-only* — once an
//! envelope is published it cannot be mutated or revoked.

#![forbid(unsafe_code)]
#![warn(missing_debug_implementations)]

pub mod bus;
pub mod clock;
pub mod logging;
pub mod series;
pub mod session;
pub mod source;

pub use bus::{EventBus, Subscription};
pub use clock::{Clock, SystemClock};
pub use series::{TemporalSample, TemporalSeries};
pub use session::{
    spawn_recorder, summarize as summarize_session, SessionHeader, SessionReadError, SessionReader,
    SessionRow, SessionStats, SessionWriter, SESSION_FORMAT_VERSION, SESSION_KIND,
    SESSION_MIN_READABLE_VERSION,
};
pub use source::{EventSource, FileEventSource};
