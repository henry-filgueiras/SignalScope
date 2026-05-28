//! Canonical observability session recording.
//!
//! A *session* is a single SignalScope run preserved as a portable
//! artifact: a stream of newline-delimited JSON. The first line is a
//! [`SessionHeader`]; every subsequent line is one [`SessionRow::Envelope`]
//! carrying a published bus envelope verbatim.
//!
//! # On-disk shape (v2)
//!
//! ```jsonc
//! {"row":"header","kind":"signalscope-session","format_version":2,
//!  "created_at":"2026-05-28T17:16:50Z","tool_version":"0.1.0","label":"…"}
//! {"row":"envelope","id":1,"at":"2026-05-28T17:16:51Z","source":"wifi",
//!  "event":{"type":"Wifi", … }}
//! {"row":"envelope","id":2,"at":"2026-05-28T17:16:51Z","source":"gateway",
//!  "event":{"type":"GatewayLatency", … }}
//! …
//! ```
//!
//! Two row variants: `header` (exactly once, line 1) and `envelope`
//! (every other line). `SessionRow` is a tagged enum so future row
//! kinds (replay markers, operator notes) can land without breaking
//! existing readers.
//!
//! # Design goals (in priority order)
//!
//! 1. **Append-only.** A session is a temporal recording. Rows are
//!    never rewritten and never reordered. `kill -9` may lose the
//!    tail; it must never corrupt earlier rows.
//! 2. **Inspectable.** `tail -f`, `jq`, `wc -l` all just work. ISO-8601
//!    timestamps survive `jq -r '.at'`. Not a database; not binary; not
//!    compressed.
//! 3. **Versioned.** A header line carries `format_version`. Readers
//!    refuse files they don't understand instead of silently
//!    misinterpreting them. Writers may add header fields freely;
//!    readers tolerate unknown fields.
//! 4. **Semantically faithful.** Whatever the bus carries gets
//!    recorded, in publication order, with wall-clock timestamps.
//!    Observations, scans, gateway/DNS latency, interface counters,
//!    interface state changes, roams, correlation findings (including
//!    `Active`/`Escalating`/`Recovering`/`Resolved` lifecycle edges),
//!    and sensor-health events all round-trip exactly.
//!
//! # Version history
//!
//! - **v1** — `OffsetDateTime` serialized as the default `time` crate
//!   tuple form. Internal only; never shipped.
//! - **v2** — `created_at` and envelope `at` switched to RFC 3339
//!   strings for canonical inspectability. **Current.**

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
/// (e.g. a row shape changes incompatibly). Adding new optional fields
/// to the header or new variants to `Event` does NOT require a bump —
/// the reader tolerates unknown header fields and serde's tagged enum
/// rejects unknown variants only at the row level (the rest of the
/// session still reads).
pub const SESSION_FORMAT_VERSION: u32 = 2;

/// Oldest format version this reader will still accept. When the format
/// makes a backwards-incompatible jump (like v1→v2's timestamp swap), set
/// this to the new minimum and refuse anything older with a clear error
/// instead of letting serde fail mysteriously deep in a row.
pub const SESSION_MIN_READABLE_VERSION: u32 = 2;

/// Discriminator written into every header so a stray JSONL file can be
/// identified as a SignalScope session at a glance.
pub const SESSION_KIND: &str = "signalscope-session";

/// Header line written once at the top of every session file.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SessionHeader {
    pub kind: String,
    pub format_version: u32,
    #[serde(with = "time::serde::rfc3339")]
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
    #[error(
        "session file format version {found} is newer than supported ({supported})"
    )]
    UnsupportedNewerVersion { found: u32, supported: u32 },
    #[error(
        "session file format version {found} is older than the minimum supported ({minimum})"
    )]
    UnsupportedOlderVersion { found: u32, minimum: u32 },
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

        // Two-phase header parse so kind/version errors win over JSON
        // shape errors. Otherwise a legacy file with v1 tuple timestamps
        // surfaces as "invalid type: sequence" instead of the actual
        // "format version is too old" story.
        let raw: serde_json::Value = serde_json::from_str(&first)
            .map_err(|source| SessionReadError::BadJson { line: 1, source })?;
        match raw.get("row").and_then(|v| v.as_str()) {
            Some("header") => {}
            Some("envelope") => return Err(SessionReadError::MissingHeader),
            _ => return Err(SessionReadError::MissingHeader),
        }
        if let Some(kind) = raw.get("kind").and_then(|v| v.as_str()) {
            if kind != SESSION_KIND {
                return Err(SessionReadError::WrongKind {
                    found: kind.to_string(),
                    expected: SESSION_KIND,
                });
            }
        }
        if let Some(v) = raw.get("format_version").and_then(|v| v.as_u64()) {
            let v = v as u32;
            if v > SESSION_FORMAT_VERSION {
                return Err(SessionReadError::UnsupportedNewerVersion {
                    found: v,
                    supported: SESSION_FORMAT_VERSION,
                });
            }
            if v < SESSION_MIN_READABLE_VERSION {
                return Err(SessionReadError::UnsupportedOlderVersion {
                    found: v,
                    minimum: SESSION_MIN_READABLE_VERSION,
                });
            }
        }

        // kind + version validated; now do the strict shape parse.
        let row: SessionRow = serde_json::from_value(raw)
            .map_err(|source| SessionReadError::BadJson { line: 1, source })?;
        let header = match row {
            SessionRow::Header(h) => h,
            SessionRow::Envelope(_) => return Err(SessionReadError::MissingHeader),
        };
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

