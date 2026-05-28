//! ratatui rendering. Pure function over `&AppState` — no I/O, no state
//! mutation. Layouts recompute from the frame area on every draw, so resize
//! is automatically supported.

use std::collections::BTreeMap;

use ratatui::layout::{Constraint, Direction, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::symbols;
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, List, ListItem, Paragraph, Sparkline, Wrap};
use ratatui::Frame;
use signalscope_analysis::{pressure_tier, PressureTier};
use signalscope_events::{
    BandClass, CorrelationFinding, DnsLatencyObservation, EventCategory, FindingLifecycle,
    GatewayLatencyObservation, NeighborAp, ObservationConfidence, SensorHealth, SensorState,
};

use crate::app::{AppState, FeedItem};
use crate::theme;

pub fn render(f: &mut Frame, state: &AppState) {
    let area = f.area();

    let outer = Layout::new(
        Direction::Vertical,
        [
            Constraint::Length(1), // header
            Constraint::Min(10),   // main
            Constraint::Length(8), // event feed
            Constraint::Length(1), // footer
        ],
    )
    .split(area);

    render_header(f, outer[0], state);
    render_main(f, outer[1], state);
    render_feed(f, outer[2], state);
    render_footer(f, outer[3], state);

    if state.show_help {
        render_help_overlay(f, area);
    }
}

fn render_header(f: &mut Frame, area: Rect, state: &AppState) {
    let sensors = "wifi · gateway · dns";
    let uptime = crate::app::fmt_uptime(state.uptime());
    let line = Line::from(vec![
        Span::styled("SignalScope", theme::title_style()),
        Span::styled("  ·  ", theme::dim()),
        Span::styled("live", Style::default().fg(theme::INFO_FG)),
        Span::styled("  ·  uptime ", theme::dim()),
        Span::styled(uptime, theme::value()),
        Span::styled("  ·  sensors: ", theme::dim()),
        Span::styled(sensors, theme::value()),
    ]);
    f.render_widget(Paragraph::new(line), area);
}

fn render_footer(f: &mut Frame, area: Rect, state: &AppState) {
    let focus_label = match state.focus {
        crate::app::Focus::Overview => "overview",
        crate::app::Focus::Neighbors => "neighbors",
        crate::app::Focus::Findings => "findings",
    };
    let detail_label = if state.show_neighbor_detail {
        "AP table"
    } else {
        "occupancy"
    };
    let line = Line::from(vec![
        Span::styled("q ", theme::value()),
        Span::styled("quit ", theme::dim()),
        Span::styled(" · tab ", theme::value()),
        Span::styled("focus ", theme::dim()),
        Span::styled(" · d ", theme::value()),
        Span::styled("RF view ", theme::dim()),
        Span::styled(" · ? ", theme::value()),
        Span::styled("help ", theme::dim()),
        Span::styled("    focus: ", theme::dim()),
        Span::styled(focus_label, Style::default().fg(theme::INFO_FG)),
        Span::styled("   RF: ", theme::dim()),
        Span::styled(detail_label, Style::default().fg(theme::INFO_FG)),
    ]);
    f.render_widget(Paragraph::new(line), area);
}

fn render_main(f: &mut Frame, area: Rect, state: &AppState) {
    let cols = Layout::new(
        Direction::Horizontal,
        [Constraint::Percentage(62), Constraint::Percentage(38)],
    )
    .split(area);

    // Left column: connected link / gateway / dns stack.
    // The connected-link card gets extra rows because it now hosts the
    // longitudinal "Connected for / Δ RSSI" line and a small sparkline.
    let left = Layout::new(
        Direction::Vertical,
        [
            Constraint::Length(11),
            Constraint::Length(7),
            Constraint::Min(5),
        ],
    )
    .split(cols[0]);

    render_wifi_card(
        f,
        left[0],
        state,
    );
    render_gateway_card(f, left[1], state);
    render_dns_card(f, left[2], state);

    // Right column: neighbors + findings
    let right = Layout::new(
        Direction::Vertical,
        [Constraint::Percentage(55), Constraint::Percentage(45)],
    )
    .split(cols[1]);

    render_rf_environment(f, right[0], state);
    render_findings(f, right[1], state);
}

