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
use signalscope_core::EventBus;
use signalscope_events::{
    CorrelationFinding, DnsLatencyObservation, Envelope, Event, FindingKind,
    GatewayLatencyObservation, ScanResult, WifiObservation,
};
use tokio::time::interval;
use tracing::warn;

use crate::ui;

const GATEWAY_HISTORY: usize = 240;
const DNS_HISTORY: usize = 240;
const EVENT_FEED_LIMIT: usize = 200;

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

#[derive(Debug)]
pub struct AppState {
    pub started_at: Instant,
    pub latest_wifi: Option<WifiObservation>,
    pub latest_scan: Option<ScanResult>,
    pub gateway_history: VecDeque<GatewayLatencyObservation>,
    pub dns_history: VecDeque<DnsLatencyObservation>,
    pub findings: HashMap<FindingKind, (Instant, CorrelationFinding)>,
    pub event_feed: VecDeque<FeedItem>,
    pub focus: Focus,
    pub show_help: bool,
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
            event_feed: VecDeque::with_capacity(EVENT_FEED_LIMIT),
            focus: Focus::Overview,
            show_help: false,
        }
    }

    pub fn ingest(&mut self, env: &Envelope) {
        match &env.event {
            Event::Wifi(o) => {
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
            Event::Finding(f) => {
                self.findings
                    .insert(f.kind, (Instant::now(), f.clone()));
            }
            Event::InterfaceStateChanged(_) | Event::RoamDetected(_) => {}
        }

        self.push_feed(env);
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
    use signalscope_events::EventCategory as C;
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
        Event::RoamDetected(r) => format!(
            "roam   {} → {} ({} → {})",
            r.from_bssid,
            r.to_bssid,
            r.from_rssi_dbm.map_or("—".into(), |v| format!("{v} dBm")),
            r.to_rssi_dbm.map_or("—".into(), |v| format!("{v} dBm")),
        ),
        Event::Finding(f) => format!(
            "find   [{:?} c={:.2}] {}",
            f.kind,
            f.confidence.value(),
            f.headline
        ),
    };
    Some(FeedItem {
        at: env.at,
        category: match env.event.category() {
            C::Wifi => C::Wifi,
            C::Gateway => C::Gateway,
            C::Dns => C::Dns,
            C::Interface => C::Interface,
            C::Roam => C::Roam,
            C::Finding => C::Finding,
        },
        line,
    })
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

