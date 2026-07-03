//! The **priorities panel** — "what matters most to you", the lens every model
//! recommendation is made through (ADR-0005 / the semantic-catalog design).
//!
//! Rather than ask users to introspect numeric weights they don't have, we offer
//! a few stances they recognise (the radio list) plus the two hard constraints
//! that actually filter the field (a budget ceiling and local-only). Pick a
//! stance, watch later recommendations move. The same renderer serves the
//! first-run gate and the Settings entry — only the footer differs.

use crate::app::App;
use crate::priorities::{Stance, budget_label};
use crate::theme;
use ratatui::Frame;
use ratatui::layout::{Constraint, Direction, Layout, Rect};
use ratatui::text::{Line, Span, Text};
use ratatui::widgets::{Block, Borders, Padding, Paragraph, Wrap};

const STANCE_ROWS: usize = Stance::ALL.len();
const BUDGET_ROW: usize = STANCE_ROWS;
const LOCAL_ROW: usize = STANCE_ROWS + 1;
const CONTINUE_ROW: usize = STANCE_ROWS + 2;

/// Render the panel into `area`. `first_run` only changes the footer hint.
pub fn render_priorities_setup(f: &mut Frame, area: Rect, app: &App, first_run: bool) {
    let card = centered(area, 74, 18);
    let block = Block::default()
        .borders(Borders::ALL)
        .border_style(theme::accent())
        .title(Span::styled(
            " ◇ What matters most for your models? ",
            theme::accent(),
        ))
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
        Paragraph::new(Span::styled(
            "Pick a stance — every model recommendation is made through it.",
            theme::dim(),
        ))
        .wrap(Wrap { trim: true }),
        rows[0],
    );

    let cur = app.prio_cursor;
    let mut lines: Vec<Line> = Vec::new();

    // The stance radio list.
    for (i, stance) in Stance::ALL.iter().enumerate() {
        let on_cursor = cur == i;
        let selected = app.priorities.stance == *stance;
        let marker = if on_cursor { "▸ " } else { "  " };
        let radio = if selected { "● " } else { "○ " };
        let name_style = if on_cursor {
            theme::selected()
        } else if selected {
            theme::value()
        } else {
            theme::dim()
        };
        lines.push(Line::from(vec![
            Span::styled(marker, theme::accent()),
            Span::styled(
                radio,
                if selected {
                    theme::accent()
                } else {
                    theme::dim()
                },
            ),
            Span::styled(format!("{:<18}", stance.label()), name_style),
            Span::styled(stance.blurb(), theme::dim()),
        ]));
    }

    lines.push(Line::from(""));

    // The two hard constraints.
    lines.push(constraint_line(
        cur == BUDGET_ROW,
        "Budget ceiling",
        &format!("‹ {} ›", budget_label(app.priorities.budget_cap)),
    ));
    lines.push(constraint_line(
        cur == LOCAL_ROW,
        "Local / private only",
        if app.priorities.local_only {
            "☑"
        } else {
            "☐"
        },
    ));

    lines.push(Line::from(""));

    // Continue.
    let cont = cur == CONTINUE_ROW;
    lines.push(Line::from(vec![
        Span::styled(if cont { "▸ " } else { "  " }, theme::accent()),
        Span::styled(
            "→ Continue",
            if cont {
                theme::selected()
            } else {
                theme::value()
            },
        ),
    ]));

    f.render_widget(Paragraph::new(Text::from(lines)), rows[1]);

    let hint = if first_run {
        "↑↓ move · ⏎ choose · ←→ adjust · ⎋ skip (Balanced)"
    } else {
        "↑↓ move · ⏎ choose · ←→ adjust · ⎋ back to Settings"
    };
    f.render_widget(Paragraph::new(Span::styled(hint, theme::dim())), rows[2]);
}

fn constraint_line(on_cursor: bool, label: &str, value: &str) -> Line<'static> {
    let value_style = if on_cursor {
        theme::selected()
    } else {
        theme::value()
    };
    Line::from(vec![
        Span::styled(if on_cursor { "▸ " } else { "  " }, theme::accent()),
        Span::styled(format!("{label}: "), theme::dim()),
        Span::styled(value.to_string(), value_style),
    ])
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
    use crate::priorities::Stance;

    fn render(app: &App, first_run: bool) -> String {
        let backend = ratatui::backend::TestBackend::new(100, 24);
        let mut terminal = ratatui::Terminal::new(backend).unwrap();
        terminal
            .draw(|f| render_priorities_setup(f, f.area(), app, first_run))
            .unwrap();
        crate::ui::buffer_to_string(terminal.backend().buffer())
    }

    #[test]
    fn lists_all_four_stances_and_the_constraints() {
        let app = App::new();
        let s = render(&app, true);
        assert!(s.contains("Balanced"));
        assert!(s.contains("Best results"));
        assert!(s.contains("Keep costs low"));
        assert!(s.contains("Fastest responses"));
        assert!(s.contains("Budget ceiling"));
        assert!(s.contains("Local / private only"));
        assert!(s.contains("skip (Balanced)")); // first-run footer
    }

    #[test]
    fn marks_the_selected_stance() {
        let mut app = App::new();
        app.priorities.stance = Stance::Fastest;
        let s = render(&app, false);
        assert!(s.contains("●")); // a radio is filled
        assert!(s.contains("back to Settings")); // settings footer
    }
}