/// One-glance summary of a recorded session — enough to confirm a
/// handed-off file is what the operator thinks it is, without loading
/// the full envelope stream into memory anywhere downstream.
#[derive(Debug, Clone, Default)]
pub struct SessionStats {
    pub envelope_count: u64,
    pub first_at: Option<OffsetDateTime>,
    pub last_at: Option<OffsetDateTime>,
    pub wifi: u64,
    pub scan: u64,
    pub gateway: u64,
    pub dns: u64,
    pub iface: u64,
    pub iface_state: u64,
    pub roam: u64,
    pub findings: u64,
    pub health: u64,
}

impl SessionStats {
    /// Wall-clock span the recording covers. `None` until at least one
    /// envelope has been observed.
    pub fn duration(&self) -> Option<std::time::Duration> {
        let first = self.first_at?;
        let last = self.last_at?;
        let secs = (last - first).whole_seconds().max(0);
        Some(std::time::Duration::from_secs(secs as u64))
    }

    fn observe(&mut self, env: &Envelope) {
        self.envelope_count += 1;
        if self.first_at.is_none() {
            self.first_at = Some(env.at);
        }
        self.last_at = Some(env.at);
        use signalscope_events::Event;
        match &env.event {
            Event::Wifi(_) => self.wifi += 1,
            Event::Scan(_) => self.scan += 1,
            Event::GatewayLatency(_) => self.gateway += 1,
            Event::DnsLatency(_) => self.dns += 1,
            Event::InterfaceCounters(_) => self.iface += 1,
            Event::InterfaceStateChanged(_) => self.iface_state += 1,
            Event::RoamDetected(_) => self.roam += 1,
            Event::Finding(_) => self.findings += 1,
            Event::SensorHealth(_) => self.health += 1,
        }
    }
}

