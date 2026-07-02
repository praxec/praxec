//! The Run body — a master-detail cockpit for the Status facet:
//!
//! - **left**: the dynamic "Needs You" queue (highest priority → top-left, the
//!   first thing the eye lands on). Present only when something's pending.
//! - **center**: the live mission **tree** — pure structure + state (glyph +
//!   name + state word), an over-learned schema (file-explorer / CI-pipeline).
//!   Motion (a per-executor spinner) marks only running nodes.
//! - **right**: the **inspector** — full detail of the *selected* node
//!   (executor, harness, waits, actions). This is the progressive disclosure:
//!   the tree stays minimal, detail appears on selection.

use crate::app::App;
use crate::theme;
use crate::view::{
    ExecutorKind, Hitl, MissionView, NodeRole, NodeState, Speaker, TaskNode, Verdict,
};
use ratatui::layout::{Constraint, Direction, Layout, Rect};
use ratatui::style::Style;
use ratatui::text::{Line, Span, Text};
use ratatui::widgets::{Block, Borders, Padding, Paragraph, Wrap};
use ratatui::Frame;

const STATUS_COL: usize = 26;

fn spinner_frames(kind: Option<ExecutorKind>) -> &'static [&'static str] {
    match kind {
        Some(ExecutorKind::Llm) => &["⠁", "⠃", "⠇", "⠧", "⠷", "⠿", "⠷", "⠧", "⠇", "⠃"], // thinking
        Some(ExecutorKind::Agent) => &["◐", "◓", "◑", "◒"],                             // turning
        Some(ExecutorKind::Tool) => &["⣾", "⣽", "⣻", "⢿", "⡿", "⣟", "⣯", "⣷"],          // churning
        Some(ExecutorKind::Script) => &["|", "/", "-", "\\"],
        None => &["⠋", "⠙", "⠹", "⠸", "⠼", "⠴", "⠦", "⠧", "⠇", "⠏"], // container
    }
}

pub fn render_run(f: &mut Frame, area: Rect, app: &App) {
    let Some(m) = app.mission.as_ref() else {
        render_front_door(f, area);
        return;
    };
    // COCKPIT-02 — `Status` (the Tree) is the only advertised Run facet, so this
    // is always the surface to draw. No "— coming soon" placeholder remains: the
    // nav can't select a facet that doesn't render.

    let needs = m.needs_you_with_context();
    let has_needs = !needs.is_empty();

    // Drilled into an ask → master-detail: the compact list stays on the left
    // for context, the ask's full detail takes the middle (replacing the tree).
    if let Some(di) = app.drilled_ask {
        let cols = Layout::default()
            .direction(Direction::Horizontal)
            .constraints([Constraint::Length(28), Constraint::Min(20)])
            .split(area);
        render_needs_list(f, cols[0], &needs, app);
        render_ask_detail(f, cols[1], &needs, di, app);
        return;
    }

    // Default → [needs list?] [tree] [inspector]. The list is now COMPACT (one
    // line per ask); detail lives behind a drill-in, a consistent abstraction.
    let selected = m.nth_selectable(app.selected);
    let mut constraints = Vec::new();
    if has_needs {
        constraints.push(Constraint::Length(28));
    }
    constraints.push(Constraint::Min(20));
    constraints.push(Constraint::Length(38));
    let cols = Layout::default()
        .direction(Direction::Horizontal)
        .constraints(constraints)
        .split(area);

    let mut i = 0;
    if has_needs {
        render_needs_list(f, cols[i], &needs, app);
        i += 1;
    }
    render_tree(f, cols[i], app, m);
    i += 1;
    render_inspector(f, cols[i], selected);
}

// ── center: the tree (structure + state only) ───────────────────────────────

fn render_tree(f: &mut Frame, area: Rect, app: &App, m: &MissionView) {
    let mut lines: Vec<Line> = Vec::new();
    let mut idx = 0usize;
    let n = m.nodes.len();
    for (i, node) in m.nodes.iter().enumerate() {
        build_node(&mut lines, node, "", i + 1 == n, app, &mut idx);
    }
    let block = Block::default().padding(Padding::new(1, 1, 0, 0));
    f.render_widget(Paragraph::new(Text::from(lines)).block(block), area);
}

