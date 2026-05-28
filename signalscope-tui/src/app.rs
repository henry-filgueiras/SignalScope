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
use signalscope_core::{EventBus, TemporalSeries};
use signalscope_events::{
    Bssid, CorrelationFinding, DnsLatencyObservation, Envelope, Event, FindingLifecycle,
    GatewayLatencyObservation, InterfaceCountersObservation, ScanResult, SensorHealth, SensorId,
    SensorState, Ssid, WifiObservation,
};
use tokio::time::interval;
use tracing::warn;

use crate::replay::Playback;
use crate::ui;

const GATEWAY_HISTORY: usize = 240;
const DNS_HISTORY: usize = 240;
const EVENT_FEED_LIMIT: usize = 200;
const SIGNAL_HISTORY: usize = 90;
/// Throughput is derived from successive counter snapshots. Mirrors the
/// analysis crate's `THROUGHPUT_WINDOW` so the dashboard and the future
/// finding rules speak about the same rolling rate.
const THROUGHPUT_WINDOW: Duration = Duration::from_secs(15);
/// How many per-step throughput samples to retain for the RX/TX
/// sparklines. At the iface sensor's 2 s cadence this is ~10 min of
/// rolling history — enough to see a transfer crest and recover.
const THROUGHPUT_HISTORY: usize = 300;

