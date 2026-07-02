//! The chat rail — the spine of the conversational cockpit (ADR-0005). A
//! scrollable transcript with an Oatmeal-style pinned input at the bottom. You
//! type intent here; it drives the map (the stage) and narrates back.

use crate::app::App;
use crate::theme;
use ratatui::layout::{Constraint, Direction, Layout, Rect};
use ratatui::text::{Line, Span, Text};
use ratatui::widgets::{Block, Borders, Padding, Paragraph, Wrap};
use ratatui::Frame;

pub fn render_chat(f: &mut Frame, area: Rect, app: &App) {
    let block = Block::default()
        .borders(Borders::ALL)
        .border_style(theme::accent())
        .title(Span::styled(" ◇ Mission Control ", theme::accent()))
        .padding(Padding::new(1, 1, 0, 0));
    let inner = block.inner(area);
    f.render_widget(block, area);

    // Transcript (fills) + a pinned two-line input at the bottom.
    let rows = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Min(0), Constraint::Length(2)])
        .split(inner);

    // Transcript — show the most recent turns (tail), each as who + text.
    let mut lines: Vec<Line> = Vec::new();
    for turn in &app.chat_log {
        let (who, who_style) = if turn.you {
            ("you  ", theme::good())
        } else {
            ("mc   ", theme::accent())
        };
        lines.push(Line::from(vec![
            Span::styled(who, who_style),
            Span::styled(turn.text.clone(), theme::value()),
        ]));
        lines.push(Line::from(""));
    }
    // The reply as it streams in: show the partial text live (with a cursor)
    // once tokens arrive; until then, a "thinking" spinner.
    if let Some(partial) = &app.streaming {
        lines.push(Line::from(vec![
            Span::styled("mc   ", theme::accent()),
            Span::styled(format!("{partial}█"), theme::value()),
        ]));
        lines.push(Line::from(""));
    } else if app.thinking {
        const FRAMES: [char; 10] = ['⠋', '⠙', '⠹', '⠸', '⠼', '⠴', '⠦', '⠧', '⠇', '⠏'];
        let frame = FRAMES[(app.tick as usize) % FRAMES.len()];
        lines.push(Line::from(vec![
            Span::styled("mc   ", theme::accent()),
            Span::styled(format!("{frame} thinking…"), theme::dim()),
        ]));
        lines.push(Line::from(""));
    }
    // Pin the newest content to the bottom: a chat transcript grows downward, so
    // scroll past the overflow instead of clipping the latest messages off-screen.
    let transcript = Paragraph::new(Text::from(lines)).wrap(Wrap { trim: true });
    let total = transcript.line_count(rows[0].width) as u16;
    let scroll = total.saturating_sub(rows[0].height);
    f.render_widget(transcript.scroll((scroll, 0)), rows[0]);

    // Pinned input.
    let (text, style) = if app.chat_input.is_empty() {
        ("type to drive the map…".to_string(), theme::dim())
    } else {
        (format!("{}█", app.chat_input), theme::value())
    };
    f.render_widget(
        Paragraph::new(Line::from(vec![
            Span::styled("› ", theme::accent()),
            Span::styled(text, style),
        ])),
        rows[1],
    );
}
