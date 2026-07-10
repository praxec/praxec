//! The persistent shell. Two-line header (mission name + status chips + mode),
//! a chunked navigable nav strip, the active facet body, and a single bottom
//! action bar that answers "what do I do here".

use crate::app::{App, Focus, Mode};
use crate::nav;
use crate::theme;
use crate::view::MissionView;
use ratatui::Frame;
use ratatui::layout::{Alignment, Constraint, Direction, Layout, Rect};
use ratatui::text::{Line, Span};
use ratatui::widgets::Paragraph;

pub fn render_shell(f: &mut Frame, app: &App) {
    let rows = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(1), // header: name + chips + mode
            Constraint::Length(1), // nav
            Constraint::Length(1), // rule
            Constraint::Min(3),    // body (active facet)
            Constraint::Length(1), // rule
            Constraint::Length(1), // action bar
        ])
        .split(f.area());

    render_header(f, rows[0], app);
    render_nav(f, rows[1], app);
    render_rule(f, rows[2]);
    render_body(f, rows[3], app);
    render_rule(f, rows[4]);
    render_action_bar(f, rows[5], app);
}

fn render_rule(f: &mut Frame, area: Rect) {
    let line = "─".repeat(area.width as usize);
    f.render_widget(Paragraph::new(Span::styled(line, theme::border())), area);
}

fn render_header(f: &mut Frame, area: Rect, app: &App) {
    // Three zones: mission name (fills), status chips, mode indicator (always
    // visible at the far right so the mode never truncates).
    let cols = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([
            Constraint::Min(0),
            Constraint::Length(40),
            Constraint::Length(14),
        ])
        .split(area);

    // Left: the human mission name (Run) or "Library" (Build).
    let left = match app.mode {
        Mode::Run => match app.mission.as_ref() {
            Some(m) => Line::from(vec![
                Span::styled(format!(" {}", m.name), theme::brand()),
                Span::styled(format!("   {}", m.orchestrator), theme::dim()),
            ]),
            None => Line::from(Span::styled(" no active mission", theme::dim())),
        },
        Mode::Build => Line::from(Span::styled(" Library", theme::brand())),
    };
    f.render_widget(Paragraph::new(left), cols[0]);

    // Center-right: status chips.
    let mut chips = Vec::new();
    // ADR-0009 — the mediator's cross-mission Needs-You count: always visible
    // (even at the fleet level, with no single active mission) so a parked
    // mission anywhere in the fleet surfaces in one place.
    if !app.inbox.is_empty() {
        chips.push(Span::styled(
            format!("✋ {} need you", app.inbox.len()),
            theme::state_needs(),
        ));
        chips.push(Span::raw("  "));
    }
    if app.mode == Mode::Run
        && let Some(m) = app.mission.as_ref()
    {
        let c = m.counts();
        chips.push(Span::styled(
            format!("● {} running", c.running),
            theme::good(),
        ));
        chips.push(Span::raw("  "));
        chips.push(Span::styled(
            format!("◷ {} blocked", c.blocked),
            theme::warn(),
        ));
        chips.push(Span::raw("  "));
        chips.push(Span::styled(
            format!("◆ {} needs you", c.needs_you),
            theme::accent(),
        ));
    }
    f.render_widget(
        Paragraph::new(Line::from(chips)).alignment(Alignment::Right),
        cols[1],
    );

    // Far right: the mode indicator (rare context switch via Tab).
    let mode_hint = match app.mode {
        Mode::Run => "run · ⇥ build",
        Mode::Build => "build · ⇥ run",
    };
    f.render_widget(
        Paragraph::new(Span::styled(mode_hint, theme::dim())).alignment(Alignment::Right),
        cols[2],
    );
}

fn render_nav(f: &mut Frame, area: Rect, app: &App) {
    let (items, chunks): (&[&str], &[usize]) = match app.mode {
        Mode::Run => (&nav::RUN, &nav::RUN_CHUNKS),
        Mode::Build => (&nav::BUILD, &nav::BUILD_CHUNKS),
    };

    let mut spans: Vec<Span> = Vec::new();
    for (i, label) in items.iter().enumerate() {
        if chunks.contains(&i) {
            spans.push(Span::styled(" │ ", theme::dim()));
        }
        let style = if i == app.nav_index {
            // Bright when the nav has focus; calmer "current" when focus is in the body.
            if app.focus == Focus::Nav {
                theme::active_tab()
            } else {
                theme::nav_current()
            }
        } else {
            theme::inactive_tab()
        };
        spans.push(Span::styled(format!(" {label} "), style));
    }

    let cols = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([Constraint::Min(0), Constraint::Length(12)])
        .split(area);
    f.render_widget(Paragraph::new(Line::from(spans)), cols[0]);
    f.render_widget(
        Paragraph::new(Span::styled("", theme::dim())).alignment(Alignment::Right),
        cols[1],
    );
}

/// Width of the persistent chat rail (the spine).
const CHAT_W: u16 = 40;

