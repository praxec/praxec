//! H9 / ADR-0009 — the **mediator inbox** overlay. Opened with `a` when the
//! "✋ N need you" chip is showing. The count chip alone was unactionable: the
//! human could *see* parked missions but had no way to *answer* them. This
//! overlay lists the parked missions and their human-actor choices and
//! dispatches the chosen transition via [`crate::app::App::answer_inbox`].

use crate::app::App;
use crate::theme;
use ratatui::layout::{Constraint, Direction, Layout, Rect};
use ratatui::text::{Line, Span, Text};
use ratatui::widgets::{Block, Borders, Padding, Paragraph, Wrap};
use ratatui::Frame;

pub fn render_inbox(f: &mut Frame, area: Rect, app: &App) {
    let card = centered(area, 76, 18);
    let block = Block::default()
        .borders(Borders::ALL)
        .border_style(theme::state_needs())
        .title(Span::styled(" ✋ Needs You ", theme::state_needs()))
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
            "Parked missions waiting on your call.",
            theme::dim(),
        )),
        rows[0],
    );

    let mut lines: Vec<Line> = Vec::new();
    if app.inbox.is_empty() {
        lines.push(Line::from(Span::styled(
            "Nothing waiting — you're all caught up.",
            theme::dim(),
        )));
    }
    for (i, item) in app.inbox.iter().enumerate() {
        let on_item = i == app.inbox_cursor;
        // The parked mission + what it's asking.
        lines.push(Line::from(vec![
            Span::styled(if on_item { "▸ " } else { "  " }, theme::accent()),
            Span::styled(
                item.definition_id.clone(),
                if on_item {
                    theme::selected()
                } else {
                    theme::value()
                },
            ),
            Span::styled(format!("  ({})", item.mission_id), theme::dim()),
        ]));
        if !item.prompt.is_empty() {
            lines.push(Line::from(vec![
                Span::raw("    "),
                Span::styled(item.prompt.clone(), theme::dim()),
            ]));
        }
        // The human-actor choices — only the selected item highlights one.
        let mut choice_spans = vec![Span::raw("    ")];
        for (c, choice) in item.choices.iter().enumerate() {
            let highlighted = on_item && c == app.inbox_choice;
            choice_spans.push(Span::styled(
                format!("[{choice}]"),
                if highlighted {
                    theme::selected()
                } else {
                    theme::accent()
                },
            ));
            choice_spans.push(Span::raw(" "));
        }
        lines.push(Line::from(choice_spans));
        lines.push(Line::from(""));
    }
    f.render_widget(
        Paragraph::new(Text::from(lines)).wrap(Wrap { trim: false }),
        rows[1],
    );

    f.render_widget(
        Paragraph::new(Span::styled(
            "↑↓ mission · ←→ choice · ⏎ answer · ⎋ close",
            theme::dim(),
        )),
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
    use crate::mediator::InboxItem;

    fn render(app: &App) -> String {
        let backend = ratatui::backend::TestBackend::new(100, 24);
        let mut terminal = ratatui::Terminal::new(backend).unwrap();
        terminal.draw(|f| render_inbox(f, f.area(), app)).unwrap();
        crate::ui::buffer_to_string(terminal.backend().buffer())
    }

    fn item(id: &str) -> InboxItem {
        InboxItem {
            mission_id: id.into(),
            definition_id: "cognitive/flow.safe-refactor".into(),
            version: 1,
            prompt: "Approve the edits?".into(),
            choices: vec!["approve".into(), "reject".into()],
        }
    }

    #[test]
    fn inbox_lists_parked_missions_and_their_choices() {
        let mut app = App::new();
        app.inbox = vec![item("m1")];
        app.inbox_open = true;
        let s = render(&app);
        assert!(s.contains("Needs You"));
        assert!(s.contains("safe-refactor"));
        assert!(s.contains("[approve]"));
        assert!(s.contains("[reject]"));
    }
}
