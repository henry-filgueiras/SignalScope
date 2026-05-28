//! Append-only observability session recorder.
//!
//! A *session* is a single SignalScope run preserved as a portable, replayable
//! artifact: a stream of newline-delimited JSON. The first line is a
//! [`SessionHeader`]; every subsequent line is one [`SessionRow::Envelope`]
//! carrying a published bus envelope verbatim.
//!
//! Design goals (in priority order):
//!
//! 1. **Append-only.** A session is a temporal recording. Rows are never
//!    rewritten and never reordered. `kill -9` may lose the tail; it must
//!    never corrupt earlier rows.
//! 2. **Inspectable.** `tail -f`, `jq`, `wc -l` should all just work. This is
//!    intentionally not a database. No SQLite, no binary framing, no
//!    compression — yet.
//! 3. **Versioned.** A header line carries `format_version`. Readers refuse
//!    files newer than they understand instead of silently misinterpreting
//!    them. Writers may add header fields freely; readers tolerate unknown
//!    fields.
//! 4. **Semantically faithful.** Whatever the bus carries gets recorded, in
//!    the same order, with the same timestamps. Lifecycle transitions,
//!    observation confidence, sensor-health distinctions, monotonic event
//!    ids — all preserved. No flattening to derived summaries.

use std::fs::{File, OpenOptions};
use std::io::{self, BufRead, BufReader, BufWriter, Write};
use std::path::{Path, PathBuf};
use std::sync::Arc;

use parking_lot::Mutex;
use serde::{Deserialize, Serialize};
use time::OffsetDateTime;
use tokio::task::JoinHandle;

use signalscope_events::Envelope;

use crate::bus::EventBus;

/// Bump when the on-disk schema changes in a way readers cannot tolerate
/// (e.g. a row shape changes incompatibly). Adding new optional fields does
/// not require a bump.
pub const SESSION_FORMAT_VERSION: u32 = 1;

/// Discriminator written into every header so a stray JSONL file can be
/// identified as a SignalScope session at a glance.
pub const SESSION_KIND: &str = "signalscope-session";

/// Header line written once at the top of every session file.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SessionHeader {
    pub kind: String,
    pub format_version: u32,
    pub created_at: OffsetDateTime,
    pub tool_version: String,
    /// Free-form operator label captured at recording time. Useful for
    /// post-hoc filing (`"hotel-wifi-2026-05-28"`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub label: Option<String>,
}

impl SessionHeader {
    pub fn new(label: Option<String>) -> Self {
        Self {
            kind: SESSION_KIND.into(),
            format_version: SESSION_FORMAT_VERSION,
            created_at: OffsetDateTime::now_utc(),
            tool_version: env!("CARGO_PKG_VERSION").into(),
            label,
        }
    }
}

/// What a session line carries on disk. The tagged-enum framing lets future
/// row kinds (replay markers, operator notes) land without breaking the
/// existing schema — readers only need to match the variants they care about.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "row")]
pub enum SessionRow {
    #[serde(rename = "header")]
    Header(SessionHeader),
    #[serde(rename = "envelope")]
    Envelope(Envelope),
}

#[derive(Debug)]
struct WriterInner {
    out: BufWriter<File>,
    path: PathBuf,
}

/// Handle to an open session file. Cloneable so multiple producers can share
/// one recording; writes are serialized internally so the on-disk order
/// matches publication order even under concurrent callers.
#[derive(Debug, Clone)]
pub struct SessionWriter {
    inner: Arc<Mutex<WriterInner>>,
}

impl SessionWriter {
    /// Create or truncate a session file at `path` and write the header.
    /// Parent directories are created if missing.
    pub fn create(path: impl AsRef<Path>, header: SessionHeader) -> io::Result<Self> {
        let path = path.as_ref().to_path_buf();
        if let Some(parent) = path.parent() {
            if !parent.as_os_str().is_empty() {
                std::fs::create_dir_all(parent)?;
            }
        }
        // Truncate: a session is a single run. Appending two runs into one
        // file would silently violate temporal monotonicity.
        let file = OpenOptions::new()
            .create(true)
            .write(true)
            .truncate(true)
            .open(&path)?;
        let mut out = BufWriter::new(file);
        write_row(&mut out, &SessionRow::Header(header))?;
        out.flush()?;
        Ok(Self {
            inner: Arc::new(Mutex::new(WriterInner { out, path })),
        })
    }

