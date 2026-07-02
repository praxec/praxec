//! The **Settings** overlay — the persistent home for the choices the first-run
//! gates make (ADR-0005). Opened with `g`. A three-row menu (Priorities / Chat
//! model / Embedding model); each row ⏎ into the *same* surface the gate uses, so
//! "first-run when unset" and "editable forever" are one code path.

use crate::app::App;
use crate::theme;
use ratatui::layout::{Constraint, Direction, Layout, Rect};
use ratatui::text::{Line, Span, Text};
use ratatui::widgets::{Block, Borders, Padding, Paragraph};
use ratatui::Frame;

pub fn render_settings(f: &mut Frame, area: Rect, app: &App) {
    // Drilled into the priorities sub-panel → render that (shared renderer).
    if app.settings_editing_priorities {
        crate::ui::priorities_setup::render_priorities_setup(f, area, app, false);
        return;
    }

    let card = centered(area, 70, 14);
    let block = Block::default()
        .borders(Borders::ALL)
        .border_style(theme::accent())
        .title(Span::styled(" ⚙ Settings ", theme::accent()))
        .padding(Padding::new(2, 2, 1, 1));
    let inner = block.inner(card);
    f.render_widget(block, card);

    let rows = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(1),
            Constraint::Min(0),
            Constraint::Length(1),
        ])
        .split(inner);

    f.render_widget(
        Paragraph::new(Span::styled("Change these any time.", theme::dim())),
        rows[0],
    );

    // Current values shown inline so the menu reads as status, not just options.
    let stance = app.priorities.stance.label();
    let chat = app
        .chat_model
        .as_ref()
        .map(|m| format!("{} · {}", m.vendor, m.model))
        .unwrap_or_else(|| "not set".to_string());
    let embed = app
        .embedding
        .as_ref()
        .map(|e| format!("{} · {}", e.vendor, e.model))
        .unwrap_or_else(|| "lexical (none)".to_string());

    let items = [
        ("Priorities", stance.to_string()),
        ("Chat model", chat),
        ("Embedding model", embed),
    ];
    let mut lines: Vec<Line> = Vec::new();
    for (i, (label, value)) in items.iter().enumerate() {
        let on = i == app.settings_cursor;
        lines.push(Line::from(vec![
            Span::styled(if on { "▸ " } else { "  " }, theme::accent()),
            Span::styled(
                format!("{label:<18}"),
                if on {
                    theme::selected()
                } else {
                    theme::value()
                },
            ),
            Span::styled(value.clone(), theme::dim()),
        ]));
    }
    f.render_widget(Paragraph::new(Text::from(lines)), rows[1]);

    f.render_widget(
        Paragraph::new(Span::styled("↑↓ move · ⏎ open · ⎋ close", theme::dim())),
        rows[2],
    );
}

/// A `w`×`h` rect centred in `area` (clamped to fit).
fn centered(area: Rect, w: u16, h: u16) -> Rect {
    let w = w.min(area.width);
    let h = h.min(area.height);
    Rect {
        x: area.x + (area.width - w) / 2,
        y: area.y + (area.height - h) / 2,
        width: w,
        height: h,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn render(app: &App) -> String {
        let backend = ratatui::backend::TestBackend::new(100, 24);
        let mut terminal = ratatui::Terminal::new(backend).unwrap();
        terminal
            .draw(|f| render_settings(f, f.area(), app))
            .unwrap();
        crate::ui::buffer_to_string(terminal.backend().buffer())
    }

    #[test]
    fn menu_lists_the_three_settings_with_current_values() {
        let mut app = App::new();
        app.settings_open = true;
        let s = render(&app);
        assert!(s.contains("Priorities"));
        assert!(s.contains("Balanced")); // the current stance value
        assert!(s.contains("Chat model"));
        assert!(s.contains("Embedding model"));
    }

    #[test]
    fn drilling_into_priorities_shows_the_panel() {
        let mut app = App::new();
        app.settings_open = true;
        app.settings_editing_priorities = true;
        let s = render(&app);
        assert!(s.contains("What matters most"));
        assert!(s.contains("back to Settings"));
    }
}
