//! Pull abstraction over envelope streams.
//!
//! Two implementations matter today:
//!
//! * [`Subscription`](crate::bus::Subscription) — live, broadcast from the bus.
//! * [`FileEventSource`] — replay from a session file written by
//!   [`SessionWriter`](crate::session::SessionWriter).
//!
//! Both yield `Arc<Envelope>` via `async fn next()`. Consumers that only
//! need to consume envelopes in order (the TUI, the analysis engine) can be
//! written against [`EventSource`] and operate against either source without
//! caring whether the data is live or recorded.
//!
//! This is intentionally a minimal abstraction — no seeking, no pacing
//! controls, no timeline scrubbing. Those are replay-UI concerns and remain
//! out of scope.

use std::sync::Arc;

use signalscope_events::Envelope;

use crate::bus::Subscription;
use crate::session::{SessionReader, SessionReadError};

/// A pull-style source of envelopes. `None` means the source is exhausted
/// (bus closed, file fully read).
pub trait EventSource: Send {
    fn next_envelope(
        &mut self,
    ) -> impl std::future::Future<Output = Option<Arc<Envelope>>> + Send + '_;
}

impl EventSource for Subscription {
    async fn next_envelope(&mut self) -> Option<Arc<Envelope>> {
        self.recv().await
    }
}

/// Replay envelopes out of a session file, as-fast-as-possible. Wraps
/// [`SessionReader`]. Parse failures are surfaced via `tracing::warn` and
/// then treated as end-of-stream — there is nothing useful a generic
/// consumer can do with a half-readable session, and silently swallowing
/// corruption is worse than stopping.
#[derive(Debug)]
pub struct FileEventSource {
    reader: SessionReader,
}

impl FileEventSource {
    pub fn new(reader: SessionReader) -> Self {
        Self { reader }
    }

    pub fn open(path: impl AsRef<std::path::Path>) -> Result<Self, SessionReadError> {
        Ok(Self::new(SessionReader::open(path)?))
    }

    pub fn header(&self) -> &crate::session::SessionHeader {
        self.reader.header()
    }
}

impl EventSource for FileEventSource {
    async fn next_envelope(&mut self) -> Option<Arc<Envelope>> {
        match self.reader.next()? {
            Ok(env) => Some(Arc::new(env)),
            Err(e) => {
                tracing::warn!(error = %e, "session replay aborted on parse error");
                None
            }
        }
    }
}
