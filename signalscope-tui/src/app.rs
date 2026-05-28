//! Top-level TUI state and event loop.

use std::collections::{HashMap, VecDeque};
use std::io;
use std::sync::Arc;
use std::time::{Duration, Instant};

use anyhow::Result;
use crossterm::event::{
    DisableMouseCapture, EnableMouseCapture, Event as CtEvent, EventStream, KeyCode, KeyEventKind,
    KeyModifiers,
};
use crossterm::execute;
use crossterm::terminal::{
    disable_raw_mode, enable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen,
};
use futures::StreamExt;
use ratatui::backend::CrosstermBackend;
use ratatui::Terminal;
use signalscope_analysis::{InterfaceThroughputWindow, Throughput};
use signalscope_core::EventBus;
use signalscope_events::{
    Bssid, CorrelationFinding, DnsLatencyObservation, Envelope, Event, FindingLifecycle,
    GatewayLatencyObservation, InterfaceCountersObservation, ScanResult, SensorHealth, SensorId,
    SensorState, Ssid, WifiObservation,
};
use tokio::time::interval;
use tracing::warn;

use crate::ui;

const GATEWAY_HISTORY: usize = 240;
const DNS_HISTORY: usize = 240;
const EVENT_FEED_LIMIT: usize = 200;
const SIGNAL_HISTORY: usize = 90;
/// Throughput is derived from successive counter snapshots. Mirrors the
/// analysis crate's `THROUGHPUT_WINDOW` so the dashboard and the future
/// finding rules speak about the same rolling rate.
const THROUGHPUT_WINDOW: Duration = Duration::from_secs(15);

pub async fn run(bus: Arc<EventBus>) -> Result<()> {
    let mut state = AppState::new();
    // Seed from backlog so the UI is populated immediately on startup.
    for env in bus.recent() {
        state.ingest(&env);
    }

    let mut sub = bus.subscribe();

    enable_raw_mode()?;
    let mut stdout = io::stdout();
    execute!(stdout, EnterAlternateScreen, EnableMouseCapture)?;
    let backend = CrosstermBackend::new(stdout);
    let mut terminal = Terminal::new(backend)?;

    let result = main_loop(&mut terminal, &mut state, &mut sub).await;

    // Always tear the terminal down, even on error.
    disable_raw_mode().ok();
    execute!(
        terminal.backend_mut(),
        LeaveAlternateScreen,
        DisableMouseCapture
    )
    .ok();
    terminal.show_cursor().ok();

    result
}

async fn main_loop<B>(
    terminal: &mut Terminal<B>,
    state: &mut AppState,
    sub: &mut signalscope_core::Subscription,
) -> Result<()>
where
    B: ratatui::backend::Backend,
{
    let mut term_events = EventStream::new();
    let mut ticker = interval(Duration::from_millis(250));
    let mut dirty = true;

    loop {
        if dirty {
            terminal.draw(|f| ui::render(f, state))?;
            dirty = false;
        }

        tokio::select! {
            biased;
            maybe_env = sub.recv() => {
                match maybe_env {
                    Some(env) => {
                        state.ingest(&env);
                        dirty = true;
                    }
                    None => {
                        warn!("event bus closed; exiting");
                        break;
                    }
                }
            }
            maybe_input = term_events.next() => {
                match maybe_input {
                    Some(Ok(ev)) => {
                        if handle_input(ev, state) == InputOutcome::Quit {
                            break;
                        }
                        dirty = true;
                    }
                    Some(Err(e)) => {
                        warn!(error = %e, "terminal input error");
                    }
                    None => break,
                }
            }
            _ = ticker.tick() => {
                // Periodic redraw for "alive" sparklines + clock.
                dirty = true;
            }
            _ = tokio::signal::ctrl_c() => {
                break;
            }
        }
    }

    Ok(())
}

#[derive(Debug, PartialEq, Eq)]
enum InputOutcome {
    Continue,
    Quit,
}