fn build_node(
    lines: &mut Vec<Line<'static>>,
    node: &TaskNode,
    prefix: &str,
    is_last: bool,
    app: &App,
    idx: &mut usize,
) {
    let selected = *idx == app.selected;
    let connector = if is_last { "└ " } else { "├ " };
    let (glyph, glyph_style) = glyph_for(node.state, node.kind, app.tick);
    let left = format!("{prefix}{connector}{glyph} {}", node.name);
    let pad = STATUS_COL.saturating_sub(left.chars().count()).max(1);
    let name_style = if selected {
        // Bright cursor when the tree has focus; calm marker when focus is in the nav.
        if app.focus == crate::app::Focus::Tree {
            theme::selected()
        } else {
            theme::selected_dim()
        }
    } else {
        name_style_for(node.state, node.role)
    };

    let mut spans = vec![
        Span::raw(format!("{prefix}{connector}")),
        Span::styled(format!("{glyph} "), glyph_style),
        Span::styled(node.name.clone(), name_style),
        Span::raw(" ".repeat(pad)),
        Span::styled(state_word(node.state), status_style(node.state)),
    ];
    // Task-spine markers: on the critical path (⏱) and parallel batch (⟂).
    if node.role == NodeRole::Task {
        if node.critical {
            spans.push(Span::styled("  ⏱".to_string(), theme::accent()));
        }
        if let Some(p) = &node.parallel_with {
            spans.push(Span::styled(format!("  ⟂{p}"), theme::dim()));
        }
    }
    if !node.children.is_empty() {
        spans.push(Span::raw("  "));
        spans.push(Span::styled(
            if node.expanded { "▾" } else { "▸" }.to_string(),
            theme::dim(),
        ));
    }
    lines.push(Line::from(spans));
    *idx += 1;

    if node.expanded {
        let child_prefix = format!("{prefix}{}", if is_last { "  " } else { "│ " });
        let cn = node.children.len();
        for (i, c) in node.children.iter().enumerate() {
            build_node(lines, c, &child_prefix, i + 1 == cn, app, idx);
        }
    }
}

// ── left: the compact asks list (master) ────────────────────────────────────

/// The "Needs You" master list — one terse line per ask (kind tag + name), at
/// the same abstraction level as a tree node. No question, no choices here:
/// those live behind a drill-in, so the top level stays low-overhead.
fn render_needs_list(f: &mut Frame, area: Rect, needs: &[(String, &TaskNode)], app: &App) {
    let focused = app.focus == crate::app::Focus::Needs;
    let border = if focused {
        theme::state_needs()
    } else {
        theme::border()
    };
    let block = Block::default()
        .borders(Borders::ALL)
        .border_style(border)
        .title(Span::styled(" ◆ NEEDS YOU ", theme::state_needs()))
        .padding(Padding::new(1, 1, 0, 0));

    let mut lines: Vec<Line> = Vec::new();
    for (i, (_crumb, node)) in needs.iter().enumerate() {
        let tag = node.hitl.as_ref().map(|h| h.tag()).unwrap_or("DO");
        let is_open = app.drilled_ask == Some(i);
        let is_sel = focused && i == app.needs_selected;
        let name_style = if is_sel {
            theme::selected()
        } else {
            theme::value()
        };
        lines.push(Line::from(vec![
            Span::styled(if is_open { "▸ " } else { "  " }, theme::accent()),
            Span::styled(format!("[{tag}] "), theme::accent()),
            Span::styled(node.name.clone(), name_style),
        ]));
    }
    lines.push(Line::from(""));
    lines.push(Line::from(Span::styled("⏎ open", theme::dim())));
    f.render_widget(Paragraph::new(Text::from(lines)).block(block), area);
}

// ── middle: the drilled-in ask detail (the "do the ask" workspace) ───────────

