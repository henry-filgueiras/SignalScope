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
pub mod session;
pub mod source;

pub use bus::{EventBus, Subscription};
pub use clock::{Clock, SystemClock};
pub use session::{
    spawn_recorder, SessionHeader, SessionReadError, SessionReader, SessionRow, SessionWriter,
    SESSION_FORMAT_VERSION, SESSION_KIND,
};
pub use source::{EventSource, FileEventSource};
