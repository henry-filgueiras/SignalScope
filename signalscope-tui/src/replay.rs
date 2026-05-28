//! Offline session replay: load a `.signalscope-session` file, drive
//! the dashboard against the recorded envelope stream.
//!
//! The replay model is simple. The whole file is read into memory as
//! a `Vec<Arc<Envelope>>`. A *playhead* is an index into that vec.
//! The dashboard renders the state as it existed at the playhead's
//! event timestamp — "virtual now" = `events[playhead].at`. Seek
//! keys move the playhead by event count (`[`/`]` ±1, `{`/`}` ±10,
//! `Home`/`End` to endpoints). Every seek does a full re-ingest of
//! events `0..=playhead` into a freshly-reset `AppState`.
//!
//! Two design decisions worth flagging:
//!
//! * **Event-anchored playhead.** The user always lands on a real
//!   event — never inside a gap. Recordings with two events ten
//!   hours apart still navigate cleanly: `]` jumps from one event
//!   to the other without "swaths of no-ops." Time-based scrubbing
//!   would have to snap-to-nearest-event anyway; we just collapse
//!   the two operations.
//! * **Full rebuild on every seek.** Re-ingesting up to N envelopes
//!   into an empty `AppState` is microseconds for sessions of any
//!   plausible size. The bottleneck is the operator's reaction
//!   time, not the CPU. Skipping the cache simplifies the
//!   correctness story enormously — every seek lands in the exact
//!   same state the operator would have seen had they stopped the
//!   recording at that moment.

use std::path::Path;
use std::sync::Arc;

use anyhow::{anyhow, Result};
use signalscope_core::{SessionHeader, SessionReader};
use signalscope_events::Envelope;
use time::OffsetDateTime;

use crate::landmarks::{self, TimelineLandmark};

/// Loaded, immutable session contents plus a moveable playhead.
#[derive(Debug, Clone)]
pub struct Playback {
    pub header: Arc<SessionHeader>,
    pub events: Arc<[Arc<Envelope>]>,
    /// Index of the event currently anchoring the dashboard. Invariant:
    /// `playhead < events.len()`.
    pub playhead: usize,
    /// Derived landmarks for the recording. Computed once at load time
    /// over the immutable envelope vec — a pure function of the events
    /// (see [`crate::landmarks::derive`]), so the same recording
    /// always yields the same landmarks across runs.
    pub landmarks: Arc<[TimelineLandmark]>,
}

impl Playback {
    pub fn load(path: impl AsRef<Path>) -> Result<Self> {
        let path = path.as_ref();
        let mut reader = SessionReader::open(path)?;
        let header = reader.header().clone();
        let mut events: Vec<Arc<Envelope>> = Vec::new();
        for env in &mut reader {
            events.push(Arc::new(env?));
        }
        if events.is_empty() {
            return Err(anyhow!(
                "session at {} contains zero envelopes — nothing to replay",
                path.display()
            ));
        }
        let playhead = events.len() - 1;
        let events: Arc<[Arc<Envelope>]> = events.into();
        let landmarks: Arc<[TimelineLandmark]> = landmarks::derive(&events).into();
        Ok(Self {
            header: Arc::new(header),
            events,
            playhead,
            landmarks,
        })
    }

    pub fn len(&self) -> usize {
        self.events.len()
    }

    /// Wall-clock timestamp at the playhead — the dashboard's
    /// virtual "now."
    pub fn virtual_now(&self) -> OffsetDateTime {
        self.events[self.playhead].at
    }

    /// Timestamp of the first envelope in the recording.
    pub fn first_at(&self) -> OffsetDateTime {
        self.events[0].at
    }

    /// Timestamp of the last envelope.
    pub fn last_at(&self) -> OffsetDateTime {
        self.events[self.events.len() - 1].at
    }

    /// Wall-clock offset of the playhead from the recording's start.
    pub fn elapsed(&self) -> std::time::Duration {
        let secs = (self.virtual_now() - self.first_at()).whole_seconds().max(0);
        std::time::Duration::from_secs(secs as u64)
    }