/// The full ask, in context: a breadcrumb of where it came from, the embedded
/// conversation thread (the discuss lives *here*, not handed off), quick-reply
/// choices, and a live free-text reply field. This is the consistent-
/// abstraction drill-in the top-level list points into.
fn render_ask_detail(
    f: &mut Frame,
    area: Rect,
    needs: &[(String, &TaskNode)],
    di: usize,
    app: &App,
) {
    let Some((crumb, node)) = needs.get(di) else {
        return;
    };
    let tag = node.hitl.as_ref().map(|h| h.tag()).unwrap_or("DO");
    let block = Block::default()
        .borders(Borders::ALL)
        .border_style(theme::state_needs())
        .title(Span::styled(
            format!(" {tag} · {} ", node.name),
            theme::panel_title(),
        ))
        .padding(Padding::new(1, 1, 0, 0));

    let mut lines: Vec<Line> = Vec::new();
    // Breadcrumb: where the ask came from.
    if !crumb.is_empty() {
        lines.push(Line::from(vec![
            Span::styled("from   ", theme::label()),
            Span::styled(crumb.clone(), theme::dim()),
        ]));
        lines.push(Line::from(""));
    }

    // The embedded conversation thread (agent ◂ / you ▸). For kinds without a
    // thread (form / approve), fall back to the kind's prompt.
    if node.thread.is_empty() {
        match node.hitl.as_ref() {
            Some(Hitl::Form { fields }) => {
                lines.push(Line::from(Span::styled("fill in:", theme::dim())));
                for fld in fields {
                    lines.push(Line::from(Span::styled(
                        format!("  {fld}  ▁▁▁▁▁▁▁▁"),
                        theme::value(),
                    )));
                }
            }
            Some(Hitl::Approve) => {
                lines.push(Line::from(Span::styled(
                    "approve these edits?",
                    theme::value(),
                )));
            }
            Some(Hitl::Discuss { topic }) => {
                lines.push(Line::from(Span::styled(topic.clone(), theme::value())));
            }
            Some(Hitl::Answer { question }) => {
                lines.push(Line::from(Span::styled(question.clone(), theme::value())));
            }
            None => {}
        }
    } else {
        for turn in &node.thread {
            let (who, who_style) = match turn.speaker {
                Speaker::Agent => ("agent ", theme::accent()),
                Speaker::You => ("you   ", theme::good()),
            };
            lines.push(Line::from(vec![
                Span::styled(who, who_style),
                Span::styled(turn.text.clone(), theme::value()),
            ]));
            lines.push(Line::from(""));
        }
    }

    // Quick-reply choices (selectable shortcuts for common answers).
    for (i, a) in node.actions.iter().enumerate() {
        let is_sel = app.focus == crate::app::Focus::AskDetail && i == app.ask_choice;
        let style = if is_sel {
            theme::selected()
        } else {
            theme::value()
        };
        let arrow = if is_sel {
            theme::selected()
        } else {
            theme::accent()
        };
        lines.push(Line::from(vec![
            Span::styled("  → ", arrow),
            Span::styled(a.clone(), style),
        ]));
    }
    lines.push(Line::from(""));

    // The live reply field — type a free-text answer; ⏎ sends it.
    let (reply_text, reply_style) = if app.reply.is_empty() {
        ("type a reply…".to_string(), theme::dim())
    } else {
        (format!("{}█", app.reply), theme::value())
    };
    lines.push(Line::from(vec![
        Span::styled("› ", theme::accent()),
        Span::styled(reply_text, reply_style),
    ]));
    lines.push(Line::from(""));
    lines.push(Line::from(Span::styled(
        "type a reply · ⏎ send · ↑↓ quick-reply · ⎋ back",
        theme::dim(),
    )));
    f.render_widget(
        Paragraph::new(Text::from(lines))
            .block(block)
            .wrap(Wrap { trim: true }),
        area,
    );
}

// ── right: the inspector (detail of the selected node) ───────────────────────