fn render_wifi_card(f: &mut Frame, area: Rect, state: &AppState) {
    let wifi = state.latest_wifi.as_ref();
    let health = state.health_for("wifi");
    let title = wifi_card_title(health);
    let block = card_block(&title);
    let inner = block.inner(area);
    f.render_widget(block, area);

    // Health banner sits above the data block when something is wrong.
    let status_line = wifi_status_banner(health);

    let layout = if status_line.is_some() {
        Layout::new(
            Direction::Vertical,
            [Constraint::Length(1), Constraint::Min(1)],
        )
        .split(inner)
    } else {
        Layout::new(Direction::Vertical, [Constraint::Min(1)]).split(inner)
    };

    let body_area = if let Some(line) = status_line {
        f.render_widget(Paragraph::new(line), layout[0]);
        layout[1]
    } else {
        layout[0]
    };

    let Some(w) = wifi else {
        let msg = match health.map(|h| h.state) {
            Some(SensorState::BackendUnavailable) => {
                "no Wi-Fi backend available on this host"
            }
            Some(SensorState::HardwareDisabled) => "Wi-Fi is off",
            Some(SensorState::PermissionDenied) => "permission required to read Wi-Fi state",
            _ => "no data — awaiting first observation",
        };
        f.render_widget(Paragraph::new(msg).style(theme::dim()), body_area);
        return;
    };

    let confidence_marker = match w.confidence {
        ObservationConfidence::Direct => Span::raw(""),
        ObservationConfidence::Inferred => Span::styled(
            "  (redacted source)",
            Style::default().fg(theme::WARN_FG),
        ),
        ObservationConfidence::Estimated => Span::styled(
            "  (estimated)",
            Style::default().fg(theme::WARN_FG),
        ),
        ObservationConfidence::Stale => Span::styled(
            "  (stale)",
            Style::default().fg(theme::BAD_FG),
        ),
    };

    let ssid_display = w
        .ssid
        .as_ref()
        .map(|s| s.as_str().to_string())
        .unwrap_or_else(|| "<redacted>".into());
    let bssid_display = w
        .bssid
        .as_ref()
        .map(|b| b.as_str().to_string())
        .unwrap_or_else(|| "—".into());
    let rssi_str = w
        .rssi_dbm
        .map(|r| format!("{r} dBm"))
        .unwrap_or_else(|| "—".into());
    let noise_str = w
        .noise_dbm
        .map(|n| format!("{n} dBm"))
        .unwrap_or_else(|| "—".into());
    let snr_str = w
        .snr_db()
        .map(|s| format!("{s} dB"))
        .unwrap_or_else(|| "—".into());
    let chan_str = w.channel.map(channel_display).unwrap_or_else(|| "—".into());
    let phy_str = w.phy_mode.clone().unwrap_or_else(|| "—".into());

    let rssi_goodness_val = w.rssi_dbm.map(rssi_goodness).unwrap_or(0.5);
    let rssi_color = theme::quality_color(rssi_goodness_val);

    // Longitudinal callout: how long this association has held and how
    // RSSI has drifted across the recent window. Both are computed in
    // AppState — see `connected_duration` and `rssi_delta_over`.
    let connected_str = state
        .connected_duration()
        .map(|d| humanize_duration(d))
        .unwrap_or_else(|| "—".into());
    let delta = state.rssi_delta_over(std::time::Duration::from_secs(60));
    let (delta_text, delta_color) = match delta {
        Some(d) if d.abs() < 1.5 => ("stable".to_string(), theme::DIM_FG),
        Some(d) if d < 0.0 => (format!("{d:+.0} dB / 60s"), theme::BAD_FG),
        Some(d) => (format!("{d:+.0} dB / 60s"), theme::OK_FG),
        None => ("…".to_string(), theme::DIM_FG),
    };

    let lines = vec![
        Line::from(vec![
            label("SSID"),
            Span::styled(ssid_display, theme::value()),
            confidence_marker,
        ]),
        kv("BSSID", bssid_display),
        Line::from(vec![
            label("RSSI"),
            Span::styled(
                rssi_str,
                Style::default().fg(rssi_color).add_modifier(Modifier::BOLD),
            ),
            Span::raw("    "),
            label("Noise"),
            Span::styled(noise_str, theme::value()),
            Span::raw("    "),
            label("SNR"),
            Span::styled(snr_str, theme::value()),
        ]),
        Line::from(vec![
            label("Channel"),
            Span::styled(chan_str, theme::value()),
            Span::raw("    "),
            label("PHY"),
            Span::styled(phy_str, theme::value()),
        ]),
        Line::from(vec![
            label("Held"),
            Span::styled(connected_str, theme::value()),
            Span::raw("    "),
            label("Δ RSSI"),
            Span::styled(delta_text, Style::default().fg(delta_color)),
        ]),
    ];

    // Split the body area so the text takes most of it and a small
    // RSSI sparkline lives at the bottom. If history is empty, skip the
    // sparkline so the card doesn't show a flat baseline that looks like
    // a dead reading.
    if !state.signal_history.is_empty() {
        let split = Layout::new(
            Direction::Vertical,
            [Constraint::Min(lines.len() as u16), Constraint::Length(1)],
        )
        .split(body_area);
        f.render_widget(Paragraph::new(lines), split[0]);
        let data: Vec<u64> = state
            .signal_history
            .iter()
            .map(|s| rssi_to_sparkline_height(s.rssi_dbm))
            .collect();
        let spark = Sparkline::default()
            .data(&data)
            .style(Style::default().fg(rssi_color))
            .bar_set(symbols::bar::NINE_LEVELS);
        f.render_widget(spark, split[1]);
    } else {
        f.render_widget(Paragraph::new(lines), body_area);
    }
}