/// Read a session end-to-end, returning the header and a summary of
/// the envelopes inside. Cheap — touches every row but holds nothing
/// in memory. Stops at the first parse error and surfaces it.
pub fn summarize(path: impl AsRef<Path>) -> Result<(SessionHeader, SessionStats), SessionReadError> {
    let mut reader = SessionReader::open(path)?;
    let mut stats = SessionStats::default();
    for envelope in &mut reader {
        let envelope = envelope?;
        stats.observe(&envelope);
    }
    Ok((reader.header.clone(), stats))
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
        Confidence, CorrelationFinding, DnsLatencyObservation, Event, EventId, FindingKind,
        FindingLifecycle, GatewayLatencyObservation, InterfaceCountersObservation, ScanResult,
        SensorHealth, SensorId, SensorState,
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
    fn timestamps_serialize_as_rfc3339_strings() {
        // The canonical recording shape MUST emit ISO-8601 timestamps —
        // operators inspecting a session with `jq -r '.at'` should get
        // a date string, not a numeric tuple. This guard exists so a
        // future serde-attribute regression on `Envelope::at` or
        // `SessionHeader::created_at` is caught immediately.
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("rfc3339.session");
        let writer = SessionWriter::create(&path, SessionHeader::new(None)).unwrap();
        writer.record(&sample_envelope(1)).unwrap();
        drop(writer);

        let raw = std::fs::read_to_string(&path).unwrap();
        let mut lines = raw.lines();
        let header_line = lines.next().unwrap();
        let env_line = lines.next().unwrap();

        let header_json: serde_json::Value = serde_json::from_str(header_line).unwrap();
        assert!(
            header_json["created_at"].is_string(),
            "created_at should be a string, got {}",
            header_json["created_at"]
        );

        let env_json: serde_json::Value = serde_json::from_str(env_line).unwrap();
        assert!(
            env_json["at"].is_string(),
            "envelope.at should be a string, got {}",
            env_json["at"]
        );
        let at = env_json["at"].as_str().unwrap();
        // RFC 3339 shape sanity-check — there's always a `T` separator
        // and either `Z` or a `+`/`-` offset.
        assert!(at.contains('T'), "no T separator in {at:?}");
        assert!(
            at.ends_with('Z') || at.contains('+') || at.matches('-').count() > 2,
            "no zone marker in {at:?}"
        );
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
            Err(SessionReadError::UnsupportedNewerVersion { found, supported }) => {
                assert_eq!(found, SESSION_FORMAT_VERSION + 1);
                assert_eq!(supported, SESSION_FORMAT_VERSION);
            }
            other => panic!("expected UnsupportedNewerVersion, got {other:?}"),
        }
    }

    #[test]
    fn rejects_older_than_minimum_format_version() {
        // A pre-v2 file (legacy tuple timestamps) must be refused with a
        // clear error rather than letting serde explode on `created_at`.
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("ancient.session");
        // We can't use SessionHeader::new — its created_at is RFC 3339.
        // Hand-craft a v1-shaped header by JSON value.
        let v1 = serde_json::json!({
            "row": "header",
            "kind": SESSION_KIND,
            "format_version": SESSION_MIN_READABLE_VERSION - 1,
            "created_at": [2026, 148, 17, 16, 50, 0, 0, 0, 0],
            "tool_version": "0.0.0",
        });
        std::fs::write(&path, format!("{v1}\n")).unwrap();

        match SessionReader::open(&path) {
            Err(SessionReadError::UnsupportedOlderVersion { found, minimum }) => {
                assert_eq!(found, SESSION_MIN_READABLE_VERSION - 1);
                assert_eq!(minimum, SESSION_MIN_READABLE_VERSION);
            }
            other => panic!("expected UnsupportedOlderVersion, got {other:?}"),
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

    #[test]
    fn round_trip_handles_a_mixed_event_stream() {
        // Cover every event category that the bus actually carries today:
        // observation, scan, gateway, DNS, interface counters, lifecycle
        // finding, sensor health. A canonical session must round-trip
        // them all without losing fidelity.
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("mixed.session");
        let writer = SessionWriter::create(&path, SessionHeader::new(Some("mixed".into()))).unwrap();

        let envelopes = vec![
            Envelope::with_time(
                EventId(1),
                OffsetDateTime::from_unix_timestamp(1_700_000_001).unwrap(),
                SensorId::new("wifi"),
                Event::Scan(ScanResult {
                    interface: "en0".into(),
                    neighbors: vec![],
                }),
            ),
            Envelope::with_time(
                EventId(2),
                OffsetDateTime::from_unix_timestamp(1_700_000_002).unwrap(),
                SensorId::new("gateway"),
                Event::GatewayLatency(GatewayLatencyObservation {
                    target: "192.168.1.1".into(),
                    rtt: Duration::from_micros(2500),
                    reachable: true,
                    probe: "icmp".into(),
                }),
            ),
            Envelope::with_time(
                EventId(3),
                OffsetDateTime::from_unix_timestamp(1_700_000_003).unwrap(),
                SensorId::new("dns"),
                Event::DnsLatency(DnsLatencyObservation {
                    resolver: "1.1.1.1".into(),
                    query: "example.com".into(),
                    rtt: Duration::from_millis(15),
                    answered: false,
                    error: Some("timeout".into()),
                }),
            ),
            Envelope::with_time(
                EventId(4),
                OffsetDateTime::from_unix_timestamp(1_700_000_004).unwrap(),
                SensorId::new("iface"),
                Event::InterfaceCounters(InterfaceCountersObservation {
                    interface: "en0".into(),
                    rx_bytes_total: 1_000_000,
                    tx_bytes_total: 200_000,
                    rx_packets_total: 5000,
                    tx_packets_total: 1000,
                    rx_errors_total: 0,
                    tx_errors_total: 0,
                    rx_dropped_total: None,
                    tx_dropped_total: None,
                    retry_count: None,
                }),
            ),
            Envelope::with_time(
                EventId(5),
                OffsetDateTime::from_unix_timestamp(1_700_000_005).unwrap(),
                SensorId::new("analysis"),
                Event::Finding(CorrelationFinding {
                    kind: FindingKind::GatewayInstability,
                    fingerprint: "gw_instability:192.168.1.1".into(),
                    headline: "gateway flapping".into(),
                    confidence: Confidence::new(0.7),
                    peak_confidence: Confidence::new(0.7),
                    evidence: vec!["loss 18%".into(), "p95 230 ms".into()],
                    lifecycle: FindingLifecycle::Active,
                    first_seen: OffsetDateTime::from_unix_timestamp(1_700_000_004).unwrap(),
                    last_seen: OffsetDateTime::from_unix_timestamp(1_700_000_005).unwrap(),
                }),
            ),
            Envelope::with_time(
                EventId(6),
                OffsetDateTime::from_unix_timestamp(1_700_000_006).unwrap(),
                SensorId::new("wifi"),
                Event::SensorHealth(SensorHealth {
                    sensor: SensorId::new("wifi"),
                    state: SensorState::Stale,
                    backend: Some("system_profiler".into()),
                    detail: Some("backend timed out".into()),
                }),
            ),
        ];

        for env in &envelopes {
            writer.record(env).unwrap();
        }
        drop(writer);

        let reader = SessionReader::open(&path).unwrap();
        let read: Vec<Envelope> = reader.map(|r| r.unwrap()).collect();
        assert_eq!(read.len(), envelopes.len());

        for (a, b) in read.iter().zip(envelopes.iter()) {
            assert_eq!(a.id, b.id);
            assert_eq!(a.at, b.at);
            assert_eq!(a.source.as_str(), b.source.as_str());
        }

        // Spot-check that the more complex variants made it through.
        match &read[4].event {
            Event::Finding(f) => {
                assert_eq!(f.kind, FindingKind::GatewayInstability);
                assert_eq!(f.fingerprint, "gw_instability:192.168.1.1");
                assert_eq!(f.lifecycle, FindingLifecycle::Active);
                assert!((f.confidence.value() - 0.7).abs() < 1e-6);
                assert_eq!(f.evidence.len(), 2);
            }
            other => panic!("expected Finding, got {other:?}"),
        }
        match &read[5].event {
            Event::SensorHealth(h) => {
                assert_eq!(h.state, SensorState::Stale);
                assert_eq!(h.backend.as_deref(), Some("system_profiler"));
                assert_eq!(h.detail.as_deref(), Some("backend timed out"));
            }
            other => panic!("expected SensorHealth, got {other:?}"),
        }
    }

    #[test]
    fn malformed_line_surfaces_a_bad_json_error_with_line_number() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("malformed.session");

        // Write a valid header + valid envelope + malformed line + valid envelope.
        let writer = SessionWriter::create(&path, SessionHeader::new(None)).unwrap();
        writer.record(&sample_envelope(1)).unwrap();
        drop(writer);
        // Append two rows by hand: one garbage, one valid. We do this
        // outside the SessionWriter because the writer would refuse to
        // emit non-row JSON.
        let mut f = OpenOptions::new().append(true).open(&path).unwrap();
        writeln!(f, "{{this is not valid json").unwrap();
        let good = serde_json::to_string(&SessionRow::Envelope(sample_envelope(2))).unwrap();
        writeln!(f, "{good}").unwrap();

        let mut reader = SessionReader::open(&path).unwrap();
        // First envelope reads cleanly.
        let first = reader.next().unwrap().unwrap();
        assert_eq!(first.id, EventId(1));
        // Second row is malformed — must surface BadJson with the right line number.
        match reader.next() {
            Some(Err(SessionReadError::BadJson { line, .. })) => {
                // Header is line 1; first envelope is line 2; malformed is line 3.
                assert_eq!(line, 3, "wrong line number reported");
            }
            other => panic!("expected BadJson, got {other:?}"),
        }
    }

    #[test]
    fn header_tolerates_unknown_fields_for_forward_compat() {
        // A future SignalScope might add metadata fields to the header.
        // Today's reader MUST tolerate them — otherwise a v2 file written
        // by a slightly newer version becomes unreadable for no good
        // reason. (`format_version` only bumps on incompatible row-shape
        // changes; additive header fields stay v2.)
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("future-fields.session");
        let now = OffsetDateTime::now_utc();
        // Use to_rfc3339 via the serde adapter format — build the
        // header by hand-mixing in a future field.
        let header = serde_json::json!({
            "row": "header",
            "kind": SESSION_KIND,
            "format_version": SESSION_FORMAT_VERSION,
            "created_at": now.format(&time::format_description::well_known::Rfc3339).unwrap(),
            "tool_version": "0.99.0",
            "label": "future",
            "hostname": "macmini.local",
            "operator": "henry",
        });
        let env = serde_json::to_string(&SessionRow::Envelope(sample_envelope(1))).unwrap();
        std::fs::write(&path, format!("{header}\n{env}\n")).unwrap();

        let reader = SessionReader::open(&path).expect("forward-compat header should parse");
        assert_eq!(reader.header().label.as_deref(), Some("future"));
        let read: Vec<Envelope> = reader.map(|r| r.unwrap()).collect();
        assert_eq!(read.len(), 1);
    }
}
