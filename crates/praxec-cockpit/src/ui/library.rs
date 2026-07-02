//! Build mode's library browse — the real layered library the gateway serves
//! (SPEC §32 discovery), the read half of authoring. The active Build facet
//! (`Sources`, `Flows`, `Capabilities`, …) filters the listing by kind;
//! `Sources` shows everything grouped by the owning repo namespace. Editing is
//! chat-conducted (the LLM drives the authoring workflow through
//! `praxec.command`) or done on-disk via git — this surface visualizes what's
//! there to act on.

use crate::app::App;
use crate::model::LibraryEntry;
use crate::theme;
use ratatui::layout::{Constraint, Direction, Layout, Rect};
use ratatui::text::{Line, Span, Text};
use ratatui::widgets::{Block, Borders, Padding, Paragraph, Wrap};
use ratatui::Frame;

/// The kind filter for a Build facet label. `None` means "all" (the `Sources`
/// overview). Unknown facets fall through to all.
fn facet_kind(facet: &str) -> Option<&'static str> {
    match facet {
        "Flows" => Some("workflow"),
        "Capabilities" => Some("capability"),
        "Skills" => Some("skill"),
        "Tools" => Some("tool"),
        "Connections" => Some("connection"),
        "Agents" => Some("agent"),
        _ => None, // "Sources" and anything else: the whole library.
    }
}

/// The entries shown under the active facet, in the library's stable order.
fn visible_entries<'a>(app: &'a App, facet: &str) -> Vec<&'a LibraryEntry> {
    match facet_kind(facet) {
        Some(kind) => app.library.iter().filter(|e| e.kind == kind).collect(),
        None => app.library.iter().collect(),
    }
}

/// Count of entries under the active facet — the app bounds its browse cursor
/// against this.
pub(crate) fn visible_len(app: &App, facet: &str) -> usize {
    visible_entries(app, facet).len()
}

/// The `n`-th entry under the active facet — the row ⏎ acts on.
pub(crate) fn nth_visible<'a>(app: &'a App, facet: &str, n: usize) -> Option<&'a LibraryEntry> {
    visible_entries(app, facet).into_iter().nth(n)
}

/// The opened-definition detail (⏎ on a row): its id, content hash, and body,
/// with the chat spine taking the edit from here.
fn render_detail(f: &mut Frame, area: Rect, detail: &crate::model::DefinitionDetail) {
    let block = Block::default()
        .borders(Borders::ALL)
        .border_style(theme::border())
        .title(Span::styled(
            format!(" {} ", detail.definition_id),
            theme::panel_title(),
        ))
        .padding(Padding::new(1, 1, 0, 0));
    let inner = block.inner(area);
    f.render_widget(block, area);

    let rows = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(2),
            Constraint::Min(2),
            Constraint::Length(1),
        ])
        .split(inner);

    f.render_widget(
        Paragraph::new(vec![
            Line::from(vec![
                Span::styled("hash  ", theme::label()),
                Span::styled(detail.hash.clone(), theme::dim()),
            ]),
            Line::from(Span::styled(
                "the edit basis — the publish guard rejects if this moves",
                theme::dim(),
            )),
        ]),
        rows[0],
    );

    let body = serde_json::to_string_pretty(&detail.definition).unwrap_or_default();
    f.render_widget(
        Paragraph::new(Text::from(body)).wrap(Wrap { trim: false }),
        rows[1],
    );
    f.render_widget(
        Paragraph::new(Span::styled(
            "type your change in the chat → the model edits via the workflow · ↑↓ back to the library",
            theme::accent(),
        )),
        rows[2],
    );
}