/// Map a raw RSSI (dBm) into the 0..=90 sparkline bar-height range. We
/// invert because stronger (less negative) RSSI should produce *taller*
/// bars, and clamp -90..-30 so the visual stays bounded.
fn rssi_to_sparkline_height(rssi_dbm: i32) -> u64 {
    let clamped = rssi_dbm.clamp(-90, -30);
    (clamped + 90) as u64
}

fn wifi_card_title(health: Option<&SensorHealth>) -> String {
    match health {
        Some(h) => {
            let backend = h.backend.as_deref().unwrap_or("—");
            match h.state {
                SensorState::Operational => format!("Connected link · {backend}"),
                SensorState::BackendUnavailable => "Connected link · backend unavailable".to_string(),
                SensorState::HardwareDisabled => format!("Connected link · {backend} · off"),
                SensorState::PermissionDenied => {
                    format!("Connected link · {backend} · permission denied")
                }
                SensorState::ParseFailed => format!("Connected link · {backend} · parse failed"),
                SensorState::Stale => format!("Connected link · {backend} · stale"),
            }
        }
        None => "Connected link".to_string(),
    }
}

/// Returns a one-line summary banner when the sensor is in a non-operational
/// state. Operational sensors get no banner — the card just shows data.
fn wifi_status_banner<'a>(health: Option<&'a SensorHealth>) -> Option<Line<'a>> {
    let h = health?;
    if h.state == SensorState::Operational {
        return None;
    }
    let color = match h.state {
        SensorState::Stale => theme::WARN_FG,
        SensorState::PermissionDenied | SensorState::HardwareDisabled => theme::WARN_FG,
        _ => theme::BAD_FG,
    };
    let mut spans = vec![Span::styled(
        format!("⚠ {:?}", h.state),
        Style::default().fg(color).add_modifier(Modifier::BOLD),
    )];
    if let Some(detail) = h.detail.as_deref() {
        spans.push(Span::raw(" — "));
        spans.push(Span::styled(detail.to_string(), theme::dim()));
    }
    Some(Line::from(spans))
}

fn channel_display(c: signalscope_events::Channel) -> String {
    let width = match c.width {
        Some(signalscope_events::ChannelWidth::Mhz20) => "20",
        Some(signalscope_events::ChannelWidth::Mhz40) => "40",
        Some(signalscope_events::ChannelWidth::Mhz80) => "80",
        Some(signalscope_events::ChannelWidth::Mhz160) => "160",
        Some(signalscope_events::ChannelWidth::Mhz80Plus80) => "80+80",
        None => "?",
    };
    let band = match c.band {
        signalscope_events::BandClass::TwoPointFourGhz => "2.4 GHz",
        signalscope_events::BandClass::FiveGhz => "5 GHz",
        signalscope_events::BandClass::SixGhz => "6 GHz",
        signalscope_events::BandClass::Unknown => "?",
    };
    format!("{} · {} MHz · {band}", c.number, width)
}