    /// Total recorded span — last event timestamp minus first.
    pub fn total_span(&self) -> std::time::Duration {
        let secs = (self.last_at() - self.first_at()).whole_seconds().max(0);
        std::time::Duration::from_secs(secs as u64)
    }

    /// Move the playhead by `delta` events, clamped to the valid
    /// range. Negative `delta` moves backward. Returns whether the
    /// playhead actually moved — if not, the seek can suppress a
    /// redraw.
    pub fn seek_by(&mut self, delta: isize) -> bool {
        let n = self.events.len() as isize;
        let new = (self.playhead as isize + delta).clamp(0, n - 1) as usize;
        if new == self.playhead {
            return false;
        }
        self.playhead = new;
        true
    }

    pub fn seek_to_start(&mut self) -> bool {
        if self.playhead == 0 {
            return false;
        }
        self.playhead = 0;
        true
    }

    pub fn seek_to_end(&mut self) -> bool {
        let end = self.events.len() - 1;
        if self.playhead == end {
            return false;
        }
        self.playhead = end;
        true
    }

    /// Envelopes from the start of the recording up to and including
    /// the current playhead — exactly what should be re-ingested
    /// into a freshly-reset `AppState` to reproduce the dashboard
    /// state at the playhead.
    pub fn envelopes_through_playhead(&self) -> &[Arc<Envelope>] {
        &self.events[..=self.playhead]
    }

    /// Highest-indexed landmark whose source event has already been
    /// crossed by the playhead. This is the "current" landmark the
    /// list panel should highlight. `None` if the playhead is before
    /// the first landmark.
    pub fn current_landmark_index(&self) -> Option<usize> {
        let ph = self.playhead;
        // Landmarks are produced in chronological order over the
        // envelope vec — find the last whose event_index <= playhead.
        let mut found = None;
        for (i, l) in self.landmarks.iter().enumerate() {
            if l.event_index <= ph {
                found = Some(i);
            } else {
                break;
            }
        }
        found
    }

    /// Move the playhead to the source event of the next landmark
    /// strictly after the current playhead position. Returns whether
    /// the playhead moved.
    pub fn seek_to_next_landmark(&mut self) -> bool {
        let ph = self.playhead;
        if let Some(next) = self
            .landmarks
            .iter()
            .find(|l| l.event_index > ph)
        {
            self.playhead = next.event_index;
            true
        } else {
            false
        }
    }