fn render_body(f: &mut Frame, area: Rect, app: &App) {
    match app.mode {
        // The priorities panel is the very first gate — the recommendation lens.
        Mode::Run if app.priorities_gate => {
            crate::ui::priorities_setup::render_priorities_setup(f, area, app, true)
        }
        // The Settings overlay (opened with `g`) floats over the cockpit.
        Mode::Run if app.settings_open => crate::ui::settings::render_settings(f, area, app),
        // H9 — the mediator inbox overlay (opened with `a`) floats over the
        // cockpit so the human can answer a parked mission, not just see it.
        Mode::Run if app.inbox_open => crate::ui::inbox::render_inbox(f, area, app),
        // The embedding gate is the bootstrap — shown only while undecided.
        Mode::Run if app.embedding_gate => {
            crate::ui::embedding_setup::render_embedding_setup(f, area, app)
        }
        // The first-run chat gate precedes the chat (ADR-0005).
        Mode::Run if app.chat_model.is_none() => {
            crate::ui::chat_setup::render_chat_setup(f, area, app)
        }
        Mode::Run => {
            // Two-region, chat-centric layout (ADR-0005): the stage (the map /
            // widgets) on the left, the persistent chat rail on the right.
            let cols = Layout::default()
                .direction(Direction::Horizontal)
                .constraints([Constraint::Min(40), Constraint::Length(CHAT_W)])
                .split(area);
            crate::ui::map_view::render_map(f, cols[0], app);
            crate::ui::chat::render_chat(f, cols[1], app);
        }
        Mode::Build => {
            // The layered library on the stage, the chat rail alongside —
            // authoring is chat-conducted (ADR-0005), so the spine stays put.
            let cols = Layout::default()
                .direction(Direction::Horizontal)
                .constraints([Constraint::Min(40), Constraint::Length(CHAT_W)])
                .split(area);
            let facet = nav::BUILD[app.nav_index.min(nav::BUILD.len() - 1)];
            crate::ui::library::render_library(f, cols[0], app, facet);
            crate::ui::chat::render_chat(f, cols[1], app);
        }
    }
}

fn render_action_bar(f: &mut Frame, area: Rect, app: &App) {
    let cols = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([Constraint::Min(0), Constraint::Length(40)])
        .split(area);

    let action = action_line(app);
    f.render_widget(Paragraph::new(action), cols[0]);
    // Altitude-aware keyboard hint so zooming in/out is discoverable.
    let hint = if app.mode != Mode::Run {
        "↑↓/←→ navigate · ⇥ mode · q quit"
    } else {
        match app.map.level {
            crate::map::Level::Fleet => "↑↓←→ pan · ⏎ zoom in · g settings · q quit",
            crate::map::Level::Mission => "⎋ zoom out · ↑↓ navigate · g settings · q quit",
        }
    };
    f.render_widget(
        Paragraph::new(Span::styled(hint, theme::dim())).alignment(Alignment::Right),
        cols[1],
    );
}

fn action_line(app: &App) -> Line<'static> {
    // On the Fleet map, tell the user how to traverse it (the "I can zoom into
    // anything" half of the map read-aloud test).
    if app.mode == Mode::Run && app.map.level == crate::map::Level::Fleet {
        return Line::from(Span::styled(
            " ↑↓←→ pan the fleet · ⏎ zoom into a mission",
            theme::accent(),
        ));
    }
    let Some(m) = app.mission.as_ref() else {
        return Line::from(Span::styled(
            " › What are you trying to build or change?",
            theme::accent(),
        ));
    };
    match needs_you(m) {
        Some((lane, action)) => Line::from(vec![
            Span::styled(" ◆ ", theme::accent()),
            Span::styled(format!("{} needs you", lane), theme::value()),
            Span::styled("   › ", theme::accent()),
            Span::styled(action, theme::value()),
            Span::styled("   ⏎ act", theme::dim()),
        ]),
        None => Line::from(Span::styled(
            " › Direct the mission…   all running",
            theme::dim(),
        )),
    }
}

/// (node name, first action) for the first tree node awaiting the human.
fn needs_you(m: &MissionView) -> Option<(String, String)> {
    m.first_needs_you()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::app::App;
    use crate::view::MissionView;
    use ratatui::Terminal;
    use ratatui::backend::TestBackend;

    fn render_to_string(app: &App) -> String {
        let backend = TestBackend::new(100, 26);
        let mut terminal = Terminal::new(backend).unwrap();
        terminal.draw(|f| render_shell(f, app)).unwrap();
        crate::ui::buffer_to_string(terminal.backend().buffer())
    }

    #[test]
    fn run_header_shows_mission_name_chips_and_mode_hint() {
        let app = App::new().with_mission(MissionView::demo());
        let s = render_to_string(&app);
        assert!(s.contains("Complete alignment + caching"));
        assert!(s.contains("running"));
        assert!(s.contains("needs you"));
        assert!(s.contains("build")); // mode hint
        assert!(s.contains("Status")); // nav
    }

    #[test]
    fn the_mediator_inbox_count_surfaces_in_the_header() {
        let mut app = App::new();
        app.inbox = vec![crate::mediator::InboxItem {
            mission_id: "m1".into(),
            definition_id: "f".into(),
            version: 1,
            prompt: "?".into(),
            choices: vec!["approve".into()],
        }];
        let s = render_to_string(&app);
        assert!(s.contains("1 need you"));
    }

    #[test]
    fn action_bar_points_at_the_node_that_needs_you() {
        let app = App::new().with_mission(MissionView::demo());
        let s = render_to_string(&app);
        assert!(s.contains("review-edits needs you"));
        assert!(s.contains("fold D5 in"));
    }

    #[test]
    fn build_mode_browses_the_layered_library() {
        let mut app = App::new();
        app.mode = Mode::Build;
        app.library =
            crate::gateway::Gateway::library(&crate::gateway::FakeGateway::editing_demo()).unwrap();
        let s = render_to_string(&app);
        assert!(s.contains("Sources")); // the Build nav
        assert!(s.contains("cognitive/flow.safe-refactor")); // a real library entry
        assert!(s.contains("@acme")); // a second source repo
    }
}