fn render_inspector(f: &mut Frame, area: Rect, node: Option<&TaskNode>) {
    let title = node
        .map(|n| format!(" {} ", n.name))
        .unwrap_or_else(|| " — ".into());
    let block = Block::default()
        .borders(Borders::ALL)
        .border_style(theme::border())
        .title(Span::styled(title, theme::panel_title()))
        .padding(Padding::new(1, 1, 0, 0));

    let Some(node) = node else {
        f.render_widget(Paragraph::new("").block(block), area);
        return;
    };

    let mut lines: Vec<Line> = Vec::new();
    lines.push(kv(
        "state",
        state_word(node.state),
        status_style(node.state),
    ));
    // Task-spine detail (the plan view of a deliverable).
    if node.role == NodeRole::Task {
        if node.critical {
            lines.push(kv("path", "on critical path".into(), theme::accent()));
        }
        if let Some(p) = &node.parallel_with {
            lines.push(kv("parallel", format!("with {p}"), theme::value()));
        }
        if let Some(s) = &node.scope {
            lines.push(kv("owns", s.clone(), theme::value()));
        }
    }
    if let Some(d) = &node.detail {
        lines.push(kv("executor", d.clone(), theme::value()));
        if let Some(k) = node.kind {
            lines.push(kv("kind", kind_label(k).into(), theme::dim()));
        }
    }
    for h in &node.harness {
        let (sym, style, extra) = match &h.verdict {
            Verdict::Ok => ("✓", theme::good(), String::new()),
            Verdict::Warn(m) => ("⚠", theme::warn(), format!(" {m}")),
        };
        lines.push(kv("harness", format!("{} {sym}{extra}", h.name), style));
    }
    if !node.waits_on.is_empty() {
        lines.push(kv(
            "waits",
            node.waits_on.join(", "),
            theme::state_blocked(),
        ));
    }
    if !node.actions.is_empty() {
        lines.push(Line::from(""));
        lines.push(Line::from(Span::styled("your move", theme::state_needs())));
        for a in &node.actions {
            lines.push(Line::from(Span::styled(format!("  → {a}"), theme::value())));
        }
    }
    f.render_widget(Paragraph::new(Text::from(lines)).block(block), area);
}

fn kv(k: &str, v: String, vstyle: Style) -> Line<'static> {
    Line::from(vec![
        Span::styled(format!("{k:<9}"), theme::label()),
        Span::styled(v, vstyle),
    ])
}

// ── shared ───────────────────────────────────────────────────────────────────

fn glyph_for(state: NodeState, kind: Option<ExecutorKind>, tick: u64) -> (String, Style) {
    match state {
        NodeState::Done => ("✓".into(), theme::state_done()),
        NodeState::Running => {
            let frames = spinner_frames(kind);
            (
                frames[(tick as usize) % frames.len()].into(),
                theme::state_running(),
            )
        }
        NodeState::NeedsYou => ("◆".into(), theme::state_needs()),
        NodeState::Blocked => ("⏸".into(), theme::state_blocked()),
        NodeState::Pending => ("○".into(), theme::state_pending()),
        NodeState::Failed => ("✗".into(), theme::state_failed()),
    }
}

fn name_style_for(state: NodeState, role: NodeRole) -> Style {
    // The task spine stays legible at every state; agents/steps recede when
    // done or not yet started, so the eye tracks what's live.
    if role == NodeRole::Task {
        return match state {
            NodeState::NeedsYou => theme::state_needs(),
            NodeState::Blocked => theme::state_blocked(),
            _ => theme::value(),
        };
    }
    match state {
        NodeState::Done | NodeState::Pending => theme::dim(),
        NodeState::NeedsYou => theme::state_needs(),
        _ => theme::value(),
    }
}

fn state_word(state: NodeState) -> String {
    match state {
        NodeState::Done => "done",
        NodeState::Running => "running",
        NodeState::NeedsYou => "needs you",
        NodeState::Blocked => "blocked",
        NodeState::Pending => "pending",
        NodeState::Failed => "failed",
    }
    .into()
}

fn status_style(state: NodeState) -> Style {
    match state {
        NodeState::NeedsYou => theme::state_needs(),
        NodeState::Blocked => theme::state_blocked(),
        NodeState::Running => theme::state_running(),
        NodeState::Failed => theme::state_failed(),
        _ => theme::dim(),
    }
}

fn kind_label(kind: ExecutorKind) -> &'static str {
    match kind {
        ExecutorKind::Llm => "llm",
        ExecutorKind::Agent => "agent",
        ExecutorKind::Tool => "tool",
        ExecutorKind::Script => "script",
    }
}