fn render_gateway_card(f: &mut Frame, area: Rect, state: &AppState) {
    let block = card_block("Gateway latency");
    let inner = block.inner(area);
    f.render_widget(block, area);

    let samples: Vec<&GatewayLatencyObservation> = state.gateway_history.iter().collect();
    if samples.is_empty() {
        f.render_widget(
            Paragraph::new("no data — awaiting first probe").style(theme::dim()),
            inner,
        );
        return;
    }

    let layout = Layout::new(
        Direction::Vertical,
        [Constraint::Length(1), Constraint::Min(1)],
    )
    .split(inner);

    let target = samples.last().map(|s| s.target.as_str()).unwrap_or("—");
    let median = median_rtt(&samples).unwrap_or(0.0);
    let p95 = p95_rtt(&samples).unwrap_or(0.0);
    let loss = loss_pct(&samples);
    let last = samples.last().unwrap();
    let last_str = if last.reachable {
        format!("{:.1} ms", last.rtt.as_secs_f64() * 1000.0)
    } else {
        "unreachable".into()
    };

    let goodness = rtt_goodness(median, 5.0, 40.0);
    let color = theme::quality_color(goodness);

    let summary = Line::from(vec![
        Span::styled(target.to_string(), theme::value()),
        Span::styled("  last ", theme::dim()),
        Span::styled(last_str, Style::default().fg(color)),
        Span::styled("  median ", theme::dim()),
        Span::styled(format!("{median:.1} ms"), theme::value()),
        Span::styled("  p95 ", theme::dim()),
        Span::styled(format!("{p95:.1} ms"), theme::value()),
        Span::styled("  loss ", theme::dim()),
        Span::styled(format!("{loss:.0}%"), theme::value()),
    ]);
    f.render_widget(Paragraph::new(summary), layout[0]);

    let data = sparkline_data(samples.iter().map(|s| {
        if s.reachable {
            (s.rtt.as_secs_f64() * 1000.0) as u64
        } else {
            0
        }
    }));
    let spark = Sparkline::default()
        .data(&data)
        .style(Style::default().fg(color))
        .bar_set(symbols::bar::NINE_LEVELS);
    f.render_widget(spark, layout[1]);
}

fn render_dns_card(f: &mut Frame, area: Rect, state: &AppState) {
    let block = card_block("DNS latency");
    let inner = block.inner(area);
    f.render_widget(block, area);

    let samples: Vec<&DnsLatencyObservation> = state.dns_history.iter().collect();
    if samples.is_empty() {
        f.render_widget(
            Paragraph::new("no data — awaiting first probe").style(theme::dim()),
            inner,
        );
        return;
    }

    let layout = Layout::new(
        Direction::Vertical,
        [Constraint::Length(1), Constraint::Min(1)],
    )
    .split(inner);

    let resolver = samples
        .last()
        .map(|s| s.resolver.as_str())
        .unwrap_or("—");
    let median = dns_median(&samples).unwrap_or(0.0);
    let fail_pct = dns_fail_pct(&samples);
    let last = samples.last().unwrap();
    let last_str = if last.answered {
        format!("{:.0} ms", last.rtt.as_secs_f64() * 1000.0)
    } else {
        "FAIL".into()
    };

    let goodness = rtt_goodness(median, 15.0, 150.0).min(1.0 - (fail_pct / 100.0));
    let color = theme::quality_color(goodness);

    let summary = Line::from(vec![
        Span::styled(resolver.to_string(), theme::value()),
        Span::styled("  last ", theme::dim()),
        Span::styled(last_str, Style::default().fg(color)),
        Span::styled("  median ", theme::dim()),
        Span::styled(format!("{median:.0} ms"), theme::value()),
        Span::styled("  fail ", theme::dim()),
        Span::styled(format!("{fail_pct:.0}%"), theme::value()),
    ]);
    f.render_widget(Paragraph::new(summary), layout[0]);

    let data = sparkline_data(samples.iter().map(|s| {
        if s.answered {
            (s.rtt.as_secs_f64() * 1000.0) as u64
        } else {
            0
        }
    }));
    let spark = Sparkline::default()
        .data(&data)
        .style(Style::default().fg(color))
        .bar_set(symbols::bar::NINE_LEVELS);
    f.render_widget(spark, layout[1]);
}

fn render_rf_environment(f: &mut Frame, area: Rect, state: &AppState) {
    let neighbors = state
        .latest_scan
        .as_ref()
        .map(|s| s.neighbors.as_slice())
        .unwrap_or(&[]);

    let mode_hint = if state.show_neighbor_detail { "detail" } else { "occupancy" };
    let block = card_block(&format!(
        "RF environment · {} APs visible · {mode_hint}",
        neighbors.len()
    ));
    let inner = block.inner(area);
    f.render_widget(block, area);

    let connected_channel = state.latest_wifi.as_ref().and_then(|w| w.channel);
    let summary = environmental_summary(neighbors, connected_channel, state);

    let layout = Layout::new(
        Direction::Vertical,
        [
            Constraint::Length(1), // summary
            Constraint::Length(1), // spacer
            Constraint::Min(1),    // body
        ],
    )
    .split(inner);
    f.render_widget(Paragraph::new(summary), layout[0]);

    if state.show_neighbor_detail {
        render_neighbor_table(f, layout[2], neighbors, state.latest_wifi.as_ref());
    } else {
        render_occupancy_histogram(f, layout[2], neighbors, connected_channel);
    }
}