    /// Move the playhead to the source event of the most recent
    /// landmark strictly before the current playhead position.
    /// Returns whether the playhead moved.
    pub fn seek_to_prev_landmark(&mut self) -> bool {
        let ph = self.playhead;
        if let Some(prev) = self
            .landmarks
            .iter()
            .rev()
            .find(|l| l.event_index < ph)
        {
            self.playhead = prev.event_index;
            true
        } else {
            false
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use signalscope_core::{SessionHeader, SessionWriter};
    use signalscope_events::{
        Event, EventId, GatewayLatencyObservation, SensorId,
    };

    fn write_session_with_n_events(n: u64) -> (tempfile::TempDir, std::path::PathBuf) {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("nav.signalscope-session");
        let writer = SessionWriter::create(&path, SessionHeader::new(Some("nav".into()))).unwrap();
        for i in 1..=n {
            let env = Envelope::with_time(
                EventId(i),
                OffsetDateTime::from_unix_timestamp(1_700_000_000 + i as i64).unwrap(),
                SensorId::new("gateway"),
                Event::GatewayLatency(GatewayLatencyObservation {
                    target: "192.168.1.1".into(),
                    rtt: std::time::Duration::from_millis(i),
                    reachable: true,
                    probe: "icmp".into(),
                }),
            );
            writer.record(&env).unwrap();
        }
        (dir, path)
    }

    #[test]
    fn loads_with_playhead_at_end() {
        let (_dir, path) = write_session_with_n_events(5);
        let pb = Playback::load(&path).unwrap();
        assert_eq!(pb.len(), 5);
        assert_eq!(pb.playhead, 4, "playhead should default to last event");
        assert_eq!(pb.virtual_now(), pb.last_at());
    }

    #[test]
    fn rejects_empty_session() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("empty.signalscope-session");
        // Header but no envelopes.
        let writer = SessionWriter::create(&path, SessionHeader::new(None)).unwrap();
        drop(writer);
        match Playback::load(&path) {
            Err(e) => assert!(format!("{e}").contains("zero envelopes")),
            Ok(_) => panic!("expected error on empty session"),
        }
    }

    #[test]
    fn seek_steps_clamp_at_boundaries() {
        let (_dir, path) = write_session_with_n_events(3);
        let mut pb = Playback::load(&path).unwrap();
        // Start at end (index 2). Forward by 5 should clamp, return false.
        assert!(!pb.seek_by(5));
        assert_eq!(pb.playhead, 2);
        // Back by 10 should clamp to 0.
        assert!(pb.seek_by(-10));
        assert_eq!(pb.playhead, 0);
        // Back again at 0 is a no-op.
        assert!(!pb.seek_by(-1));
        assert_eq!(pb.playhead, 0);
    }

    #[test]
    fn seek_by_one_walks_event_by_event() {
        let (_dir, path) = write_session_with_n_events(4);
        let mut pb = Playback::load(&path).unwrap();
        pb.seek_to_start();
        assert_eq!(pb.playhead, 0);
        assert!(pb.seek_by(1));
        assert_eq!(pb.playhead, 1);
        assert!(pb.seek_by(1));
        assert_eq!(pb.playhead, 2);
        assert!(pb.seek_by(-1));
        assert_eq!(pb.playhead, 1);
    }

    #[test]
    fn seek_to_endpoints_works() {
        let (_dir, path) = write_session_with_n_events(7);
        let mut pb = Playback::load(&path).unwrap();
        assert!(pb.seek_to_start());
        assert_eq!(pb.playhead, 0);
        assert!(!pb.seek_to_start()); // already there
        assert!(pb.seek_to_end());
        assert_eq!(pb.playhead, 6);
        assert!(!pb.seek_to_end()); // already there
    }

    #[test]
    fn envelopes_through_playhead_grows_with_playhead() {
        let (_dir, path) = write_session_with_n_events(5);
        let mut pb = Playback::load(&path).unwrap();
        pb.seek_to_start();
        assert_eq!(pb.envelopes_through_playhead().len(), 1);
        pb.seek_by(2);
        assert_eq!(pb.envelopes_through_playhead().len(), 3);
        pb.seek_to_end();
        assert_eq!(pb.envelopes_through_playhead().len(), 5);
    }

    #[test]
    fn virtual_now_tracks_playhead_timestamp() {
        let (_dir, path) = write_session_with_n_events(3);
        let mut pb = Playback::load(&path).unwrap();
        pb.seek_to_start();
        assert_eq!(
            pb.virtual_now(),
            OffsetDateTime::from_unix_timestamp(1_700_000_001).unwrap()
        );
        pb.seek_by(1);
        assert_eq!(
            pb.virtual_now(),
            OffsetDateTime::from_unix_timestamp(1_700_000_002).unwrap()
        );
    }

    #[test]
    fn loads_landmarks_alongside_events() {
        // Mix in two finding events; the deriver should produce two
        // landmarks even though the recording has six envelopes.
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("landmarks.signalscope-session");
        let writer = SessionWriter::create(&path, SessionHeader::new(None)).unwrap();
        for i in 1..=3u64 {
            let env = Envelope::with_time(
                EventId(i),
                OffsetDateTime::from_unix_timestamp(1_700_000_000 + i as i64).unwrap(),
                SensorId::new("gateway"),
                Event::GatewayLatency(GatewayLatencyObservation {
                    target: "192.168.1.1".into(),
                    rtt: std::time::Duration::from_millis(2),
                    reachable: true,
                    probe: "icmp".into(),
                }),
            );
            writer.record(&env).unwrap();
        }
        let active = Envelope::with_time(
            EventId(4),
            OffsetDateTime::from_unix_timestamp(1_700_000_010).unwrap(),
            SensorId::new("analysis"),
            Event::Finding(signalscope_events::CorrelationFinding {
                kind: signalscope_events::FindingKind::GatewayInstability,
                fingerprint: "x".into(),
                headline: "test active".into(),
                confidence: signalscope_events::Confidence::new(0.7),
                peak_confidence: signalscope_events::Confidence::new(0.7),
                evidence: vec![],
                lifecycle: signalscope_events::FindingLifecycle::Active,
                first_seen: OffsetDateTime::from_unix_timestamp(1_700_000_010).unwrap(),
                last_seen: OffsetDateTime::from_unix_timestamp(1_700_000_010).unwrap(),
            }),
        );
        let resolved = Envelope::with_time(
            EventId(5),
            OffsetDateTime::from_unix_timestamp(1_700_000_020).unwrap(),
            SensorId::new("analysis"),
            Event::Finding(signalscope_events::CorrelationFinding {
                kind: signalscope_events::FindingKind::GatewayInstability,
                fingerprint: "x".into(),
                headline: "test resolved".into(),
                confidence: signalscope_events::Confidence::new(0.7),
                peak_confidence: signalscope_events::Confidence::new(0.7),
                evidence: vec![],
                lifecycle: signalscope_events::FindingLifecycle::Resolved,
                first_seen: OffsetDateTime::from_unix_timestamp(1_700_000_010).unwrap(),
                last_seen: OffsetDateTime::from_unix_timestamp(1_700_000_020).unwrap(),
            }),
        );
        writer.record(&active).unwrap();
        writer.record(&resolved).unwrap();
        drop(writer);

        let pb = Playback::load(&path).unwrap();
        assert_eq!(pb.events.len(), 5);
        assert_eq!(pb.landmarks.len(), 2, "two findings = two landmarks");
    }

    #[test]
    fn landmark_navigation_jumps_between_landmark_event_indices() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("nav.signalscope-session");
        let writer = SessionWriter::create(&path, SessionHeader::new(None)).unwrap();
        // Filler gateway events at 0..2, finding at 3, filler at 4..6,
        // finding at 7, filler at 8..9.
        for i in 0..10u64 {
            let env = if i == 3 || i == 7 {
                Envelope::with_time(
                    EventId(i + 1),
                    OffsetDateTime::from_unix_timestamp(1_700_000_000 + i as i64).unwrap(),
                    SensorId::new("analysis"),
                    Event::Finding(signalscope_events::CorrelationFinding {
                        kind: signalscope_events::FindingKind::GatewayInstability,
                        fingerprint: format!("x{i}"),
                        headline: format!("event {i}"),
                        confidence: signalscope_events::Confidence::new(0.7),
                        peak_confidence: signalscope_events::Confidence::new(0.7),
                        evidence: vec![],
                        lifecycle: signalscope_events::FindingLifecycle::Active,
                        first_seen: OffsetDateTime::from_unix_timestamp(1_700_000_000).unwrap(),
                        last_seen: OffsetDateTime::from_unix_timestamp(1_700_000_000).unwrap(),
                    }),
                )
            } else {
                Envelope::with_time(
                    EventId(i + 1),
                    OffsetDateTime::from_unix_timestamp(1_700_000_000 + i as i64).unwrap(),
                    SensorId::new("gateway"),
                    Event::GatewayLatency(GatewayLatencyObservation {
                        target: "192.168.1.1".into(),
                        rtt: std::time::Duration::from_millis(2),
                        reachable: true,
                        probe: "icmp".into(),
                    }),
                )
            };
            writer.record(&env).unwrap();
        }
        drop(writer);

        let mut pb = Playback::load(&path).unwrap();
        assert_eq!(pb.landmarks.len(), 2);
        let lm0 = pb.landmarks[0].event_index;
        let lm1 = pb.landmarks[1].event_index;
        assert_eq!(lm0, 3);
        assert_eq!(lm1, 7);

        // Start at end (playhead = 9). 'prev' should jump back to lm1.
        assert!(pb.seek_to_prev_landmark());
        assert_eq!(pb.playhead, lm1);
        // Again back to lm0.
        assert!(pb.seek_to_prev_landmark());
        assert_eq!(pb.playhead, lm0);
        // No earlier landmark — no-op.
        assert!(!pb.seek_to_prev_landmark());
        assert_eq!(pb.playhead, lm0);
        // Forward.
        assert!(pb.seek_to_next_landmark());
        assert_eq!(pb.playhead, lm1);
        // No later landmark — no-op.
        assert!(!pb.seek_to_next_landmark());
        assert_eq!(pb.playhead, lm1);
    }