    /// Append one envelope to the session. Flushes per row: the recording
    /// cadence is on the order of seconds, and an abrupt termination should
    /// lose at most the most recent observation, never the tail of the run.
    pub fn record(&self, envelope: &Envelope) -> io::Result<()> {
        let mut guard = self.inner.lock();
        write_row(&mut guard.out, &SessionRow::Envelope(envelope.clone()))?;
        guard.out.flush()
    }

    pub fn path(&self) -> PathBuf {
        self.inner.lock().path.clone()
    }
}

fn write_row<W: Write>(out: &mut W, row: &SessionRow) -> io::Result<()> {
    serde_json::to_writer(&mut *out, row).map_err(io::Error::other)?;
    out.write_all(b"\n")
}

/// Subscribe to `bus` and persist every envelope (backlog included, in bus
/// order) into `writer`. The returned task exits when the bus is dropped or
/// the writer errors; abort it on graceful shutdown.
pub fn spawn_recorder(bus: Arc<EventBus>, writer: SessionWriter) -> JoinHandle<()> {
    let mut sub = bus.subscribe();
    let backlog = bus.recent();
    tokio::spawn(async move {
        for env in backlog {
            if let Err(e) = writer.record(&env) {
                tracing::warn!(error = %e, "session writer failed; recorder exiting");
                return;
            }
        }
        while let Some(env) = sub.recv().await {
            if let Err(e) = writer.record(&env) {
                tracing::warn!(error = %e, "session writer failed; recorder exiting");
                return;
            }
        }
    })
}

/// Streaming reader for a previously-written session file. Yields envelopes
/// in file order. The header is consumed on construction so callers can
/// reject incompatible recordings before doing any work.
#[derive(Debug)]
pub struct SessionReader {
    header: SessionHeader,
    lines: std::io::Lines<BufReader<File>>,
    line_no: usize,
}

