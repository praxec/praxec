//! `praxec-cockpit` — Mission Control cockpit binary.
//!
//! `--demo` opens a synthetic multi-lane mission; `--snapshot` renders the
//! cockpit to text (no live terminal) for UI iteration.

use std::io::{self, Stdout};
use std::sync::mpsc;
use std::time::Duration;

use anyhow::Result;
use praxec_cockpit::agent::{self, AgentEvent, Turn};
use praxec_cockpit::app::{App, ChatLine, Key, Mode};
use praxec_cockpit::{snapshot, ui};
use ratatui::backend::CrosstermBackend;
use ratatui::crossterm::event::{self, Event, KeyCode, KeyEventKind, KeyModifiers};
use ratatui::crossterm::execute;
use ratatui::crossterm::terminal::{
    disable_raw_mode, enable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen,
};
use ratatui::Terminal;

/// The value following `flag` in `args` (e.g. `--workflow wf_x` → `Some("wf_x")`).
fn arg_value(args: &[String], flag: &str) -> Option<String> {
    args.iter()
        .position(|a| a == flag)
        .and_then(|i| args.get(i + 1))
        .cloned()
}

fn main() -> Result<()> {
    let args: Vec<String> = std::env::args().collect();

    if args.iter().any(|a| a == "--snapshot") {
        return print_snapshot();
    }

    // Load any saved provider keys into the env so aether-llm's `from_env`
    // provider construction finds them (mirrors the praxec CLI startup).
    if let Ok(keys_path) = praxec_core::provider_keys::resolve_path() {
        let _ = praxec_core::provider_keys::load_into_env_with(
            &keys_path,
            |k| std::env::var(k).ok(),
            // SAFETY: synchronous, at startup, before any task is spawned.
            |k, v| unsafe { std::env::set_var(k, v) },
        );
    }

    // Start at the first-run setup gate (assign a chat model) — pre-filled or
    // skipped if existing praxec config is detected — then the fleet map.
    let mut app = App::fresh_detecting();
    // The live cockpit drives commands through the Mission Control LLM.
    app.llm_enabled = true;

    // A tokio runtime runs each LLM turn off the UI thread; results return over
    // a channel the render loop drains each tick.
    let rt = tokio::runtime::Runtime::new()?;

    // Live mission: `--workflow <id>` (or $PRAXEC_WORKFLOW) connects the
    // StdioGateway to a running praxec and shows that workflow's real HATEOAS
    // surface. Absent (or on failure) we stay on the demo fleet, with a note.
    if let Some(workflow_id) =
        arg_value(&args, "--workflow").or_else(|| std::env::var("PRAXEC_WORKFLOW").ok())
    {
        let config = arg_value(&args, "--config").or_else(|| std::env::var("PRAXEC_CONFIG").ok());
        // Connect + fetch the initial response, KEEPING the gateway alive so ⏎ can
        // submit transitions later. The async fetch avoids a nested block_on.
        let connected = rt.block_on(async {
            let gw = praxec_cockpit::gateway::StdioGateway::connect(
                rt.handle().clone(),
                config.as_deref(),
            )
            .await?;
            let resp = gw.fetch(&workflow_id).await?;
            Ok::<_, anyhow::Error>((gw, resp))
        });
        match connected {
            Ok((gw, resp)) => {
                app.gateway = Some(resp);
                app.conn = Some(Box::new(gw));
                app.map.level = praxec_cockpit::map::Level::Mission;
                app.chat_log.push(praxec_cockpit::app::ChatLine {
                    you: false,
                    text: format!("Watching workflow {workflow_id}."),
                });
            }
            Err(e) => {
                app.chat_log.push(praxec_cockpit::app::ChatLine {
                    you: false,
                    text: format!(
                        "Couldn't reach workflow {workflow_id}: {e}. Showing the demo fleet."
                    ),
                });
            }
        }
    }

    let mut terminal = setup_terminal()?;
    let res = run_loop(&mut terminal, &mut app, &rt);
    restore_terminal(&mut terminal)?;
    res
}