pub async fn run_replay(playback: Playback) -> Result<()> {
    let mut state = AppState::new();
    state.playback = Some(playback);
    state.rebuild_to_playhead();

    enable_raw_mode()?;
    let mut stdout = io::stdout();
    execute!(stdout, EnterAlternateScreen, EnableMouseCapture)?;
    let backend = CrosstermBackend::new(stdout);
    let mut terminal = Terminal::new(backend)?;

    let result = replay_loop(&mut terminal, &mut state).await;

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

async fn replay_loop<B>(terminal: &mut Terminal<B>, state: &mut AppState) -> Result<()>
where
    B: ratatui::backend::Backend,
{
    let mut term_events = EventStream::new();
    let mut dirty = true;
    loop {
        if dirty {
            terminal.draw(|f| ui::render(f, state))?;
            dirty = false;
        }
        tokio::select! {
            biased;
            maybe_input = term_events.next() => {
                match maybe_input {
                    Some(Ok(ev)) => {
                        match handle_replay_input(ev, state) {
                            InputOutcome::Quit => break,
                            InputOutcome::Continue => {}
                        }
                        // Always redraw — sparkline frames can shift even
                        // when the seek itself moved zero events (focus,
                        // detail toggle, help overlay).
                        dirty = true;
                    }
                    Some(Err(e)) => warn!(error = %e, "terminal input error"),
                    None => break,
                }
            }
            _ = tokio::signal::ctrl_c() => break,
        }
    }
    Ok(())
}

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

/// Input handler used only in replay mode. Adds seek bindings on top
/// of the shared common keys.
fn handle_replay_input(ev: CtEvent, state: &mut AppState) -> InputOutcome {
    if let CtEvent::Key(k) = ev {
        if k.kind != KeyEventKind::Press {
            return InputOutcome::Continue;
        }
        // Common keys first.
        match (k.code, k.modifiers) {
            (KeyCode::Char('q') | KeyCode::Esc, _) => return InputOutcome::Quit,
            (KeyCode::Char('c'), KeyModifiers::CONTROL) => return InputOutcome::Quit,
            (KeyCode::Char('?'), _) | (KeyCode::Char('h'), KeyModifiers::NONE) => {
                state.show_help = !state.show_help;
                return InputOutcome::Continue;
            }
            (KeyCode::Char('d'), _) => {
                state.show_neighbor_detail = !state.show_neighbor_detail;
                return InputOutcome::Continue;
            }
            (KeyCode::Char('f'), _) | (KeyCode::Tab, _) => {
                state.focus = state.focus.next();
                return InputOutcome::Continue;
            }
            _ => {}
        }
        // Seek bindings. Shift-modified brackets ({, }) jump 10 events.
        // Single brackets step one event. n / p hop between landmarks.
        // Home/End and g/G jump to recording boundaries.
        let moved = match (k.code, k.modifiers) {
            (KeyCode::Char('['), _) => seek(state, -1),
            (KeyCode::Char(']'), _) => seek(state, 1),
            (KeyCode::Char('{'), _) => seek(state, -10),
            (KeyCode::Char('}'), _) => seek(state, 10),
            (KeyCode::Left, KeyModifiers::SHIFT) => seek(state, -10),
            (KeyCode::Right, KeyModifiers::SHIFT) => seek(state, 10),
            (KeyCode::Left, _) => seek(state, -1),
            (KeyCode::Right, _) => seek(state, 1),
            (KeyCode::Char('n'), _) => seek_to_next_landmark(state),
            (KeyCode::Char('p'), _) => seek_to_prev_landmark(state),
            (KeyCode::Home, _) | (KeyCode::Char('g'), KeyModifiers::NONE) => seek_to_start(state),
            (KeyCode::End, _) | (KeyCode::Char('G'), _) => seek_to_end(state),
            _ => false,
        };
        if moved {
            state.rebuild_to_playhead();
        }
    }
    InputOutcome::Continue
}

fn seek(state: &mut AppState, delta: isize) -> bool {
    match state.playback.as_mut() {
        Some(p) => p.seek_by(delta),
        None => false,
    }
}
fn seek_to_start(state: &mut AppState) -> bool {
    match state.playback.as_mut() {
        Some(p) => p.seek_to_start(),
        None => false,
    }
}
fn seek_to_end(state: &mut AppState) -> bool {
    match state.playback.as_mut() {
        Some(p) => p.seek_to_end(),
        None => false,
    }
}
fn seek_to_next_landmark(state: &mut AppState) -> bool {
    match state.playback.as_mut() {
        Some(p) => p.seek_to_next_landmark(),
        None => false,
    }
}
fn seek_to_prev_landmark(state: &mut AppState) -> bool {
    match state.playback.as_mut() {
        Some(p) => p.seek_to_prev_landmark(),
        None => false,
    }
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

#[derive(Debug)]
pub struct AppState {
    pub started_at: Instant,
    pub latest_wifi: Option<WifiObservation>,
    pub latest_scan: Option<ScanResult>,
    /// Wall-clock-timestamped gateway probe history. Each sample carries
    /// the full observation so the panel can read out target, RTT, and
    /// reachability without a parallel structure.
    pub gateway_history: TemporalSeries<GatewayLatencyObservation>,
    pub dns_history: TemporalSeries<DnsLatencyObservation>,
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
    /// callout. Reset on association change and on Wi-Fi sensor
    /// degradation.
    pub signal_history: TemporalSeries<i32>,
    /// Most recent interface counter snapshot. Kept verbatim so the
    /// dashboard can show absolute totals next to the derived rate.
    pub latest_counters: Option<InterfaceCountersObservation>,
    /// Rolling throughput derivation. Shares the math with the analysis
    /// crate so future throughput rules and the dashboard agree on what
    /// "now" means.
    pub throughput: InterfaceThroughputWindow,
    /// Per-step RX rate samples in bits/sec — one row per counter pair.
    /// Bursts show as spikes, idle as zeros, sustained transfer as a
    /// plateau. Fed by [`InterfaceThroughputWindow::step_throughput`].
    pub rx_throughput_history: TemporalSeries<f64>,
    pub tx_throughput_history: TemporalSeries<f64>,
    pub focus: Focus,
    pub show_help: bool,
    /// When true, the RF environment panel renders the neighbor AP table
    /// instead of the occupancy histogram. Default is the histogram —
    /// individual identities are demoted to opt-in detail.
    pub show_neighbor_detail: bool,
    /// `Some` in `signalscope analyze` mode. When set, `virtual_now()`
    /// returns the playhead's wall-clock `at` instead of real time, so
    /// every temporal callout on the dashboard ("Held 12m", "stable
    /// 2m12s", "Δ RSSI / 60s") reads as it would have at the moment
    /// of recording.
    pub playback: Option<Playback>,
}

impl AppState {
    pub fn new() -> Self {
        Self {
            started_at: Instant::now(),
            latest_wifi: None,
            latest_scan: None,
            gateway_history: TemporalSeries::new(GATEWAY_HISTORY),
            dns_history: TemporalSeries::new(DNS_HISTORY),
            findings: HashMap::new(),
            sensor_health: HashMap::new(),
            event_feed: VecDeque::with_capacity(EVENT_FEED_LIMIT),
            connected_identity: (None, None),
            connected_since: None,
            signal_history: TemporalSeries::new(SIGNAL_HISTORY),
            latest_counters: None,
            throughput: InterfaceThroughputWindow::new(THROUGHPUT_WINDOW),
            rx_throughput_history: TemporalSeries::new(THROUGHPUT_HISTORY),
            tx_throughput_history: TemporalSeries::new(THROUGHPUT_HISTORY),
            focus: Focus::Overview,
            show_help: false,
            show_neighbor_detail: false,
            playback: None,
        }
    }

    /// Wall-clock "now" for temporal callouts. In live mode this is
    /// real time; in replay mode it's the playhead's event timestamp.
    /// All AppState helpers that ask "how long since X" go through here.
    pub fn virtual_now(&self) -> time::OffsetDateTime {
        match &self.playback {
            Some(p) => p.virtual_now(),
            None => time::OffsetDateTime::now_utc(),
        }
    }

    /// Clear every accumulator that `ingest()` writes to. Used before
    /// re-ingesting the events from start-of-recording up to a new
    /// playhead position.
    pub fn reset_for_replay(&mut self) {
        self.latest_wifi = None;
        self.latest_scan = None;
        self.gateway_history.clear();
        self.dns_history.clear();
        self.findings.clear();
        self.sensor_health.clear();
        self.event_feed.clear();
        self.connected_identity = (None, None);
        self.connected_since = None;
        self.signal_history.clear();
        self.latest_counters = None;
        self.throughput.forget();
        self.rx_throughput_history.clear();
        self.tx_throughput_history.clear();
    }

    /// Rebuild dashboard state from the recorded envelope stream up
    /// to and including the current playhead. Idempotent; safe to call
    /// on every seek.
    pub fn rebuild_to_playhead(&mut self) {
        let envelopes = match &self.playback {
            Some(p) => p.envelopes_through_playhead().to_vec(),
            None => return,
        };
        self.reset_for_replay();
        for env in envelopes {
            self.ingest(&env);
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
                self.gateway_history.push(env.at, o.clone());
            }
            Event::DnsLatency(o) => {
                self.dns_history.push(env.at, o.clone());
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
                // Record the per-step rate (rate between this sample
                // and the previous one) so the sparkline shows bursts
                // and idle periods as visible shapes. This is distinct
                // from `current_throughput()` which is the rolling
                // average — that one drives the headline number.
                if let Some(step) = self.throughput.step_throughput() {
                    self.rx_throughput_history.push(env.at, step.rx_bps);
                    self.tx_throughput_history.push(env.at, step.tx_bps);
                }
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
                    self.rx_throughput_history.clear();
                    self.tx_throughput_history.clear();
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
            self.signal_history.push(at, rssi);
        }
    }

    /// Wall-clock duration since the current association was first
    /// observed. `None` when not associated.
    pub fn connected_duration(&self) -> Option<Duration> {
        let since = self.connected_since?;
        let now = self.virtual_now();
        let secs = (now - since).whole_seconds().max(0);
        Some(Duration::from_secs(secs as u64))
    }

    /// Difference of mean RSSI between the recent and prior halves of
    /// `lookback`. Returns `None` if either half has fewer than 2
    /// samples — we don't want to claim a trend from a single reading.
    pub fn rssi_delta_over(&self, lookback: Duration) -> Option<f64> {
        let now = self.virtual_now();
        let half = lookback.as_secs() as i64 / 2;
        let recent_start = now - time::Duration::seconds(half);
        let prior_start = now - time::Duration::seconds(lookback.as_secs() as i64);
        let mut recent_sum = 0i64;
        let mut recent_n = 0i64;
        let mut prior_sum = 0i64;
        let mut prior_n = 0i64;
        for s in self.signal_history.iter() {
            if s.at >= recent_start {
                recent_sum += s.value as i64;
                recent_n += 1;
            } else if s.at >= prior_start {
                prior_sum += s.value as i64;
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