    #[test]
    fn current_landmark_index_reflects_playhead_position() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("current.signalscope-session");
        let writer = SessionWriter::create(&path, SessionHeader::new(None)).unwrap();
        // Three findings at event indices 0, 2, 4.
        for i in 0..6u64 {
            let env = if i % 2 == 0 {
                Envelope::with_time(
                    EventId(i + 1),
                    OffsetDateTime::from_unix_timestamp(1_700_000_000 + i as i64).unwrap(),
                    SensorId::new("analysis"),
                    Event::Finding(signalscope_events::CorrelationFinding {
                        kind: signalscope_events::FindingKind::GatewayInstability,
                        fingerprint: format!("f{i}"),
                        headline: "x".into(),
                        confidence: signalscope_events::Confidence::new(0.7),
                        peak_confidence: signalscope_events::Confidence::new(0.7),
                        evidence: vec![],
                        lifecycle: signalscope_events::FindingLifecycle::Active,
                        first_seen: OffsetDateTime::from_unix_timestamp(1_700_000_000).unwrap(),
                        last_seen: OffsetDateTime::from_unix_timestamp(1_700_000_000).unwrap(),
                    }),
                )
            } else {
                Envelope::with_time(
                    EventId(i + 1),
                    OffsetDateTime::from_unix_timestamp(1_700_000_000 + i as i64).unwrap(),
                    SensorId::new("gateway"),
                    Event::GatewayLatency(GatewayLatencyObservation {
                        target: "192.168.1.1".into(),
                        rtt: std::time::Duration::from_millis(2),
                        reachable: true,
                        probe: "icmp".into(),
                    }),
                )
            };
            writer.record(&env).unwrap();
        }
        drop(writer);

        let mut pb = Playback::load(&path).unwrap();
        assert_eq!(pb.landmarks.len(), 3);
        pb.seek_to_start();
        assert_eq!(pb.current_landmark_index(), Some(0));
        // Move past index 0's event (still landmark 0 is the most
        // recent one crossed until we pass lm 1).
        pb.seek_by(1);
        assert_eq!(pb.current_landmark_index(), Some(0));
        pb.seek_by(1); // now at event_index 2 = landmark 1
        assert_eq!(pb.current_landmark_index(), Some(1));
        pb.seek_to_end();
        assert_eq!(pb.current_landmark_index(), Some(2));
    }

    #[test]
    fn elapsed_and_total_span_are_consistent() {
        let (_dir, path) = write_session_with_n_events(4);
        let mut pb = Playback::load(&path).unwrap();
        pb.seek_to_start();
        assert_eq!(pb.elapsed(), std::time::Duration::from_secs(0));
        pb.seek_to_end();
        assert_eq!(pb.elapsed(), pb.total_span());
        // 4 events at 1-second cadence = 3 s between first and last.
        assert_eq!(pb.total_span(), std::time::Duration::from_secs(3));
    }
}
