//! The first-run **embedding gate** — a recommendation-first, three-screen flow
//! (ADR-0005 / the semantic-catalog design). Semantic discovery is an opt-in
//! add-on; rather than make the user be an embedding-model expert, we collect the
//! providers they have and recommend the single best model for *this job*
//! (description retrieval, ranked by MTEB), with the cost in plain
//! orders of magnitude. Browse is the escape hatch; `⎋` skips to lexical search.

use crate::app::{App, EmbedPhase};
use crate::llm::has_key;
use crate::theme;
use praxec_core::providers::ProviderId;
use ratatui::layout::{Constraint, Direction, Layout, Rect};
use ratatui::text::{Line, Span, Text};
use ratatui::widgets::{Block, Borders, Padding, Paragraph, Wrap};
use ratatui::Frame;

pub fn render_embedding_setup(f: &mut Frame, area: Rect, app: &App) {
    let title = match app.embed_phase {
        EmbedPhase::Providers => " ◇ Which providers do you have? ",
        EmbedPhase::ProviderKey => " ◇ Add an API key ",
        EmbedPhase::Recommend => " ◇ Recommended embedding model ",
        EmbedPhase::Browse => " ◇ Choose an embedding model ",
    };
    let card = centered(area, 74, 20);
    let block = Block::default()
        .borders(Borders::ALL)
        .border_style(theme::accent())
        .title(Span::styled(title, theme::accent()))
        .padding(Padding::new(2, 2, 1, 1));
    let inner = block.inner(card);
    f.render_widget(block, card);

    let rows = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(2),
            Constraint::Min(0),
            Constraint::Length(1),
        ])
        .split(inner);

    match app.embed_phase {
        EmbedPhase::Providers => providers(f, &rows, app),
        EmbedPhase::ProviderKey => provider_key(f, &rows, app),
        EmbedPhase::Recommend => recommend_view(f, &rows, app),
        EmbedPhase::Browse => browse(f, &rows, app),
    }
}

fn provider_display(slug: &str) -> &'static str {
    ProviderId::from_slug(slug)
        .map(|p| p.display())
        .unwrap_or("Provider")
}

fn providers(f: &mut Frame, rows: &[Rect], app: &App) {
    f.render_widget(
        Paragraph::new(Span::styled(
            "Add the API keys you have — I'll recommend the best model for your search.",
            theme::dim(),
        ))
        .wrap(Wrap { trim: true }),
        rows[0],
    );

    let provs = app.embed_providers();
    let mut lines: Vec<Line> = Vec::new();
    for (i, slug) in provs.iter().enumerate() {
        let selected = i == app.embed_cursor;
        let marker = if selected { "▸ " } else { "  " };
        let is_local = app
            .embed_options
            .iter()
            .any(|o| &o.vendor == slug && o.local);
        let status = if is_local {
            Span::styled("✓ local (no key)", theme::good())
        } else if has_key(slug) {
            Span::styled("✓ key set", theme::good())
        } else {
            Span::styled("⏎ add key", theme::dim())
        };
        lines.push(Line::from(vec![
            Span::styled(marker, theme::accent()),
            Span::styled(
                format!("{:<14}", provider_display(slug)),
                if selected {
                    theme::selected()
                } else {
                    theme::value()
                },
            ),
            status,
        ]));
    }
    // The "Continue" affordance.
    let cont_selected = app.embed_cursor >= provs.len();
    lines.push(Line::from(""));
    lines.push(Line::from(vec![
        Span::styled(if cont_selected { "▸ " } else { "  " }, theme::accent()),
        Span::styled(
            "→ Continue (recommend a model)",
            if cont_selected {
                theme::selected()
            } else {
                theme::value()
            },
        ),
    ]));
    f.render_widget(Paragraph::new(Text::from(lines)), rows[1]);

    f.render_widget(
        Paragraph::new(Span::styled(
            "↑↓ move · ⏎ add key / continue · ⎋ skip (lexical search)",
            theme::dim(),
        )),
        rows[2],
    );
}

