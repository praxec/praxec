//! The first-run **chat gate** (ADR-0005 §5) — recommendation-first, mirroring
//! the embedding gate. The conductor drives the cockpit by *calling tools*, so
//! tool-calling is a hard filter; among the reachable tool-callers we recommend
//! the single best one **for the user's stance** (`crate::priorities`), with the
//! reasoning effort and the cost magnitude at an adjustable requests/day shown
//! inline. Browse is the escape hatch. A chat model is required, so there's no
//! skip.

use crate::app::{App, ChatPhase};
use crate::chat_catalog;
use crate::llm::has_key;
use crate::theme;
use praxec_core::providers::ProviderId;
use ratatui::Frame;
use ratatui::layout::{Constraint, Direction, Layout, Rect};
use ratatui::text::{Line, Span, Text};
use ratatui::widgets::{Block, Borders, Padding, Paragraph, Wrap};

pub fn render_chat_setup(f: &mut Frame, area: Rect, app: &App) {
    let title = match app.chat_phase {
        ChatPhase::Providers => " ◇ Which providers do you have? ",
        ChatPhase::ProviderKey => " ◇ Add an API key ",
        ChatPhase::Recommend => " ◇ Recommended assistant model ",
        ChatPhase::Browse => " ◇ Choose an assistant model ",
    };
    let card = centered(area, 76, 20);
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

    match app.chat_phase {
        ChatPhase::Providers => providers(f, &rows, app),
        ChatPhase::ProviderKey => provider_key(f, &rows, app),
        ChatPhase::Recommend => recommend_view(f, &rows, app),
        ChatPhase::Browse => browse(f, &rows, app),
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
            "Mission Control runs on an assistant model. Add the keys you have — \
             I'll recommend the best one for the job.",
            theme::dim(),
        ))
        .wrap(Wrap { trim: true }),
        rows[0],
    );

    let provs = app.chat_providers();
    let mut lines: Vec<Line> = Vec::new();
    for (i, slug) in provs.iter().enumerate() {
        let selected = i == app.chat_cursor;
        let marker = if selected { "▸ " } else { "  " };
        let is_local = app
            .chat_options
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
    let cont_selected = app.chat_cursor >= provs.len();
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
            "↑↓ move · ⏎ add key / continue · ^C quit",
            theme::dim(),
        )),
        rows[2],
    );
}

fn provider_key(f: &mut Frame, rows: &[Rect], app: &App) {
    let provs = app.chat_providers();
    let slug = provs.get(app.chat_cursor).map(String::as_str).unwrap_or("");
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
    let (text, style) = if app.chat_key_input.is_empty() {
        (format!("paste {env}"), theme::dim())
    } else {
        (
            format!("{}█", "•".repeat(app.chat_key_input.chars().count())),
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
            format!(
                "Best value for your '{}' stance (it must call tools):",
                app.priorities.stance.label()
            ),
            theme::dim(),
        ))
        .wrap(Wrap { trim: true }),
        rows[0],
    );

    let body = match app.chat_recommended() {
        Some(opt) => {
            let reqs = app.chat_requests_per_day();
            let mag = chat_catalog::chat_cost_magnitude(opt, reqs, &app.chat_reasoning).label();
            let why = chat_catalog::chat_rationale(opt, reqs, &app.chat_reasoning);
            Text::from(vec![
                Line::from(vec![
                    Span::styled(
                        format!("★ {} · {}", opt.vendor, opt.model),
                        theme::selected(),
                    ),
                    Span::styled("   calls tools ✓", theme::good()),
                ]),
                Line::from(Span::styled(
                    format!(
                        "   Intelligence {:.0} · ~{:.0} tok/s",
                        opt.intelligence, opt.speed_tps
                    ),
                    theme::dim(),
                )),
                Line::from(""),
                Line::from(vec![
                    Span::styled("   ↑↓ effort: ", theme::dim()),
                    Span::styled(format!("‹ {} ›", app.chat_reasoning), theme::value()),
                    Span::styled("    ←→ requests/day: ", theme::dim()),
                    Span::styled(format!("‹ ~{} ›", fmt_count(reqs)), theme::value()),
                ]),
                Line::from(vec![
                    Span::styled("   cost: ", theme::dim()),
                    Span::styled(mag.to_string(), theme::value()),
                ]),
                Line::from(""),
                Line::from(Span::styled(why, theme::dim())),
            ])
        }
        None => Text::from(Span::styled(
            "No reachable tool-calling model — add a provider key on the previous screen.",
            theme::dim(),
        )),
    };
    f.render_widget(Paragraph::new(body).wrap(Wrap { trim: true }), rows[1]);

    f.render_widget(
        Paragraph::new(Span::styled(
            "⏎ use · b browse (more capable) · ↑↓ effort · ←→ requests/day · ⎋ back",
            theme::dim(),
        )),
        rows[2],
    );
}