/// One-line "ambient weather report" for the RF environment card.
/// Anchored on the *connected* channel — modern Wi-Fi pain is local, not
/// global. Density trend reads off the active `RfDensityTrend` finding
/// so the panel agrees with the findings list rather than computing a
/// parallel verdict.
fn environmental_summary<'a>(
    neighbors: &[NeighborAp],
    connected_channel: Option<signalscope_events::Channel>,
    state: &'a AppState,
) -> Line<'a> {
    let per_channel = per_channel_counts(neighbors);

    let (anchor_spans, anchor_count) = match connected_channel {
        Some(c) => {
            let n = per_channel.get(&c.number).copied().unwrap_or(0);
            let tier = pressure_tier(n);
            let (tier_color, tier_label) = (tier_color(tier), tier.headline_label());
            (
                vec![
                    Span::styled("connected ch", theme::dim()),
                    Span::styled(format!("{}", c.number), theme::value()),
                    Span::styled("  ·  pressure: ", theme::dim()),
                    Span::styled(
                        tier_label.to_string(),
                        Style::default().fg(tier_color).add_modifier(Modifier::BOLD),
                    ),
                ],
                n,
            )
        }
        None => (
            vec![Span::styled("unassociated", theme::dim())],
            0usize,
        ),
    };
    let _ = anchor_count;

    let rising = state.findings.contains_key("rf_density_trend:rising");
    let falling = state.findings.contains_key("rf_density_trend:falling");
    let (trend, trend_color) = match (rising, falling) {
        (true, _) => ("density rising", theme::WARN_FG),
        (_, true) => ("density falling", theme::INFO_FG),
        _ => ("density stable", theme::DIM_FG),
    };

    let mut spans = anchor_spans;
    spans.push(Span::styled("  ·  ", theme::dim()));
    spans.push(Span::styled(trend, Style::default().fg(trend_color)));
    Line::from(spans)
}

/// Primary visualization: per-band channel occupancy bars. The connected
/// channel — if any — is marked with a trailing arrow so it remains the
/// natural anchor for the eye. Bars are one block per AP, capped to keep
/// the panel compact.
fn render_occupancy_histogram(
    f: &mut Frame,
    area: Rect,
    neighbors: &[NeighborAp],
    connected_channel: Option<signalscope_events::Channel>,
) {
    if neighbors.is_empty() {
        f.render_widget(
            Paragraph::new("no RF data yet — awaiting scan").style(theme::dim()),
            area,
        );
        return;
    }

    // Channels with no band assignment are excluded from the histogram —
    // they're rare and there's no good place to put them.
    let mut by_band: BTreeMap<BandSort, BTreeMap<u16, usize>> = BTreeMap::new();
    for ap in neighbors {
        let Some(ch) = ap.channel else { continue };
        by_band
            .entry(BandSort::from(ch.band))
            .or_default()
            .entry(ch.number)
            .and_modify(|n| *n += 1)
            .or_insert(1);
    }

    if by_band.is_empty() {
        f.render_widget(
            Paragraph::new("scan reports no channel data\npress 'd' for raw AP details")
                .style(theme::dim())
                .wrap(Wrap { trim: true }),
            area,
        );
        return;
    }

    const BAR_WIDTH: usize = 14;
    let max_count = by_band
        .values()
        .flat_map(|m| m.values().copied())
        .max()
        .unwrap_or(1)
        .max(1);

    let mut lines: Vec<Line> = Vec::new();
    for (band_sort, channels) in &by_band {
        if !lines.is_empty() {
            lines.push(Line::from(""));
        }
        lines.push(Line::from(Span::styled(
            band_sort.label().to_string(),
            Style::default().fg(theme::INFO_FG).add_modifier(Modifier::BOLD),
        )));
        for (ch, count) in channels {
            let bar_units = ((*count * BAR_WIDTH) + max_count - 1) / max_count;
            let bar: String = "█".repeat(bar_units.min(BAR_WIDTH));
            let padding: String = " ".repeat(BAR_WIDTH.saturating_sub(bar_units));

            let connected = connected_channel.is_some_and(|c| c.number == *ch);
            let bar_color = if connected {
                theme::TITLE_FG
            } else {
                bar_color_for_count(*count)
            };
            let mut spans = vec![
                Span::styled(
                    format!("  ch{:<4}", ch),
                    if connected {
                        Style::default().fg(theme::TITLE_FG).add_modifier(Modifier::BOLD)
                    } else {
                        theme::value()
                    },
                ),
                Span::styled(bar, Style::default().fg(bar_color)),
                Span::styled(padding, theme::dim()),
                Span::styled(format!("  {:>2}", count), theme::value()),
            ];
            if connected {
                spans.push(Span::styled(
                    "  ← connected",
                    Style::default().fg(theme::TITLE_FG),
                ));
            }
            lines.push(Line::from(spans));
        }
    }

    let total = lines.len();
    let visible: Vec<Line> = lines.into_iter().take(area.height as usize).collect();
    let truncated = total > visible.len();
    let mut paragraph_lines = visible;
    if truncated {
        // Replace the last line with a truncation hint.
        let last = paragraph_lines.pop();
        let _ = last;
        paragraph_lines.push(Line::from(Span::styled(
            "  …  press 'd' for full AP list",
            theme::dim(),
        )));
    }
    f.render_widget(Paragraph::new(paragraph_lines), area);
}