/// Render the key states to text and print them — drives `--snapshot`.
fn print_snapshot() -> Result<()> {
    // (1) FLEET (L0): the whole map + the chat spine, with a command being typed.
    let mut fleet_app = App::new();
    fleet_app.chat_log.push(ChatLine {
        you: true,
        text: "what needs me".into(),
    });
    fleet_app.chat_log.push(ChatLine {
        you: false,
        text: "→ Complete alignment + caching (needs you).".into(),
    });
    for c in "postgres".chars() {
        fleet_app.on_key(Key::Char(c)); // a command mid-type
    }

    // (2) ZOOMING: the container transform mid-flight (~50%) — the destination
    // mission opening out of the selected tile, over the dimmed fleet.
    let mut zoom_app = App::new();
    zoom_app.on_key(Key::Enter); // begin zoom into the selected mission
    if let Some(t) = zoom_app.map.transition.as_mut() {
        t.progress = 0.28; // ease-out is fast early, so a small p shows a partial aperture
    }

    // (3) MISSION (L1): zoomed in — the task-spine tree, breadcrumb on Mission.
    let mut mission_app = App::new();
    mission_app.on_key(Key::Enter);
    mission_app.map.transition = None; // settle the transition

    // (0) SETUP GATE: first run — vendor → model → key. Shown at the key step
    // (state set directly so the snapshot is deterministic regardless of any
    // provider keys configured on the machine rendering it).
    use praxec_cockpit::app::ChatPhase;
    let mut setup_app = App::fresh();
    setup_app.chat_phase = ChatPhase::Providers; // recommendation-first chat gate

    // (0y) REAL MISSION — the live HATEOAS surface (state + legal actions),
    // here fed by the bundled fixture (the StdioGateway feeds it live).
    use praxec_cockpit::gateway::{FakeGateway, Gateway};
    use praxec_cockpit::map::Level;
    let mut real_mission = App::new();
    real_mission.gateway = FakeGateway::editing_demo().get("wf_safe_refactor_01").ok();
    real_mission.map.level = Level::Mission;

    let mut build_app = App::new();
    build_app.mode = Mode::Build;
    // The Build snapshot browses the real layered library (the fixture stands
    // in for a live gateway's `praxec.query` discovery).
    build_app.library = praxec_cockpit::gateway::FakeGateway::editing_demo()
        .library()
        .unwrap_or_default();

    // (0a) EMBEDDING GATE — providers screen + the single recommendation.
    use praxec_cockpit::app::EmbedPhase;
    let mut embed_providers = App::new();
    embed_providers.embedding = None;
    embed_providers.embedding_gate = true;
    embed_providers.embed_phase = EmbedPhase::Providers;
    let mut embed_recommend = App::new();
    embed_recommend.embedding = None;
    embed_recommend.embedding_gate = true;
    embed_recommend.embed_phase = EmbedPhase::Recommend;

    // (0z) PRIORITIES PANEL — the first gate (the recommendation lens) + Settings.
    let mut prio_app = App::new();
    prio_app.priorities_gate = true;
    let mut settings_app = App::new();
    settings_app.settings_open = true;

    println!("──────────────── PRIORITIES — what matters most (95x22) ────────────────");
    print!("{}", snapshot::render_to_text(&prio_app, 95, 22));
    println!("──────────────── SETTINGS — the persistent home, key `g` (95x22) ────────────────");
    print!("{}", snapshot::render_to_text(&settings_app, 95, 22));
    println!("──────────────── EMBEDDING GATE — providers (95x22) ────────────────");
    print!("{}", snapshot::render_to_text(&embed_providers, 95, 22));
    println!("──────────────── EMBEDDING GATE — recommendation (95x22) ────────────────");
    print!("{}", snapshot::render_to_text(&embed_recommend, 95, 22));
    println!("──────────────── CHAT GATE — pick an assistant model (120x20) ────────────────");
    print!("{}", snapshot::render_to_text(&setup_app, 120, 20));
    println!("──────────────── REAL MISSION — live HATEOAS surface (120x24) ────────────────");
    print!("{}", snapshot::render_to_text(&real_mission, 120, 24));
    println!("──────────────── FLEET — the map (120x30) ────────────────");
    print!("{}", snapshot::render_to_text(&fleet_app, 120, 30));
    println!("──────────────── ZOOMING — container transform ~50% (120x30) ────────────────");
    print!("{}", snapshot::render_to_text(&zoom_app, 120, 30));
    println!("──────────────── MISSION — zoomed in (120x30) ────────────────");
    print!("{}", snapshot::render_to_text(&mission_app, 120, 30));
    println!("──────────────── BUILD (110x28) ────────────────");
    print!("{}", snapshot::render_to_text(&build_app, 110, 28));
    Ok(())
}