fn browse(f: &mut Frame, rows: &[Rect], app: &App) {
    f.render_widget(
        Paragraph::new(Span::styled(
            "Your providers' tool-calling models, most capable first:",
            theme::dim(),
        )),
        rows[0],
    );
    let reachable = app.chat_reachable();
    let reqs = app.chat_requests_per_day();
    let mut lines: Vec<Line> = Vec::new();
    for (i, o) in reachable.iter().enumerate() {
        let selected = i == app.chat_cursor.min(reachable.len().saturating_sub(1));
        let marker = if selected { "▸ " } else { "  " };
        let mag =
            chat_catalog::chat_cost_magnitude(o, reqs, &chat_catalog::default_reasoning(o)).label();
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
                format!(
                    "   int {:.0} · {:.0} tok/s · best: {} · {}",
                    o.intelligence,
                    o.speed_tps,
                    best_at(o),
                    mag
                ),
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

/// The affinity this model scores highest in — its headline strength.
fn best_at(o: &chat_catalog::ChatModelOption) -> String {
    chat_catalog::Affinity::ALL
        .iter()
        .copied()
        .max_by(|a, b| {
            a.score(&o.scores, o.intelligence)
                .partial_cmp(&b.score(&o.scores, o.intelligence))
                .unwrap_or(std::cmp::Ordering::Equal)
        })
        .map(|a| a.to_string())
        .unwrap_or_else(|| "general".to_string())
}

fn fmt_count(n: usize) -> String {
    if n >= 1000 {
        format!("{}k", n / 1000)
    } else {
        n.to_string()
    }
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

    fn render(app: &App, w: u16, h: u16) -> String {
        let backend = ratatui::backend::TestBackend::new(w, h);
        let mut terminal = ratatui::Terminal::new(backend).unwrap();
        terminal
            .draw(|f| render_chat_setup(f, f.area(), app))
            .unwrap();
        crate::ui::buffer_to_string(terminal.backend().buffer())
    }

    #[test]
    fn providers_screen_lists_providers_and_continue() {
        let mut app = App::new();
        app.chat_model = None;
        app.chat_phase = ChatPhase::Providers;
        let s = render(&app, 100, 24);
        assert!(s.contains("OpenAI"));
        assert!(s.contains("Anthropic"));
        assert!(s.contains("Continue"));
    }

    #[test]
    fn recommend_screen_surfaces_the_effort_and_volume_controls() {
        // The footer controls render regardless of which providers are reachable
        // (keeping the test hermetic — no dependency on machine provider keys).
        let mut app = App::new();
        app.chat_model = None;
        app.chat_phase = ChatPhase::Recommend;
        let s = render(&app, 100, 24);
        assert!(s.contains("effort"));
        assert!(s.contains("requests/day"));
        assert!(s.contains("browse"));
        // The stance is named so the recommendation reads as "for *your* goal".
        assert!(s.contains("stance"));
    }
}
