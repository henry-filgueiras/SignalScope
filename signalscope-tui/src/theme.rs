//! Minimal palette — restrained on purpose. SignalScope should feel like a
//! flight panel, not a hacker movie.

use ratatui::style::{Color, Modifier, Style};

pub const TITLE_FG: Color = Color::Rgb(200, 220, 255);
pub const FRAME_FG: Color = Color::Rgb(80, 100, 120);
pub const LABEL_FG: Color = Color::Rgb(150, 160, 175);
pub const VALUE_FG: Color = Color::Rgb(230, 235, 245);
pub const DIM_FG: Color = Color::Rgb(110, 120, 135);

pub const OK_FG: Color = Color::Rgb(120, 200, 140);
pub const WARN_FG: Color = Color::Rgb(230, 195, 120);
pub const BAD_FG: Color = Color::Rgb(220, 110, 110);
pub const INFO_FG: Color = Color::Rgb(130, 180, 230);

pub fn title_style() -> Style {
    Style::default()
        .fg(TITLE_FG)
        .add_modifier(Modifier::BOLD)
}

pub fn label() -> Style {
    Style::default().fg(LABEL_FG)
}

pub fn value() -> Style {
    Style::default().fg(VALUE_FG)
}

pub fn dim() -> Style {
    Style::default().fg(DIM_FG)
}

pub fn frame() -> Style {
    Style::default().fg(FRAME_FG)
}

/// Color a numeric quality value: lower is better for latency, higher is
/// better for RSSI. Callers pass a normalized 0.0..=1.0 "goodness" score.
pub fn quality_color(goodness: f32) -> Color {
    if goodness >= 0.66 {
        OK_FG
    } else if goodness >= 0.33 {
        WARN_FG
    } else {
        BAD_FG
    }
}