fn setup_terminal() -> Result<Terminal<CrosstermBackend<Stdout>>> {
    enable_raw_mode()?;
    let mut stdout = io::stdout();
    execute!(stdout, EnterAlternateScreen)?;
    Ok(Terminal::new(CrosstermBackend::new(stdout))?)
}

fn restore_terminal(terminal: &mut Terminal<CrosstermBackend<Stdout>>) -> Result<()> {
    disable_raw_mode()?;
    execute!(terminal.backend_mut(), LeaveAlternateScreen)?;
    terminal.show_cursor()?;
    Ok(())
}

fn run_loop(
    terminal: &mut Terminal<CrosstermBackend<Stdout>>,
    app: &mut App,
    rt: &tokio::runtime::Runtime,
) -> Result<()> {
    let (tx, rx) = mpsc::channel::<AgentEvent>();
    while !app.should_quit {
        terminal.draw(|f| ui::render(f, app))?;
        // Draw ~60fps while a zoom transition is animating or a turn is in
        // flight (to spin the indicator); otherwise stay calm.
        let poll = if app.map.is_transitioning() || app.thinking {
            Duration::from_millis(16)
        } else {
            Duration::from_millis(120)
        };
        if event::poll(poll)? {
            if let Event::Key(key) = event::read()? {
                if key.kind == KeyEventKind::Press {
                    let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
                    let mapped = match key.code {
                        // Ctrl-C is the global quit (printable keys now type).
                        KeyCode::Char('c') if ctrl => Key::Quit,
                        // Ctrl-E edits the selected Build-mode definition (⏎ launches).
                        KeyCode::Char('e') if ctrl => Key::Edit,
                        KeyCode::Tab => Key::ToggleMode,
                        KeyCode::Enter => Key::Enter,
                        KeyCode::Up => Key::Up,
                        KeyCode::Down => Key::Down,
                        KeyCode::Right => Key::Right,
                        KeyCode::Left => Key::Left,
                        KeyCode::Backspace => Key::Backspace,
                        KeyCode::Esc => Key::Escape,
                        // Printable keys feed the always-on chat input.
                        KeyCode::Char(c) => Key::Char(c),
                        _ => Key::Other,
                    };
                    app.on_key(mapped);
                }
            }
        } else {
            app.tick(); // idle: advance the spinner
        }

        // A submitted LLM command: snapshot the transcript and run the turn
        // off-thread; the result returns over the channel.
        if let Some(user_msg) = app.pending_turn.take() {
            if let Some(model) = app.chat_model.clone() {
                // The real conversation (user + model replies only) minus the
                // just-submitted message, which is passed as `user_msg`. UI chrome
                // (greeting, setup notices, action narration) never reaches here.
                let history: Vec<Turn> = app
                    .convo
                    .iter()
                    .take(app.convo.len().saturating_sub(1))
                    .cloned()
                    .collect();
                let tx = tx.clone();
                rt.spawn(async move {
                    // Forward each streamed event to the UI thread as it arrives.
                    agent::run_turn_streaming(&model, &history, &user_msg, |ev| {
                        let _ = tx.send(ev);
                    })
                    .await;
                });
            } else {
                app.thinking = false;
            }
        }

        // Drain streamed events onto the UI thread (text grows the live reply;
        // Done/Failed finish the turn).
        while let Ok(event) = rx.try_recv() {
            app.on_agent_event(event);
        }
    }
    Ok(())
}
