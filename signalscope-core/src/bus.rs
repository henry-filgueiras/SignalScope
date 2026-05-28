//! Append-only in-memory event bus.
//!
//! Two roles:
//!
//! 1. *Broadcast*: live subscribers (the TUI, the analysis loop) receive
//!    each published envelope via a `tokio::sync::broadcast` channel.
//! 2. *Ring buffer*: a bounded back-buffer of recent envelopes is retained so
//!    that newly-attached consumers (e.g. the TUI on startup, or analysis
//!    rules that need a short look-back window) can replay recent history
//!    without losing it to broadcast lag.
//!
//! The bus assigns monotonic `EventId`s. It does *not* persist events —
//! durable storage is a future concern (see `docs/architecture.md`).

use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use parking_lot::Mutex;
use std::collections::VecDeque;
use tokio::sync::broadcast;

use signalscope_events::{Envelope, Event, EventId, SensorId};

use crate::clock::{Clock, SystemClock};

/// Default broadcast channel capacity. Slow subscribers that lag past this
/// will observe `Lagged` errors and skip ahead — see `Subscription::recv`.
const DEFAULT_BROADCAST_CAPACITY: usize = 1024;

/// Default ring-buffer capacity used for back-replay.
const DEFAULT_BACKLOG_CAPACITY: usize = 4096;

#[derive(Debug)]
pub struct EventBus {
    next_id: AtomicU64,
    clock: Arc<dyn Clock>,
    tx: broadcast::Sender<Arc<Envelope>>,
    backlog: Mutex<VecDeque<Arc<Envelope>>>,
    backlog_capacity: usize,
}

impl EventBus {
    pub fn new() -> Arc<Self> {
        Self::with_capacities(DEFAULT_BROADCAST_CAPACITY, DEFAULT_BACKLOG_CAPACITY)
    }

    pub fn with_capacities(broadcast_cap: usize, backlog_cap: usize) -> Arc<Self> {
        let (tx, _rx) = broadcast::channel(broadcast_cap);
        Arc::new(Self {
            next_id: AtomicU64::new(1),
            clock: Arc::new(SystemClock),
            tx,
            backlog: Mutex::new(VecDeque::with_capacity(backlog_cap)),
            backlog_capacity: backlog_cap,
        })
    }

    /// Publish an event from `source`. Returns the assigned envelope.
    pub fn publish(&self, source: SensorId, event: Event) -> Arc<Envelope> {
        let id = EventId(self.next_id.fetch_add(1, Ordering::Relaxed));
        let env = Arc::new(Envelope::with_time(id, self.clock.now(), source, event));

        {
            let mut backlog = self.backlog.lock();
            if backlog.len() == self.backlog_capacity {
                backlog.pop_front();
            }
            backlog.push_back(env.clone());
        }

        // Errors here mean "no live subscribers" — that's fine; the backlog
        // still retains the event for the next subscriber.
        let _ = self.tx.send(env.clone());
        env
    }

    /// Subscribe for future events. The returned `Subscription` does *not*
    /// include backlog by default — call [`Self::recent`] for that.
    pub fn subscribe(self: &Arc<Self>) -> Subscription {
        Subscription {
            rx: self.tx.subscribe(),
        }
    }

    /// Snapshot of the back-buffer in chronological order. Cheap because the
    /// envelopes are `Arc`-shared with subscribers.
    pub fn recent(&self) -> Vec<Arc<Envelope>> {
        let backlog = self.backlog.lock();
        backlog.iter().cloned().collect()
    }

    /// Snapshot the last `n` envelopes in chronological order.
    pub fn recent_n(&self, n: usize) -> Vec<Arc<Envelope>> {
        let backlog = self.backlog.lock();
        let start = backlog.len().saturating_sub(n);
        backlog.iter().skip(start).cloned().collect()
    }
}

#[derive(Debug)]
pub struct Subscription {
    rx: broadcast::Receiver<Arc<Envelope>>,
}

impl Subscription {
    /// Await the next envelope. Returns `None` if the bus has been dropped.
    /// Lagged subscribers skip silently to the newest item — the design
    /// favors latency over completeness for the live view (the backlog is the
    /// source of truth for catch-up).
    pub async fn recv(&mut self) -> Option<Arc<Envelope>> {
        loop {
            match self.rx.recv().await {
                Ok(env) => return Some(env),
                Err(broadcast::error::RecvError::Lagged(_)) => continue,
                Err(broadcast::error::RecvError::Closed) => return None,
            }
        }
    }
}
