//! The **real** mission view (L1) — the HATEOAS affordance surface a running
//! workflow actually exposes (ADR-0005 / "make it real"). A praxec workflow is
//! a state machine: it's at one `state`, with a set of legal next transitions
//! (the `links`), guidance, and a context blackboard. This renders exactly that —
//! the same surface the model sees — replacing the demo task-tree fiction.

use crate::model::GatewayResponse;
use crate::theme;
use ratatui::Frame;
use ratatui::layout::{Constraint, Direction, Layout, Rect};
use ratatui::text::{Line, Span, Text};
use ratatui::widgets::{Block, Borders, Padding, Paragraph, Wrap};

/// Render a live mission from its gateway response. `action_cursor` selects among
/// the legal next actions.
pub fn render_mission(f: &mut Frame, area: Rect, resp: &GatewayResponse, action_cursor: usize) {
    let block = Block::default()
        .borders(Borders::ALL)
        .border_style(theme::border())
        .title(Span::styled(
            format!(
                " {} · {} ",
                resp.workflow.definition_id, resp.workflow.state
            ),
            theme::panel_title(),
        ))
        .padding(Padding::new(1, 1, 0, 0));
    let inner = block.inner(area);
    f.render_widget(block, area);

    let rows = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Min(3), Constraint::Length(1)])
        .split(inner);

    let mut lines: Vec<Line> = Vec::new();

    // Status line: where this mission stands — a colored badge (ADR-0008
    // running/waiting/succeeded/failed), the fail reason when failed, the version.
    let status = &resp.result.status;
    let glyph = match status.as_str() {
        "succeeded" => "✓",
        "failed" => "✗",
        _ => "●", // running / waiting (color carries the distinction)
    };
    let mut status_spans = vec![Span::styled(
        format!("{glyph} {}", status.to_uppercase()),
        theme::mission_status(status),
    )];
    if let Some(reason) = &resp.result.reason {
        status_spans.push(Span::styled(format!("  ({reason})"), theme::dim()));
    }
    status_spans.push(Span::styled(
        format!("   v{}", resp.workflow.version),
        theme::dim(),
    ));
    lines.push(Line::from(status_spans));
    if let Some(summary) = &resp.summary {
        lines.push(Line::from(Span::styled(summary.clone(), theme::dim())));
    }

    // Outcomes — the mission's measurable definition of done (ADR-0008), each
    // with a live met/unmet mark. The deterministic "are we there yet".
    if !resp.outcomes.is_empty() {
        lines.push(Line::from(""));
        let met = resp.outcomes.iter().filter(|o| o.met).count();
        lines.push(Line::from(Span::styled(
            format!("outcomes  {met}/{} met", resp.outcomes.len()),
            theme::accent(),
        )));
        for oc in &resp.outcomes {
            let (mark, style) = if oc.met {
                ("✓ ", theme::good())
            } else {
                ("○ ", theme::dim())
            };
            lines.push(Line::from(vec![
                Span::styled(mark, style),
                Span::styled(
                    oc.statement.clone(),
                    if oc.met { theme::value() } else { theme::dim() },
                ),
            ]));
        }
    }

    // Guidance — the goal + the bounds, verbatim from the workflow.
    if let Some(g) = &resp.guidance {
        lines.push(Line::from(""));
        if let Some(goal) = &g.goal {
            lines.push(Line::from(vec![
                Span::styled("goal  ", theme::label()),
                Span::styled(goal.clone(), theme::value()),
            ]));
        }
        if let Some(instr) = &g.instructions {
            lines.push(Line::from(vec![
                Span::styled("bound ", theme::label()),
                Span::styled(instr.clone(), theme::dim()),
            ]));
        }
    }

    // The legal next actions — the HATEOAS link surface. The human and the model
    // share this exact list (who acts is the `actor`).
    lines.push(Line::from(""));
    let actions = resp.legal_actions();
    lines.push(Line::from(Span::styled(
        if actions.is_empty() {
            "no legal actions (terminal state)"
        } else {
            "legal next actions"
        },
        theme::accent(),
    )));
    for (i, link) in actions.iter().enumerate() {
        let selected = i == action_cursor.min(actions.len().saturating_sub(1));
        let marker = if selected { "▸ " } else { "  " };
        let title = link.title.clone().unwrap_or_else(|| link.rel.clone());
        let actor = link.actor.clone().unwrap_or_else(|| "—".to_string());
        lines.push(Line::from(vec![
            Span::styled(marker, theme::accent()),
            Span::styled(
                format!("{title:<28}"),
                if selected {
                    theme::selected()
                } else {
                    theme::value()
                },
            ),
            Span::styled(format!("{:<18}", link.rel), theme::dim()),
            Span::styled(format!("[{actor}]"), theme::dim()),
        ]));
    }

    f.render_widget(
        Paragraph::new(Text::from(lines)).wrap(Wrap { trim: true }),
        rows[0],
    );
    f.render_widget(
        Paragraph::new(Span::styled(
            "↑↓ choose · ⏎ submit · ⎋ zoom out · these are the moves the model sees too",
            theme::dim(),
        )),
        rows[1],
    );
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::gateway::{FakeGateway, Gateway};
    use ratatui::Terminal;
    use ratatui::backend::TestBackend;

    fn render(resp: &GatewayResponse) -> String {
        let backend = TestBackend::new(90, 24);
        let mut terminal = Terminal::new(backend).unwrap();
        terminal
            .draw(|f| render_mission(f, f.area(), resp, 0))
            .unwrap();
        crate::ui::buffer_to_string(terminal.backend().buffer())
    }

    #[test]
    fn renders_status_badge_and_outcomes_checklist() {
        let json = serde_json::json!({
            "workflow": { "id": "wf1", "definitionId": "flow.x", "state": "verifying", "version": 3 },
            "result": { "status": "failed", "reason": "guard_unmet" },
            "outcomes": [
                { "id": "verified", "statement": "Patch passes verification.", "met": true },
                { "id": "signed",   "statement": "Human signed off.",          "met": false }
            ],
            "links": []
        });
        let resp: GatewayResponse = serde_json::from_value(json).unwrap();
        let s = render(&resp);
        assert!(s.contains("FAILED"), "status badge label; got:\n{s}");
        assert!(s.contains("guard_unmet"), "fail reason; got:\n{s}");
        assert!(s.contains("1/2 met"), "outcomes tally; got:\n{s}");
        assert!(
            s.contains("Patch passes verification"),
            "met outcome; got:\n{s}"
        );
        assert!(s.contains("Human signed off"), "unmet outcome; got:\n{s}");
    }

    #[test]
    fn shows_state_guidance_and_the_legal_actions() {
        let resp = FakeGateway::editing_demo()
            .get("wf_safe_refactor_01")
            .unwrap();
        let s = render(&resp);
        assert!(s.contains("editing")); // the real state
        assert!(s.contains("safe-refactor")); // the definition id
        assert!(s.contains("legal next actions"));
        assert!(s.contains("Edits complete")); // a real link title
        assert!(s.contains("Request review"));
        assert!(s.contains("[human]")); // the actor surfaces who acts
        assert!(s.contains("smallest safe extraction")); // the goal
    }
}