/// Detail mode: the original neighbor table. Kept behind a 'd' toggle so
/// identity-oriented inspection remains available without crowding the
/// primary view.
fn render_neighbor_table(
    f: &mut Frame,
    area: Rect,
    neighbors: &[NeighborAp],
    current: Option<&signalscope_events::WifiObservation>,
) {
    let mut sorted: Vec<&NeighborAp> = neighbors.iter().collect();
    sorted.sort_by(|a, b| match (a.rssi_dbm, b.rssi_dbm) {
        (Some(x), Some(y)) => y.cmp(&x),
        (Some(_), None) => std::cmp::Ordering::Less,
        (None, Some(_)) => std::cmp::Ordering::Greater,
        (None, None) => std::cmp::Ordering::Equal,
    });

    let current_bssid = current.and_then(|w| w.bssid.as_ref());

    let items: Vec<ListItem> = sorted
        .iter()
        .take(area.height as usize)
        .map(|ap| {
            let is_current = ap
                .bssid
                .as_ref()
                .zip(current_bssid)
                .map_or(false, |(a, b)| a == b);
            let marker = if is_current { "● " } else { "  " };
            let ssid = ap
                .ssid
                .as_ref()
                .map(|s| s.as_str().to_string())
                .unwrap_or_else(|| "<redacted>".into());
            let ssid_trunc: String = ssid.chars().take(16).collect();
            let channel = ap
                .channel
                .map(|c| format!("{:>3}", c.number))
                .unwrap_or_else(|| "  -".into());
            let bssid_display = ap
                .bssid
                .as_ref()
                .map(|b| b.as_str().to_string())
                .unwrap_or_else(|| "—".into());
            let (rssi_text, rssi_color) = match ap.rssi_dbm {
                Some(r) => (format!("{:>4} dBm", r), theme::quality_color(rssi_goodness(r))),
                None => ("   —    ".to_string(), theme::DIM_FG),
            };
            let ssid_style = match (is_current, ap.confidence) {
                (true, _) => Style::default()
                    .fg(theme::TITLE_FG)
                    .add_modifier(Modifier::BOLD),
                (_, ObservationConfidence::Direct) => theme::value(),
                _ => theme::dim(),
            };
            ListItem::new(Line::from(vec![
                Span::styled(marker, Style::default().fg(theme::INFO_FG)),
                Span::styled(format!("{:<16}", ssid_trunc), ssid_style),
                Span::raw(" "),
                Span::styled(format!("{:<17}", bssid_display), theme::dim()),
                Span::raw(" ch"),
                Span::styled(channel, theme::value()),
                Span::raw(" "),
                Span::styled(rssi_text, Style::default().fg(rssi_color)),
            ]))
        })
        .collect();

    f.render_widget(List::new(items), area);
}

fn per_channel_counts(neighbors: &[NeighborAp]) -> std::collections::HashMap<u16, usize> {
    let mut m = std::collections::HashMap::new();
    for ap in neighbors {
        if let Some(ch) = ap.channel {
            *m.entry(ch.number).or_insert(0) += 1;
        }
    }
    m
}

fn tier_color(tier: PressureTier) -> Color {
    match tier {
        PressureTier::Low => theme::OK_FG,
        PressureTier::Moderate => theme::INFO_FG,
        PressureTier::Elevated => theme::WARN_FG,
        PressureTier::Severe => theme::BAD_FG,
    }
}

