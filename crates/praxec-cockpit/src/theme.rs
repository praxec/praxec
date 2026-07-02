//! Cockpit color theme. Basic ANSI colors for broad terminal compatibility;
//! a richer palette can come later behind the same accessors.

use ratatui::style::{Color, Modifier, Style};

/// Bold brand accent (the product mark, panel titles).
pub fn brand() -> Style {
    Style::default()
        .fg(Color::Cyan)
        .add_modifier(Modifier::BOLD)
}

/// Panel title text.
pub fn panel_title() -> Style {
    Style::default()
        .fg(Color::Cyan)
        .add_modifier(Modifier::BOLD)
}

/// Panel border lines.
pub fn border() -> Style {
    Style::default().fg(Color::DarkGray)
}

/// De-emphasized text (meta, dividers, hints).
pub fn dim() -> Style {
    Style::default().fg(Color::DarkGray)
}

/// Field labels.
pub fn label() -> Style {
    Style::default().fg(Color::Gray)
}

/// Primary field values.
pub fn value() -> Style {
    Style::default().fg(Color::White)
}

/// Interactive accent (prompts, links, the live affordance surface).
pub fn accent() -> Style {
    Style::default().fg(Color::Cyan)
}

/// Warning / needs-attention.
pub fn warn() -> Style {
    Style::default().fg(Color::Yellow)
}

/// Healthy / passed.
pub fn good() -> Style {
    Style::default().fg(Color::Green)
}

/// The selected tab (mode toggle).
pub fn active_tab() -> Style {
    Style::default()
        .fg(Color::Black)
        .bg(Color::Cyan)
        .add_modifier(Modifier::BOLD)
}

/// Unselected tabs.
pub fn inactive_tab() -> Style {
    Style::default().fg(Color::Gray)
}

/// ADR-0008 — the badge style for a mission resolution status. Four colors:
/// running (cyan, advancing), waiting (yellow, needs input), succeeded (green),
/// failed (red). Unknown values fall back to gray.
pub fn mission_status(status: &str) -> Style {
    let color = match status {
        "running" => Color::Cyan,
        "waiting" => Color::Yellow,
        "succeeded" => Color::Green,
        "failed" => Color::Red,
        _ => Color::Gray,
    };
    Style::default().fg(color).add_modifier(Modifier::BOLD)
}

// ── node-state palette (the tree) ───────────────────────────────────────────

/// Completed — green check, but the row text recedes (data-ink: done work
/// shouldn't compete for attention).
pub fn state_done() -> Style {
    Style::default().fg(Color::Green)
}
/// Actively running (the spinner).
pub fn state_running() -> Style {
    Style::default().fg(Color::Cyan)
}
/// Awaiting the human — the single loudest thing on screen.
pub fn state_needs() -> Style {
    Style::default()
        .fg(Color::Magenta)
        .add_modifier(Modifier::BOLD)
}
/// Waiting on upstream work.
pub fn state_blocked() -> Style {
    Style::default().fg(Color::Yellow)
}
pub fn state_pending() -> Style {
    Style::default().fg(Color::DarkGray)
}
pub fn state_failed() -> Style {
    Style::default().fg(Color::Red)
}
/// The currently-selected tree row, when the tree has focus (bright cursor).
pub fn selected() -> Style {
    Style::default()
        .fg(Color::Black)
        .bg(Color::Cyan)
        .add_modifier(Modifier::BOLD)
}

/// The selected tree row when focus is elsewhere (where you'll return to).
pub fn selected_dim() -> Style {
    Style::default().fg(Color::Cyan)
}

/// The current nav facet when the nav does NOT have focus.
pub fn nav_current() -> Style {
    Style::default()
        .fg(Color::Cyan)
        .add_modifier(Modifier::BOLD)
}