pub fn render_library(f: &mut Frame, area: Rect, app: &App, facet: &str) {
    // ⏎ opened a definition — show its body + hash (the edit basis).
    if let Some(detail) = app.library_detail.as_ref() {
        render_detail(f, area, detail);
        return;
    }
    let block = Block::default()
        .borders(Borders::ALL)
        .border_style(theme::border())
        .title(Span::styled(format!(" {facet} "), theme::panel_title()))
        .padding(Padding::new(1, 1, 0, 0));
    let inner = block.inner(area);
    f.render_widget(block, area);

    let rows = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Min(3), Constraint::Length(1)])
        .split(inner);

    let entries = visible_entries(app, facet);

    if app.library.is_empty() {
        f.render_widget(
            Paragraph::new(Span::styled(
                "No library yet — connect a gateway, or couldn't reach it.",
                theme::dim(),
            ))
            .wrap(Wrap { trim: true }),
            rows[0],
        );
        return;
    }

    let mut lines: Vec<Line> = Vec::new();

    // A count header so the operator sees the breadth of the layered library.
    lines.push(Line::from(Span::styled(
        format!(
            "{} definition{} from {} source{}",
            app.library.len(),
            if app.library.len() == 1 { "" } else { "s" },
            source_count(app),
            if source_count(app) == 1 { "" } else { "s" },
        ),
        theme::dim(),
    )));
    lines.push(Line::from(""));

    if entries.is_empty() {
        lines.push(Line::from(Span::styled(
            format!("nothing under {facet} yet"),
            theme::dim(),
        )));
    }

    let cursor = app.library_cursor.min(entries.len().saturating_sub(1));
    for (i, entry) in entries.iter().enumerate() {
        let selected = i == cursor;
        let marker = if selected { "▸ " } else { "  " };
        let source = entry.namespace().unwrap_or("local");
        lines.push(Line::from(vec![
            Span::styled(marker, theme::accent()),
            Span::styled(
                format!("{:<34}", entry.id),
                if selected {
                    theme::selected()
                } else {
                    theme::value()
                },
            ),
            Span::styled(format!("{:<12}", entry.kind), theme::dim()),
            Span::styled(format!("@{source}"), theme::dim()),
        ]));
        // The selected row expands its title/description for context.
        if selected && !entry.title.is_empty() {
            let blurb = if entry.description.is_empty() {
                entry.title.clone()
            } else {
                format!("{} — {}", entry.title, entry.description)
            };
            lines.push(Line::from(vec![
                Span::raw("    "),
                Span::styled(blurb, theme::dim()),
            ]));
        }
    }

    f.render_widget(
        Paragraph::new(Text::from(lines)).wrap(Wrap { trim: true }),
        rows[0],
    );
    f.render_widget(
        Paragraph::new(Span::styled(
            "↑↓ browse · ⏎ launch · ^E edit · ⇥ run · the same library the model discovers",
            theme::dim(),
        )),
        rows[1],
    );
}

/// Distinct source namespaces represented in the library.
fn source_count(app: &App) -> usize {
    let mut seen: Vec<&str> = app
        .library
        .iter()
        .map(|e| e.namespace().unwrap_or("local"))
        .collect();
    seen.sort_unstable();
    seen.dedup();
    seen.len()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::app::{App, Mode};
    use crate::gateway::{FakeGateway, Gateway};
    use ratatui::backend::TestBackend;
    use ratatui::Terminal;

    fn app_with_library() -> App {
        let mut app = App::new();
        app.mode = Mode::Build;
        app.library = FakeGateway::editing_demo().library().unwrap();
        app
    }

    fn render(app: &App, facet: &str) -> String {
        let backend = TestBackend::new(96, 24);
        let mut terminal = Terminal::new(backend).unwrap();
        terminal
            .draw(|f| render_library(f, f.area(), app, facet))
            .unwrap();
        crate::ui::buffer_to_string(terminal.backend().buffer())
    }

    #[test]
    fn sources_facet_lists_the_whole_layered_library() {
        let app = app_with_library();
        let s = render(&app, "Sources");
        assert!(s.contains("cognitive/flow.safe-refactor"));
        assert!(s.contains("acme/flow.deploy"));
        assert!(s.contains("@cognitive")); // the owning source namespace
        assert!(s.contains("@acme"));
        assert!(s.contains("4 definitions from 3 sources"));
    }

    #[test]
    fn flows_facet_filters_to_workflows() {
        let app = app_with_library();
        let s = render(&app, "Flows");
        assert!(s.contains("cognitive/flow.safe-refactor"));
        // A capability must NOT appear under Flows.
        assert!(!s.contains("cap.plan.vet"));
    }

    #[test]
    fn empty_library_shows_an_explicit_state() {
        let mut app = App::new();
        app.mode = Mode::Build;
        let s = render(&app, "Sources");
        assert!(s.contains("No library yet"));
    }

    #[test]
    fn ctrl_e_opens_the_selected_definition_and_seeds_an_edit() {
        use crate::app::Key;
        let mut app = app_with_library();
        app.conn = Some(Box::new(FakeGateway::editing_demo()));
        app.nav_index = 0; // Sources facet — all entries
        app.library_cursor = 0;
        app.on_key(Key::Edit); // Ctrl-E edits; ⏎ now launches a workflow

        let detail = app
            .library_detail
            .as_ref()
            .expect("Ctrl-E opened a definition");
        assert!(!detail.definition_id.is_empty());
        assert!(
            app.chat_input.starts_with("Edit "),
            "chat seeded for edit: {}",
            app.chat_input
        );

        // The body view shows the hash (the edit basis) and a chat-conducted hint.
        let s = render(&app, "Sources");
        assert!(s.contains("hash"));
        assert!(s.contains("edits via the workflow"));
    }
}