fn bar_color_for_count(count: usize) -> Color {
    tier_color(pressure_tier(count))
}

/// Wrapper for ordered display of bands. The `BandClass` enum doesn't
/// have a deterministic order beyond declaration; we want histogram
/// sections to consistently read low → high.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
struct BandSort(u8);

impl BandSort {
    fn label(self) -> &'static str {
        match self.0 {
            0 => "2.4 GHz",
            1 => "5 GHz",
            2 => "6 GHz",
            _ => "unknown band",
        }
    }
}

impl From<BandClass> for BandSort {
    fn from(b: BandClass) -> Self {
        BandSort(match b {
            BandClass::TwoPointFourGhz => 0,
            BandClass::FiveGhz => 1,
            BandClass::SixGhz => 2,
            BandClass::Unknown => 3,
        })
    }
}

fn render_findings(f: &mut Frame, area: Rect, state: &AppState) {
    let block = card_block("Findings");
    let inner = block.inner(area);
    f.render_widget(block, area);

    if state.findings.is_empty() {
        f.render_widget(
            Paragraph::new("nothing flagged — network looks calm")
                .style(theme::dim())
                .wrap(Wrap { trim: true }),
            inner,
        );
        return;
    }

    let mut entries: Vec<&CorrelationFinding> = state.findings.values().collect();
    entries.sort_by(|a, b| {
        b.peak_confidence
            .value()
            .partial_cmp(&a.peak_confidence.value())
            .unwrap_or(std::cmp::Ordering::Equal)
    });

    let items: Vec<ListItem> = entries
        .iter()
        .take(inner.height as usize)
        .map(|f| {
            let conf = f.confidence.value();
            let conf_color = if conf >= 0.7 {
                theme::BAD_FG
            } else if conf >= 0.4 {
                theme::WARN_FG
            } else {
                theme::INFO_FG
            };
            let (marker, marker_color) = lifecycle_glyph(f.lifecycle);
            let duration = humanize_duration(f.active_duration());
            ListItem::new(Line::from(vec![
                Span::styled(
                    format!("{marker} "),
                    Style::default().fg(marker_color).add_modifier(Modifier::BOLD),
                ),
                Span::styled(
                    format!("c={conf:.2} "),
                    Style::default().fg(conf_color).add_modifier(Modifier::BOLD),
                ),
                Span::styled(format!("{duration:<5} "), theme::dim()),
                Span::styled(f.headline.clone(), theme::value()),
            ]))
        })
        .collect();

    f.render_widget(List::new(items), inner);
}

fn lifecycle_glyph(state: FindingLifecycle) -> (&'static str, ratatui::style::Color) {
    match state {
        FindingLifecycle::Active => ("●", theme::BAD_FG),
        FindingLifecycle::Escalating => ("↑", theme::BAD_FG),
        FindingLifecycle::Recovering => ("↓", theme::WARN_FG),
        FindingLifecycle::Resolved => ("○", theme::OK_FG),
    }
}

fn humanize_duration(d: std::time::Duration) -> String {
    let secs = d.as_secs();
    let h = secs / 3600;
    let m = (secs % 3600) / 60;
    let s = secs % 60;
    if h > 0 {
        format!("{h}h{m:02}m")
    } else if m > 0 {
        format!("{m}m{s:02}s")
    } else {
        format!("{s}s")
    }
}

fn render_feed(f: &mut Frame, area: Rect, state: &AppState) {
    let block = card_block("Event feed");
    let inner = block.inner(area);
    f.render_widget(block, area);

    let items: Vec<ListItem> = state
        .event_feed
        .iter()
        .rev()
        .take(inner.height as usize)
        .map(feed_item_row)
        .collect();

    f.render_widget(List::new(items), inner);
}

fn render_help_overlay(f: &mut Frame, area: Rect) {
    let w = 50.min(area.width.saturating_sub(4));
    let h = 11.min(area.height.saturating_sub(4));
    let x = (area.width.saturating_sub(w)) / 2;
    let y = (area.height.saturating_sub(h)) / 2;
    let rect = Rect::new(x, y, w, h);

    let block = Block::default()
        .borders(Borders::ALL)
        .border_style(theme::frame())
        .title(Span::styled(" Help ", theme::title_style()));
    let inner = block.inner(rect);
    f.render_widget(ratatui::widgets::Clear, rect);
    f.render_widget(block, rect);

    let body = vec![
        Line::from("q / Esc        quit"),
        Line::from("Ctrl-C         quit"),
        Line::from("tab / f        cycle focus"),
        Line::from("d              toggle RF view (occupancy ↔ AP table)"),
        Line::from("?  / h         toggle this help"),
        Line::from(""),
        Line::from(Span::styled(
            "SignalScope is read-only — no packet capture, no probes",
            theme::dim(),
        )),
        Line::from(Span::styled(
            "beyond ping + DNS resolution.",
            theme::dim(),
        )),
    ];
    f.render_widget(Paragraph::new(body), inner);
}