fn provider_key(f: &mut Frame, rows: &[Rect], app: &App) {
    let provs = app.embed_providers();
    let slug = provs
        .get(app.embed_cursor)
        .map(String::as_str)
        .unwrap_or("");
    let env = ProviderId::from_slug(slug)
        .and_then(|p| p.credentials().primary())
        .unwrap_or("API key");
    f.render_widget(
        Paragraph::new(Span::styled(
            format!("Enter your {} API key ({env}).", provider_display(slug)),
            theme::dim(),
        )),
        rows[0],
    );
    let (text, style) = if app.embed_key_input.is_empty() {
        (format!("paste {env}"), theme::dim())
    } else {
        (
            format!("{}█", "•".repeat(app.embed_key_input.chars().count())),
            theme::value(),
        )
    };
    f.render_widget(
        Paragraph::new(Line::from(vec![
            Span::styled("› ", theme::accent()),
            Span::styled(text, style),
        ])),
        rows[1],
    );
    f.render_widget(
        Paragraph::new(Span::styled(
            "paste your API key · ⏎ save · ⎋ back",
            theme::dim(),
        )),
        rows[2],
    );
}

fn recommend_view(f: &mut Frame, rows: &[Rect], app: &App) {
    f.render_widget(
        Paragraph::new(Span::styled(
            "Best fit for semantic search across the providers you configured:",
            theme::dim(),
        ))
        .wrap(Wrap { trim: true }),
        rows[0],
    );

    let candidates = app.embed_candidates();
    let body = match praxec_embeddings::recommend(&candidates) {
        Some(opt) => {
            let why = praxec_embeddings::rationale(opt, &candidates);
            Text::from(vec![
                Line::from(Span::styled(
                    format!("★ {} · {}", opt.vendor, opt.model),
                    theme::selected(),
                )),
                Line::from(""),
                Line::from(Span::styled(why, theme::value())),
            ])
        }
        None => Text::from(Span::styled(
            "No reachable provider — add a key, or skip for lexical search.",
            theme::dim(),
        )),
    };
    f.render_widget(Paragraph::new(body).wrap(Wrap { trim: true }), rows[1]);

    f.render_widget(
        Paragraph::new(Span::styled(
            "⏎ use this · → other options · ⎋ skip · ← back",
            theme::dim(),
        )),
        rows[2],
    );
}

fn browse(f: &mut Frame, rows: &[Rect], app: &App) {
    f.render_widget(
        Paragraph::new(Span::styled(
            "Your providers' models, best retrieval quality first:",
            theme::dim(),
        )),
        rows[0],
    );
    let reachable = app.embed_reachable();
    let mut lines: Vec<Line> = Vec::new();
    for (i, o) in reachable.iter().enumerate() {
        let selected = i == app.embed_cursor.min(reachable.len().saturating_sub(1));
        let marker = if selected { "▸ " } else { "  " };
        let mag = praxec_embeddings::cost_magnitude(o).label();
        lines.push(Line::from(vec![
            Span::styled(marker, theme::accent()),
            Span::styled(
                format!("{} · {}", o.vendor, o.model),
                if selected {
                    theme::selected()
                } else {
                    theme::value()
                },
            ),
            Span::styled(
                format!("   MTEB {:.0} · {}", o.mteb_score, mag),
                theme::dim(),
            ),
        ]));
    }
    f.render_widget(Paragraph::new(Text::from(lines)), rows[1]);
    f.render_widget(
        Paragraph::new(Span::styled("↑↓ pick · ⏎ use · ⎋ back", theme::dim())),
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
    use crate::app::App;
    use ratatui::backend::TestBackend;
    use ratatui::Terminal;

    fn render(app: &App, w: u16, h: u16) -> String {
        let backend = TestBackend::new(w, h);
        let mut terminal = Terminal::new(backend).unwrap();
        terminal
            .draw(|f| render_embedding_setup(f, f.area(), app))
            .unwrap();
        crate::ui::buffer_to_string(terminal.backend().buffer())
    }

    #[test]
    fn providers_screen_lists_providers_and_continue() {
        let mut app = App::new();
        app.embedding_gate = true;
        app.embed_phase = EmbedPhase::Providers;
        let s = render(&app, 100, 24);
        assert!(s.contains("OpenAI"));
        assert!(s.contains("OpenRouter"));
        assert!(s.contains("Continue"));
        assert!(s.contains("skip"));
    }

    #[test]
    fn recommend_screen_shows_one_pick_with_a_cost_magnitude() {
        let mut app = App::new();
        app.embedding_gate = true;
        app.embed_phase = EmbedPhase::Recommend;
        let s = render(&app, 100, 24);
        assert!(s.contains("★"), "a single bold recommendation");
        assert!(s.contains("MTEB"), "rationale cites the benchmark");
        assert!(
            s.contains("a day") || s.contains("local"),
            "cost shown as an order of magnitude"
        );
    }
}