#[derive(Debug, thiserror::Error)]
pub enum SessionReadError {
    #[error("io error: {0}")]
    Io(#[from] io::Error),
    #[error("session file is empty")]
    Empty,
    #[error("first line is not a session header")]
    MissingHeader,
    #[error("file is kind {found:?}, expected {expected:?}")]
    WrongKind { found: String, expected: &'static str },
    #[error("session file format version {found} is newer than supported ({supported})")]
    UnsupportedVersion { found: u32, supported: u32 },
    #[error("malformed json on line {line}: {source}")]
    BadJson {
        line: usize,
        #[source]
        source: serde_json::Error,
    },
    #[error("unexpected header row at line {line} (only the first line may be a header)")]
    DuplicateHeader { line: usize },
}

impl SessionReader {
    pub fn open(path: impl AsRef<Path>) -> Result<Self, SessionReadError> {
        let file = File::open(path)?;
        let mut lines = BufReader::new(file).lines();

        let first = loop {
            match lines.next() {
                Some(Ok(s)) if s.trim().is_empty() => continue,
                Some(Ok(s)) => break s,
                Some(Err(e)) => return Err(SessionReadError::Io(e)),
                None => return Err(SessionReadError::Empty),
            }
        };

        let row: SessionRow = serde_json::from_str(&first)
            .map_err(|source| SessionReadError::BadJson { line: 1, source })?;
        let header = match row {
            SessionRow::Header(h) => h,
            SessionRow::Envelope(_) => return Err(SessionReadError::MissingHeader),
        };
        if header.kind != SESSION_KIND {
            return Err(SessionReadError::WrongKind {
                found: header.kind,
                expected: SESSION_KIND,
            });
        }
        if header.format_version > SESSION_FORMAT_VERSION {
            return Err(SessionReadError::UnsupportedVersion {
                found: header.format_version,
                supported: SESSION_FORMAT_VERSION,
            });
        }
        Ok(Self {
            header,
            lines,
            line_no: 1,
        })
    }

    pub fn header(&self) -> &SessionHeader {
        &self.header
    }
}

impl Iterator for SessionReader {
    type Item = Result<Envelope, SessionReadError>;

    fn next(&mut self) -> Option<Self::Item> {
        loop {
            let raw = self.lines.next()?;
            self.line_no += 1;
            let line = match raw {
                Ok(s) => s,
                Err(e) => return Some(Err(SessionReadError::Io(e))),
            };
            if line.trim().is_empty() {
                continue;
            }
            let row: SessionRow = match serde_json::from_str(&line) {
                Ok(r) => r,
                Err(source) => {
                    return Some(Err(SessionReadError::BadJson {
                        line: self.line_no,
                        source,
                    }))
                }
            };
            return Some(match row {
                SessionRow::Envelope(e) => Ok(e),
                SessionRow::Header(_) => Err(SessionReadError::DuplicateHeader {
                    line: self.line_no,
                }),
            });
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use signalscope_events::{
        DnsLatencyObservation, Event, EventId, GatewayLatencyObservation, SensorId,
    };
    use std::time::Duration;

    fn sample_envelope(id: u64) -> Envelope {
        Envelope::with_time(
            EventId(id),
            OffsetDateTime::from_unix_timestamp(1_700_000_000 + id as i64).unwrap(),
            SensorId::new("gateway"),
            Event::GatewayLatency(GatewayLatencyObservation {
                target: "192.168.1.1".into(),
                rtt: Duration::from_millis(7),
                reachable: true,
                probe: "icmp".into(),
            }),
        )
    }

    #[test]
    fn round_trip_preserves_envelopes_in_order() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("trip.signalscope-session");
        let writer = SessionWriter::create(&path, SessionHeader::new(Some("test".into()))).unwrap();
        let envelopes: Vec<_> = (1..=5).map(sample_envelope).collect();
        for env in &envelopes {
            writer.record(env).unwrap();
        }
        drop(writer);

        let reader = SessionReader::open(&path).unwrap();
        assert_eq!(reader.header().kind, SESSION_KIND);
        assert_eq!(reader.header().format_version, SESSION_FORMAT_VERSION);
        assert_eq!(reader.header().label.as_deref(), Some("test"));

        let read: Vec<Envelope> = reader.map(|r| r.unwrap()).collect();
        assert_eq!(read.len(), envelopes.len());
        for (a, b) in read.iter().zip(envelopes.iter()) {
            assert_eq!(a.id, b.id);
            assert_eq!(a.at, b.at);
            assert_eq!(a.source.as_str(), b.source.as_str());
        }
    }

    #[test]
    fn rejects_file_with_no_header() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("bad.session");
        let env = sample_envelope(1);
        let line = serde_json::to_string(&SessionRow::Envelope(env)).unwrap();
        std::fs::write(&path, format!("{line}\n")).unwrap();

        match SessionReader::open(&path) {
            Err(SessionReadError::MissingHeader) => {}
            other => panic!("expected MissingHeader, got {other:?}"),
        }
    }

    #[test]
    fn rejects_future_format_version() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("future.session");
        let mut header = SessionHeader::new(None);
        header.format_version = SESSION_FORMAT_VERSION + 1;
        let line = serde_json::to_string(&SessionRow::Header(header)).unwrap();
        std::fs::write(&path, format!("{line}\n")).unwrap();

        match SessionReader::open(&path) {
            Err(SessionReadError::UnsupportedVersion { found, supported }) => {
                assert_eq!(found, SESSION_FORMAT_VERSION + 1);
                assert_eq!(supported, SESSION_FORMAT_VERSION);
            }
            other => panic!("expected UnsupportedVersion, got {other:?}"),
        }
    }

    #[test]
    fn rejects_wrong_kind() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("wrongkind.session");
        let header = SessionHeader {
            kind: "something-else".into(),
            format_version: SESSION_FORMAT_VERSION,
            created_at: OffsetDateTime::now_utc(),
            tool_version: "0.0.0".into(),
            label: None,
        };
        let line = serde_json::to_string(&SessionRow::Header(header)).unwrap();
        std::fs::write(&path, format!("{line}\n")).unwrap();

        match SessionReader::open(&path) {
            Err(SessionReadError::WrongKind { found, expected }) => {
                assert_eq!(found, "something-else");
                assert_eq!(expected, SESSION_KIND);
            }
            other => panic!("expected WrongKind, got {other:?}"),
        }
    }

    #[test]
    fn round_trip_carries_dns_event_intact() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("dns.session");
        let writer = SessionWriter::create(&path, SessionHeader::new(None)).unwrap();
        let env = Envelope::with_time(
            EventId(99),
            OffsetDateTime::from_unix_timestamp(1_700_000_500).unwrap(),
            SensorId::new("dns"),
            Event::DnsLatency(DnsLatencyObservation {
                resolver: "1.1.1.1".into(),
                query: "example.com".into(),
                rtt: Duration::from_millis(12),
                answered: true,
                error: None,
            }),
        );
        writer.record(&env).unwrap();
        drop(writer);

        let mut reader = SessionReader::open(&path).unwrap();
        let read = reader.next().unwrap().unwrap();
        match &read.event {
            Event::DnsLatency(o) => {
                assert_eq!(o.resolver, "1.1.1.1");
                assert_eq!(o.query, "example.com");
                assert_eq!(o.rtt, Duration::from_millis(12));
                assert!(o.answered);
            }
            other => panic!("wrong event variant: {other:?}"),
        }
        assert!(reader.next().is_none());
    }
}