// ---------- helpers ----------

fn card_block(title: &str) -> Block<'static> {
    Block::default()
        .borders(Borders::ALL)
        .border_style(theme::frame())
        .title(Span::styled(format!(" {title} "), theme::title_style()))
}

fn kv<'a>(k: &'a str, v: impl Into<String>) -> Line<'a> {
    Line::from(vec![label(k), Span::styled(v.into(), theme::value())])
}

fn label<'a>(k: &'a str) -> Span<'a> {
    Span::styled(format!("{:<8}", k), theme::label())
}

fn feed_item_row(item: &FeedItem) -> ListItem<'_> {
    let color = match item.category {
        EventCategory::Wifi => theme::INFO_FG,
        EventCategory::Gateway => theme::OK_FG,
        EventCategory::Dns => theme::OK_FG,
        EventCategory::Interface => theme::WARN_FG,
        EventCategory::Roam => theme::WARN_FG,
        EventCategory::Finding => theme::BAD_FG,
        EventCategory::Health => theme::WARN_FG,
    };
    // HH:MM:SS in UTC. We don't know the user's TZ portably without an extra
    // dep; the priority is relative ordering, not local-time pretty-printing.
    let h = item.at.hour();
    let m = item.at.minute();
    let s = item.at.second();
    let ts = format!("{h:02}:{m:02}:{s:02}");
    ListItem::new(Line::from(vec![
        Span::styled(ts, theme::dim()),
        Span::raw("  "),
        Span::styled(item.line.clone(), Style::default().fg(color)),
    ]))
}

fn rssi_goodness(rssi_dbm: i32) -> f32 {
    // -40 dBm or stronger = 1.0; -85 dBm or weaker = 0.0.
    let clamped = rssi_dbm.clamp(-90, -30);
    ((clamped as f32 + 90.0) / 60.0).clamp(0.0, 1.0)
}

fn rtt_goodness(rtt_ms: f64, good: f64, bad: f64) -> f32 {
    if rtt_ms <= good {
        return 1.0;
    }
    if rtt_ms >= bad {
        return 0.0;
    }
    (1.0 - (rtt_ms - good) / (bad - good)) as f32
}

fn median_rtt(samples: &[&GatewayLatencyObservation]) -> Option<f64> {
    let mut v: Vec<f64> = samples
        .iter()
        .filter(|s| s.reachable)
        .map(|s| s.rtt.as_secs_f64() * 1000.0)
        .collect();
    if v.is_empty() {
        return None;
    }
    v.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
    Some(v[v.len() / 2])
}

fn p95_rtt(samples: &[&GatewayLatencyObservation]) -> Option<f64> {
    let mut v: Vec<f64> = samples
        .iter()
        .filter(|s| s.reachable)
        .map(|s| s.rtt.as_secs_f64() * 1000.0)
        .collect();
    if v.is_empty() {
        return None;
    }
    v.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
    let idx = ((v.len() as f64) * 0.95).floor() as usize;
    Some(v[idx.min(v.len() - 1)])
}

fn loss_pct(samples: &[&GatewayLatencyObservation]) -> f32 {
    if samples.is_empty() {
        return 0.0;
    }
    let lost = samples.iter().filter(|s| !s.reachable).count() as f32;
    100.0 * lost / samples.len() as f32
}

fn dns_median(samples: &[&DnsLatencyObservation]) -> Option<f64> {
    let mut v: Vec<f64> = samples
        .iter()
        .filter(|s| s.answered)
        .map(|s| s.rtt.as_secs_f64() * 1000.0)
        .collect();
    if v.is_empty() {
        return None;
    }
    v.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
    Some(v[v.len() / 2])
}

fn dns_fail_pct(samples: &[&DnsLatencyObservation]) -> f32 {
    if samples.is_empty() {
        return 0.0;
    }
    let bad = samples.iter().filter(|s| !s.answered).count() as f32;
    100.0 * bad / samples.len() as f32
}

fn sparkline_data<I: IntoIterator<Item = u64>>(values: I) -> Vec<u64> {
    values.into_iter().collect()
}