fn render_front_door(f: &mut Frame, area: Rect) {
    let block = Block::default()
        .borders(Borders::ALL)
        .border_style(theme::border())
        .title(Span::styled(" Mission ", theme::panel_title()))
        .padding(Padding::new(2, 2, 1, 1));
    let text = Text::from(vec![
        Line::from(Span::styled(
            "A deterministic harness for nondeterministic intelligence.",
            theme::dim(),
        )),
        Line::from(""),
        Line::from(Span::styled(
            "What are you trying to build or change?",
            theme::value(),
        )),
        Line::from(Span::styled(
            "› ______________________________________________",
            theme::accent(),
        )),
    ]);
    f.render_widget(Paragraph::new(text).block(block), area);
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::app::App;
    use crate::view::MissionView;
    use ratatui::backend::TestBackend;
    use ratatui::Terminal;

    fn render_to_string(app: &App) -> String {
        let backend = TestBackend::new(120, 28);
        let mut terminal = Terminal::new(backend).unwrap();
        terminal
            .draw(|f| {
                let area = f.area();
                render_run(f, area, app);
            })
            .unwrap();
        crate::ui::buffer_to_string(terminal.backend().buffer())
    }

    #[test]
    fn tree_spine_is_top_level_tasks() {
        let s = render_to_string(&App::new().with_mission(MissionView::demo()));
        assert!(s.contains("D4 · untangle delegate:"));
    }

    #[test]
    fn task_marks_the_critical_path() {
        let s = render_to_string(&App::new().with_mission(MissionView::demo()));
        assert!(s.contains('⏱'));
    }

    #[test]
    fn expanding_a_task_reveals_its_agent() {
        let mut app = App::new().with_mission(MissionView::demo());
        app.selected = 2; // D4 (collapsed by default)
        app.on_key(crate::app::Key::Right); // expand
        let s = render_to_string(&app);
        assert!(s.contains("backend-engineer"));
    }

    #[test]
    fn collapsed_task_hides_its_agent() {
        let s = render_to_string(&App::new().with_mission(MissionView::demo()));
        assert!(!s.contains("backend-engineer"));
    }

    #[test]
    fn needs_you_queue_is_on_the_left() {
        let s = render_to_string(&App::new().with_mission(MissionView::demo()));
        assert!(s.contains("NEEDS YOU"));
    }

    #[test]
    fn needs_list_tags_the_hitl_kind() {
        let s = render_to_string(&App::new().with_mission(MissionView::demo()));
        assert!(s.contains("[ASK]"));
    }

    #[test]
    fn top_level_list_keeps_the_question_behind_the_drill_in() {
        let s = render_to_string(&App::new().with_mission(MissionView::demo()));
        assert!(!s.contains("shared with"));
    }

    #[test]
    fn drilling_into_an_ask_shows_its_breadcrumb_context() {
        let mut app = App::new().with_mission(MissionView::demo());
        app.on_key(crate::app::Key::Enter); // open the asks list
        app.on_key(crate::app::Key::Enter); // drill into the ask
        let s = render_to_string(&app);
        assert!(s.contains("backend-engineer")); // breadcrumb: task › agent
    }

    fn app_selecting_a_step() -> App {
        let mut app = App::new().with_mission(MissionView::demo());
        app.selected = 2; // D4
        app.on_key(crate::app::Key::Right); // expand → backend-engineer
        app.selected = 3; // backend-engineer
        app.on_key(crate::app::Key::Right); // expand → its steps
        app.selected = 4; // refactor.delegate-untangle (first child step)
        app
    }

    #[test]
    fn inspector_shows_selected_steps_executor() {
        let s = render_to_string(&app_selecting_a_step());
        assert!(s.contains("refactor.delegate-untangle"));
    }

    #[test]
    fn inspector_shows_selected_steps_harness() {
        let s = render_to_string(&app_selecting_a_step());
        assert!(s.contains("god-file"));
    }

    #[test]
    fn inspector_shows_task_owns_scope() {
        let mut app = App::new().with_mission(MissionView::demo());
        app.selected = 2; // D4 (a task)
        let s = render_to_string(&app);
        assert!(s.contains("on critical path"));
    }

    #[test]
    fn front_door_when_no_mission() {
        let s = render_to_string(&App::new());
        assert!(s.contains("What are you trying to build or change?"));
    }
}
