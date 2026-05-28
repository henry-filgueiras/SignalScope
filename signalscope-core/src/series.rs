//! Bounded rolling window of wall-clock-timestamped samples.
//!
//! [`TemporalSeries`] is the small, deliberately ungeneric building block
//! the dashboard uses to keep rolling visual history: throughput rows on
//! the connected-link card, RSSI sparkline, gateway/DNS RTT timelines.
//! It is not a metrics framework. It is:
//!
//! * **bounded by sample count** so memory is predictable and the
//!   resulting series fits neatly onto a sparkline of known width;
//! * **timestamped with wall-clock `OffsetDateTime`** rather than
//!   `Instant`, so the same series can be reconstructed from a recorded
//!   session and rendered identically — temporal semantics survive
//!   restart and replay;
//! * **decoupled from any specific sensor type** — `T` is free.
//!
//! Helpers that need ordering (`max`, `min`) require `T: PartialOrd`;
//! helpers that look back over a time window (`mean_over`,
//! `elapsed_since_last`) are available for any `T` and read off the
//! sample timestamps directly.

use std::collections::VecDeque;
use std::time::Duration;

use time::OffsetDateTime;

/// One sample in a [`TemporalSeries`]. The timestamp is wall-clock so
/// the series remains meaningful across restarts and replays.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct TemporalSample<T> {
    pub at: OffsetDateTime,
    pub value: T,
}

#[derive(Debug, Clone)]
pub struct TemporalSeries<T> {
    capacity: usize,
    samples: VecDeque<TemporalSample<T>>,
}

impl<T> TemporalSeries<T> {
    /// Construct a new empty series with the given fixed sample capacity.
    /// `capacity == 0` is allowed and degrades to a series that never
    /// retains anything — useful for cases where a downstream renderer
    /// is disabled at configuration time.
    pub fn new(capacity: usize) -> Self {
        Self {
            capacity,
            samples: VecDeque::with_capacity(capacity),
        }
    }

    pub fn capacity(&self) -> usize {
        self.capacity
    }

    pub fn len(&self) -> usize {
        self.samples.len()
    }

    pub fn is_empty(&self) -> bool {
        self.samples.is_empty()
    }

    /// Append a sample. The oldest sample is evicted when the series is
    /// at capacity. Timestamps are not required to be monotonic — the
    /// series is positional, not sorted — but callers normally push in
    /// publication order off the bus.
    pub fn push(&mut self, at: OffsetDateTime, value: T) {
        if self.capacity == 0 {
            return;
        }
        if self.samples.len() == self.capacity {
            self.samples.pop_front();
        }
        self.samples.push_back(TemporalSample { at, value });
    }

    pub fn clear(&mut self) {
        self.samples.clear();
    }

    pub fn iter(&self) -> impl DoubleEndedIterator<Item = &TemporalSample<T>> {
        self.samples.iter()
    }

    pub fn iter_values(&self) -> impl DoubleEndedIterator<Item = &T> {
        self.samples.iter().map(|s| &s.value)
    }

    pub fn latest(&self) -> Option<&TemporalSample<T>> {
        self.samples.back()
    }

    pub fn earliest(&self) -> Option<&TemporalSample<T>> {
        self.samples.front()
    }

    /// Wall-clock span between the earliest and latest retained sample.
    /// `None` when the series holds fewer than two samples.
    pub fn span(&self) -> Option<Duration> {
        let first = self.samples.front()?;
        let last = self.samples.back()?;
        let secs = (last.at - first.at).whole_seconds().max(0);
        if secs == 0 && self.samples.len() < 2 {
            return None;
        }
        Some(Duration::from_secs(secs as u64))
    }

    /// Wall-clock distance between `now` and the most recent sample.
    /// Useful for "idle for Xs" callouts. `None` when the series is empty.
    pub fn elapsed_since_last(&self, now: OffsetDateTime) -> Option<Duration> {
        let last = self.samples.back()?;
        let secs = (now - last.at).whole_seconds().max(0);
        Some(Duration::from_secs(secs as u64))
    }
}