fn handle_input(ev: CtEvent, state: &mut AppState) -> InputOutcome {
    if let CtEvent::Key(k) = ev {
        if k.kind != KeyEventKind::Press {
            return InputOutcome::Continue;
        }
        match (k.code, k.modifiers) {
            (KeyCode::Char('q') | KeyCode::Esc, _) => return InputOutcome::Quit,
            (KeyCode::Char('c'), KeyModifiers::CONTROL) => return InputOutcome::Quit,
            (KeyCode::Char('?') | KeyCode::Char('h'), _) => {
                state.show_help = !state.show_help;
            }
            (KeyCode::Char('d'), _) => {
                state.show_neighbor_detail = !state.show_neighbor_detail;
            }
            (KeyCode::Char('f'), _) => {
                state.focus = state.focus.next();
            }
            (KeyCode::Tab, _) => {
                state.focus = state.focus.next();
            }
            _ => {}
        }
    }
    InputOutcome::Continue
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Focus {
    Overview,
    Neighbors,
    Findings,
}

impl Focus {
    fn next(self) -> Self {
        match self {
            Focus::Overview => Focus::Neighbors,
            Focus::Neighbors => Focus::Findings,
            Focus::Findings => Focus::Overview,
        }
    }
}

/// A single connected-link RSSI reading retained for the local sparkline
/// and for the "Δ over 60s" callout. Kept in the TUI rather than read
/// back from the analysis crate to keep concerns separate — the engine's
/// trend windows feed the findings panel; this buffer feeds the visual.
#[derive(Debug, Clone)]
pub struct SignalSample {
    pub at: time::OffsetDateTime,
    pub rssi_dbm: i32,
}

#[derive(Debug)]
pub struct AppState {
    pub started_at: Instant,
    pub latest_wifi: Option<WifiObservation>,
    pub latest_scan: Option<ScanResult>,
    pub gateway_history: VecDeque<GatewayLatencyObservation>,
    pub dns_history: VecDeque<DnsLatencyObservation>,
    /// Currently-active findings keyed by their stable fingerprint. The
    /// engine retires findings by emitting Resolved; we drop them from
    /// this map at that point so the panel stays calm.
    pub findings: HashMap<String, CorrelationFinding>,
    pub sensor_health: HashMap<SensorId, SensorHealth>,
    pub event_feed: VecDeque<FeedItem>,
    /// Identity of the currently-associated network (SSID, BSSID). Used
    /// to detect identity changes that should reset the longitudinal
    /// "connected for" counter.
    pub connected_identity: (Option<Ssid>, Option<Bssid>),
    /// Wall-clock time we first observed the current association. Reset
    /// when the identity changes; cleared when the sensor reports that
    /// Wi-Fi is off / unavailable.
    pub connected_since: Option<time::OffsetDateTime>,
    /// Rolling RSSI samples for the connected-link sparkline and Δ
    /// callout. Caps at `SIGNAL_HISTORY` entries (~15 min at 10 s cadence).
    pub signal_history: VecDeque<SignalSample>,
    /// Most recent interface counter snapshot. Kept verbatim so the
    /// dashboard can show absolute totals next to the derived rate.
    pub latest_counters: Option<InterfaceCountersObservation>,
    /// Rolling throughput derivation. Shares the math with the analysis
    /// crate so future throughput rules and the dashboard agree on what
    /// "now" means.
    pub throughput: InterfaceThroughputWindow,
    pub focus: Focus,
    pub show_help: bool,
    /// When true, the RF environment panel renders the neighbor AP table
    /// instead of the occupancy histogram. Default is the histogram —
    /// individual identities are demoted to opt-in detail.
    pub show_neighbor_detail: bool,
}

impl AppState {
    pub fn new() -> Self {
        Self {
            started_at: Instant::now(),
            latest_wifi: None,
            latest_scan: None,
            gateway_history: VecDeque::with_capacity(GATEWAY_HISTORY),
            dns_history: VecDeque::with_capacity(DNS_HISTORY),
            findings: HashMap::new(),
            sensor_health: HashMap::new(),
            event_feed: VecDeque::with_capacity(EVENT_FEED_LIMIT),
            connected_identity: (None, None),
            connected_since: None,
            signal_history: VecDeque::with_capacity(SIGNAL_HISTORY),
            latest_counters: None,
            throughput: InterfaceThroughputWindow::new(THROUGHPUT_WINDOW),
            focus: Focus::Overview,
            show_help: false,
            show_neighbor_detail: false,
        }
    }

    pub fn ingest(&mut self, env: &Envelope) {
        match &env.event {
            Event::Wifi(o) => {
                self.record_link_longitudinal(o, env.at);
                self.latest_wifi = Some(o.clone());
            }
            Event::Scan(s) => {
                self.latest_scan = Some(s.clone());
            }
            Event::GatewayLatency(o) => {
                if self.gateway_history.len() == GATEWAY_HISTORY {
                    self.gateway_history.pop_front();
                }
                self.gateway_history.push_back(o.clone());
            }
            Event::DnsLatency(o) => {
                if self.dns_history.len() == DNS_HISTORY {
                    self.dns_history.pop_front();
                }
                self.dns_history.push_back(o.clone());
            }
            Event::Finding(f) => match f.lifecycle {
                FindingLifecycle::Resolved => {
                    self.findings.remove(&f.fingerprint);
                }
                _ => {
                    self.findings.insert(f.fingerprint.clone(), f.clone());
                }
            },
            Event::InterfaceCounters(o) => {
                self.throughput.record(o, env.at);
                self.latest_counters = Some(o.clone());
            }
            Event::SensorHealth(h) => {
                if h.sensor.as_str() == "wifi"
                    && matches!(
                        h.state,
                        SensorState::HardwareDisabled
                            | SensorState::BackendUnavailable
                            | SensorState::PermissionDenied
                    )
                {
                    self.connected_identity = (None, None);
                    self.connected_since = None;
                    self.signal_history.clear();
                }
                if h.sensor.as_str() == "iface"
                    && matches!(
                        h.state,
                        SensorState::Stale
                            | SensorState::BackendUnavailable
                            | SensorState::HardwareDisabled
                    )
                {
                    self.throughput.forget();
                    self.latest_counters = None;
                }
                self.sensor_health.insert(h.sensor.clone(), h.clone());
            }
            Event::InterfaceStateChanged(_) | Event::RoamDetected(_) => {}
        }

        self.push_feed(env);
    }

    fn record_link_longitudinal(&mut self, obs: &WifiObservation, at: time::OffsetDateTime) {
        let new_identity = (obs.ssid.clone(), obs.bssid.clone());
        if self.connected_identity != new_identity || self.connected_since.is_none() {
            self.connected_identity = new_identity;
            self.connected_since = Some(at);
            self.signal_history.clear();
        }
        if let Some(rssi) = obs.rssi_dbm {
            if self.signal_history.len() == SIGNAL_HISTORY {
                self.signal_history.pop_front();
            }
            self.signal_history.push_back(SignalSample { at, rssi_dbm: rssi });
        }
    }

    /// Wall-clock duration since the current association was first
    /// observed. `None` when not associated.
    pub fn connected_duration(&self) -> Option<Duration> {
        let since = self.connected_since?;
        let now = time::OffsetDateTime::now_utc();
        let secs = (now - since).whole_seconds().max(0);
        Some(Duration::from_secs(secs as u64))
    }

    /// Difference of mean RSSI between the recent and prior halves of
    /// `lookback`. Returns `None` if either half has fewer than 2
    /// samples — we don't want to claim a trend from a single reading.
    pub fn rssi_delta_over(&self, lookback: Duration) -> Option<f64> {
        let now = time::OffsetDateTime::now_utc();
        let half = lookback.as_secs() as i64 / 2;
        let recent_start = now - time::Duration::seconds(half);
        let prior_start = now - time::Duration::seconds(lookback.as_secs() as i64);
        let mut recent_sum = 0i64;
        let mut recent_n = 0i64;
        let mut prior_sum = 0i64;
        let mut prior_n = 0i64;
        for s in &self.signal_history {
            if s.at >= recent_start {
                recent_sum += s.rssi_dbm as i64;
                recent_n += 1;
            } else if s.at >= prior_start {
                prior_sum += s.rssi_dbm as i64;
                prior_n += 1;
            }
        }
        if recent_n < 2 || prior_n < 2 {
            return None;
        }
        Some(recent_sum as f64 / recent_n as f64 - prior_sum as f64 / prior_n as f64)
    }

    /// Lookup current health for a sensor by id (e.g. `"wifi"`).
    pub fn health_for(&self, sensor: &str) -> Option<&SensorHealth> {
        self.sensor_health
            .get(&SensorId::new(sensor))
    }

    /// Current derived throughput, if the window has accumulated enough
    /// samples. Returns `None` until at least two counter snapshots span
    /// ≥ 1 second.
    pub fn current_throughput(&self) -> Option<Throughput> {
        self.throughput.throughput_bps()
    }

    fn push_feed(&mut self, env: &Envelope) {
        let line = format_feed_line(env);
        if let Some(line) = line {
            if self.event_feed.len() == EVENT_FEED_LIMIT {
                self.event_feed.pop_front();
            }
            self.event_feed.push_back(line);
        }
    }

    pub fn uptime(&self) -> Duration {
        self.started_at.elapsed()
    }
}

#[derive(Debug, Clone)]
pub struct FeedItem {
    pub at: time::OffsetDateTime,
    pub category: signalscope_events::EventCategory,
    pub line: String,
}

fn format_feed_line(env: &Envelope) -> Option<FeedItem> {
    let line = match &env.event {
        Event::Wifi(o) => {
            let ssid = o
                .ssid
                .as_ref()
                .map(|s| s.as_str().to_string())
                .unwrap_or_else(|| "<unassociated>".into());
            let rssi = o
                .rssi_dbm
                .map(|r| format!("{r} dBm"))
                .unwrap_or_else(|| "—".into());
            format!("wifi   {ssid} @ {rssi}")
        }
        Event::Scan(s) => format!("scan   {} neighbor APs", s.neighbors.len()),
        Event::GatewayLatency(o) => {
            if o.reachable {
                format!(
                    "gw     {} via {}: {:.1} ms",
                    o.target,
                    o.probe,
                    o.rtt.as_secs_f64() * 1000.0
                )
            } else {
                format!("gw     {} unreachable", o.target)
            }
        }
        Event::DnsLatency(o) => {
            if o.answered {
                format!(
                    "dns    {}: {:.0} ms via {}",
                    o.query,
                    o.rtt.as_secs_f64() * 1000.0,
                    o.resolver
                )
            } else {
                format!(
                    "dns    {} FAILED: {}",
                    o.query,
                    o.error.clone().unwrap_or_default()
                )
            }
        }
        Event::InterfaceStateChanged(i) => format!("iface  {} {:?}→{:?}", i.interface, i.previous, i.current),
        Event::InterfaceCounters(o) => format!(
            "iface  {} rx={} tx={} err={}/{}",
            o.interface,
            humanize_bytes(o.rx_bytes_total),
            humanize_bytes(o.tx_bytes_total),
            o.rx_errors_total,
            o.tx_errors_total,
        ),
        Event::RoamDetected(r) => format!(
            "roam   {} → {} ({} → {})",
            r.from_bssid,
            r.to_bssid,
            r.from_rssi_dbm.map_or("—".into(), |v| format!("{v} dBm")),
            r.to_rssi_dbm.map_or("—".into(), |v| format!("{v} dBm")),
        ),
        Event::Finding(f) => {
            let marker = match f.lifecycle {
                FindingLifecycle::Active => "●",
                FindingLifecycle::Escalating => "↑",
                FindingLifecycle::Recovering => "↓",
                FindingLifecycle::Resolved => "○",
            };
            format!(
                "find   {marker} [{:?} c={:.2}] {}",
                f.kind,
                f.confidence.value(),
                f.headline
            )
        }
        Event::SensorHealth(h) => {
            let backend = h.backend.as_deref().unwrap_or("—");
            let detail = h
                .detail
                .as_deref()
                .map(|d| format!(" ({d})"))
                .unwrap_or_default();
            format!(
                "health {} → {:?} via {}{}",
                h.sensor, h.state, backend, detail
            )
        }
    };
    Some(FeedItem {
        at: env.at,
        category: env.event.category(),
        line,
    })
}

/// Compact byte total formatting for the event feed. Keeps the column
/// narrow even when cumulative counters reach the gigabyte range.
fn humanize_bytes(n: u64) -> String {
    const KB: f64 = 1024.0;
    const MB: f64 = 1024.0 * KB;
    const GB: f64 = 1024.0 * MB;
    const TB: f64 = 1024.0 * GB;
    let n = n as f64;
    if n >= TB {
        format!("{:.1}T", n / TB)
    } else if n >= GB {
        format!("{:.1}G", n / GB)
    } else if n >= MB {
        format!("{:.1}M", n / MB)
    } else if n >= KB {
        format!("{:.1}K", n / KB)
    } else {
        format!("{n}B")
    }
}

/// Helper for ui — human-friendly uptime formatting.
pub fn fmt_uptime(d: Duration) -> String {
    let secs = d.as_secs();
    let h = secs / 3600;
    let m = (secs % 3600) / 60;
    let s = secs % 60;
    if h > 0 {
        format!("{h}h{m:02}m{s:02}s")
    } else if m > 0 {
        format!("{m}m{s:02}s")
    } else {
        format!("{s}s")
    }
}