impl<T: Clone> TemporalSeries<T> {
    /// Snapshot of the retained values, in chronological order.
    pub fn values(&self) -> Vec<T> {
        self.samples.iter().map(|s| s.value.clone()).collect()
    }
}

impl<T: PartialOrd + Clone> TemporalSeries<T> {
    pub fn max_value(&self) -> Option<T> {
        self.samples.iter().map(|s| s.value.clone()).reduce(|a, b| {
            if b.partial_cmp(&a).unwrap_or(std::cmp::Ordering::Equal) == std::cmp::Ordering::Greater
            {
                b
            } else {
                a
            }
        })
    }
}

impl TemporalSeries<f64> {
    /// Mean of samples whose timestamp falls within `lookback` of `now`.
    /// Returns `None` if no sample falls inside the window.
    pub fn mean_over(&self, lookback: Duration, now: OffsetDateTime) -> Option<f64> {
        let cutoff = now - time::Duration::seconds(lookback.as_secs() as i64);
        let mut sum = 0.0_f64;
        let mut count = 0_usize;
        for s in &self.samples {
            if s.at >= cutoff {
                sum += s.value;
                count += 1;
            }
        }
        if count == 0 {
            None
        } else {
            Some(sum / count as f64)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ts(secs: i64) -> OffsetDateTime {
        OffsetDateTime::from_unix_timestamp(1_700_000_000 + secs).unwrap()
    }

    #[test]
    fn push_evicts_oldest_when_at_capacity() {
        let mut s = TemporalSeries::<i32>::new(3);
        s.push(ts(0), 1);
        s.push(ts(1), 2);
        s.push(ts(2), 3);
        s.push(ts(3), 4);
        assert_eq!(s.len(), 3);
        let vals: Vec<i32> = s.iter_values().copied().collect();
        assert_eq!(vals, vec![2, 3, 4]);
    }

    #[test]
    fn zero_capacity_retains_nothing() {
        let mut s = TemporalSeries::<i32>::new(0);
        s.push(ts(0), 1);
        s.push(ts(1), 2);
        assert!(s.is_empty());
        assert!(s.latest().is_none());
    }

    #[test]
    fn span_is_none_until_two_samples() {
        let mut s = TemporalSeries::<i32>::new(10);
        assert!(s.span().is_none());
        s.push(ts(0), 1);
        assert!(s.span().is_none());
        s.push(ts(7), 2);
        assert_eq!(s.span(), Some(Duration::from_secs(7)));
    }

    #[test]
    fn elapsed_since_last_tracks_quiescence() {
        let mut s = TemporalSeries::<i32>::new(10);
        s.push(ts(100), 1);
        assert_eq!(
            s.elapsed_since_last(ts(160)),
            Some(Duration::from_secs(60))
        );
    }

    #[test]
    fn mean_over_only_includes_recent_samples() {
        let mut s = TemporalSeries::<f64>::new(10);
        // older samples that should be excluded
        s.push(ts(0), 100.0);
        s.push(ts(10), 200.0);
        // recent samples
        s.push(ts(90), 30.0);
        s.push(ts(95), 40.0);
        // 30 s lookback at now=100 → cutoff=70 → recent half only
        let mean = s.mean_over(Duration::from_secs(30), ts(100)).unwrap();
        assert!((mean - 35.0).abs() < 1e-9, "got {mean}");
    }

    #[test]
    fn mean_over_returns_none_when_window_is_empty() {
        let mut s = TemporalSeries::<f64>::new(10);
        s.push(ts(0), 100.0);
        assert!(s.mean_over(Duration::from_secs(10), ts(100)).is_none());
    }

    #[test]
    fn max_value_returns_the_largest_retained_sample() {
        let mut s = TemporalSeries::<f64>::new(5);
        s.push(ts(0), 1.0);
        s.push(ts(1), 5.0);
        s.push(ts(2), 3.0);
        assert_eq!(s.max_value(), Some(5.0));
    }

    #[test]
    fn values_returns_chronological_snapshot() {
        let mut s = TemporalSeries::<i32>::new(5);
        s.push(ts(0), 10);
        s.push(ts(1), 20);
        s.push(ts(2), 30);
        assert_eq!(s.values(), vec![10, 20, 30]);
    }
}
