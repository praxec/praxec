//! Cockpit application state and the pure key reducer (input → state change).
//! Kept free of terminal I/O so it is unit-testable; the event loop lives in
//! `main.rs`.
//!
//! Two focus zones: the **nav** strip (facets) and the **tree** body. `↑` at
//! the top of the tree escalates focus to the nav; `↓` in the nav descends back
//! into the tree. Within a zone the arrows mean different things (facet
//! left/right in nav; select up/down + expand/collapse in the tree).

use crate::map::fleet::Fleet;
use crate::map::{Level, MapState};
use crate::nav;
use crate::op::CockpitOp;
use crate::ui::fleet_view;
use crate::view::MissionView;
use ratatui::layout::Rect;

/// A fixed body rect so the Fleet tile geometry (and thus zoom apertures) is
/// deterministic for headless / snapshot rendering, independent of terminal
/// size at the moment of the keypress.
const MAP_BODY: Rect = Rect {
    x: 0,
    y: 4,
    width: 120,
    height: 23,
};

/// Which surface the cockpit is showing.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Mode {
    Build,
    Run,
}

/// Which zone has keyboard focus.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Focus {
    Nav,
    /// The compact list of asks (the HITL master list).
    Needs,
    /// A drilled-in ask's detail (question + choices + reply).
    AskDetail,
    Tree,
}

/// Which screen of the first-run embedding gate is showing.
/// Providers (enter the keys you have) → Recommend (the single best model for
/// your providers) → Browse (the escape hatch). `ProviderKey` is the key-entry
/// sub-screen reached from Providers.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EmbedPhase {
    Providers,
    ProviderKey,
    Recommend,
    Browse,
}

/// Which screen of the recommendation-first **chat gate** is showing — the same
/// shape as the embedding gate: Providers (enter your keys) → Recommend (the
/// single best conductor for your stance, with reasoning-effort + requests/day
/// knobs) → Browse (the escape hatch). `ProviderKey` is the key-entry sub-screen.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ChatPhase {
    Providers,
    ProviderKey,
    Recommend,
    Browse,
}

/// Top-level cockpit state.
pub struct App {
    pub mode: Mode,
    pub focus: Focus,
    pub should_quit: bool,
    /// Active nav facet (index into `nav::RUN` / `nav::BUILD`).
    pub nav_index: usize,
    /// Selected tree node (visible-node index).
    pub selected: usize,
    /// Selected ask in the Needs list (the master).
    pub needs_selected: usize,
    /// When an ask is drilled into: which ask (index into the asks list).
    pub drilled_ask: Option<usize>,
    /// Selected choice within the drilled-in ask's detail.
    pub ask_choice: usize,
    /// The in-progress free-text reply typed in the ask detail.
    pub reply: String,
    /// Spinner frame counter, advanced on the draw loop.
    pub tick: u64,
    /// The active single-mission view (the L1 surface the Mission renderer/keys
    /// read). `None` at the Fleet level until a mission is zoomed into.
    pub mission: Option<MissionView>,
    /// The **real** mission, as the gateway returns it (state + legal actions +
    /// guidance). When `Some`, the L1 view renders this HATEOAS surface instead
    /// of the demo task-tree; `action_cursor` selects among its legal actions.
    pub gateway: Option<crate::model::GatewayResponse>,
    pub action_cursor: usize,
    /// The live gateway *connection* (for read + submit). `None` in demo/snapshot
    /// (the response above is injected directly); the live binary sets it so ⏎
    /// can submit a transition (`praxec.command`).
    pub conn: Option<Box<dyn crate::gateway::Gateway>>,
    /// Build mode: the layered library — every discoverable definition the
    /// gateway serves. Fetched lazily the first time Build mode is entered with
    /// a live connection; pre-seeded in demo/snapshot.
    pub library: Vec<crate::model::LibraryEntry>,
    /// Selected row in the library browse list.
    pub library_cursor: usize,
    /// The definition opened with ⏎ on a library row — its body + content hash,
    /// read from the gateway. `Some` shows the detail/edit panel.
    pub library_detail: Option<crate::model::DefinitionDetail>,
    /// ADR-0008 (d) — the workflow ids the cockpit has launched this session. §32
    /// has no list-instances verb, so the cockpit tracks its own fleet; each id is
    /// polled (`conn.get`) to build the live L2 map.
    pub roster: Vec<String>,
    /// ADR-0009 — the mediator's cross-mission Needs-You inbox (every `waiting`
    /// mission with a human move), recomputed on each fleet refresh.
    pub inbox: Vec<crate::mediator::InboxItem>,
    /// The fleet of missions (the L0 terrain).
    pub fleet: Fleet,
    /// Map view-state: altitude + cursor + any in-flight zoom transition.
    pub map: MapState,
    /// The chat spine: the conversation transcript + the in-progress input.
    /// Typing anywhere (outside a HITL reply) builds `chat_input`; Enter submits
    /// it as a command (ADR-0005). The deterministic driver is replaced by the
    /// LLM in Increment 3.
    pub chat_log: Vec<ChatLine>,
    /// The **real conversation** replayed to the model — only genuine user
    /// messages + the model's own text replies. Kept separate from `chat_log`
    /// (the display transcript), so UI chrome (greeting, setup notices, action
    /// narration, errors) never pollutes the LLM context.
    pub convo: Vec<crate::agent::Turn>,
    pub chat_input: String,
    /// The chat LLM the cockpit runs on. `None` until the first-run chat gate
    /// assigns it (the chat is unusable until then) — its `None`-ness *is* the
    /// gate trigger.
    pub chat_model: Option<crate::llm::ChatModel>,
    /// Which screen of the chat gate is showing.
    pub chat_phase: ChatPhase,
    /// Cursor into the current chat-gate screen's list (providers / browse).
    pub chat_cursor: usize,
    /// The API key being typed on the chat gate's ProviderKey screen.
    pub chat_key_input: String,
    /// The chat-model catalog the gate offers — loaded from data.
    pub chat_options: Vec<crate::chat_catalog::ChatModelOption>,
    /// The reasoning effort selected on the Recommend screen (kept valid for the
    /// shown model; the gate clamps it to that model's supported levels).
    pub chat_reasoning: String,
    /// Index into [`crate::chat_catalog::requests_per_day_levels`] — the volume
    /// the cost magnitude is shown at (←→ on the Recommend screen).
    pub chat_requests_idx: usize,
    /// When true, a submitted command is driven by the LLM (`pending_turn`)
    /// rather than the deterministic `parse_command`. The live binary sets this;
    /// `App::new` leaves it off so demo/snapshot/tests stay deterministic.
    pub llm_enabled: bool,
    /// True while a turn is in flight (awaiting the model) — drives the
    /// "thinking" indicator. Set on submit, cleared when the turn finishes.
    pub thinking: bool,
    /// The in-progress assistant reply, accumulated token-by-token as the model
    /// streams. `Some` while a reply is arriving; committed to `chat_log` and
    /// cleared when the turn finishes.
    pub streaming: Option<String>,
    /// A command the LLM should run, recorded by `submit_command` for the event
    /// loop to pick up and dispatch off-thread. `App` itself never does IO.
    pub pending_turn: Option<String>,
    /// The active embedding model (powers semantic discovery). `None` = lexical
    /// search only — semantic is an **opt-in add-on**, not required.
    pub embedding: Option<praxec_embeddings::EmbeddingChoice>,
    /// Whether to show the first-run **embedding gate** (only when the user
    /// hasn't yet decided). The gate is skippable — declining keeps lexical
    /// search. `App::new` leaves it off so tests/snapshots skip it.
    pub embedding_gate: bool,
    /// Which screen of the embedding gate is showing.
    pub embed_phase: EmbedPhase,
    /// Cursor into the current screen's list (providers / browse).
    pub embed_cursor: usize,
    /// The API key being typed on the ProviderKey screen.
    pub embed_key_input: String,
    /// The embedding-model catalog the gate offers — loaded from data (not a
    /// hard-coded list), so new models ship without a cockpit release.
    pub embed_options: Vec<praxec_embeddings::EmbeddingOption>,
    /// The recommendation **stance** — the lens every model recommendation is
    /// made through (`crate::priorities`). Always present (default Balanced);
    /// editable from Settings.
    pub priorities: crate::priorities::Priorities,
    /// Whether to show the first-run **priorities panel** (only when the user
    /// hasn't decided). It's the very first gate — the lens for everything after.
    pub priorities_gate: bool,
    /// Cursor within the priorities panel (shared by first-run and Settings):
    /// rows 0..=3 = stances, 4 = budget ceiling, 5 = local-only, 6 = Continue.
    pub prio_cursor: usize,
    /// Whether the **Settings** overlay is open (reached with `g` from the
    /// cockpit). The persistent home for changing the stance + the model picks.
    pub settings_open: bool,
    /// Cursor in the Settings menu: 0 = Priorities, 1 = Chat model, 2 = Embedding.
    pub settings_cursor: usize,
    /// When the Settings overlay has drilled into the priorities sub-panel.
    pub settings_editing_priorities: bool,
    /// ADR-0009 / H9 — whether the **mediator inbox** overlay is open (reached
    /// with `a` from the cockpit when the "✋ N need you" chip is showing). The
    /// surface that lets the human actually *answer* a parked mission, not just
    /// see the count.
    pub inbox_open: bool,
    /// Which inbox item is selected (index into [`App::inbox`]).
    pub inbox_cursor: usize,
    /// Which of the selected item's human-actor choices is highlighted.
    pub inbox_choice: usize,
}

/// One line in the chat transcript.
pub struct ChatLine {
    /// True if you said it, false if Mission Control did.
    pub you: bool,
    pub text: String,
}

impl App {
    pub fn new() -> Self {
        Self {
            mode: Mode::Run,
            focus: Focus::Tree,
            should_quit: false,
            nav_index: nav::TREE, // the Run home
            selected: 0,
            needs_selected: 0,
            drilled_ask: None,
            ask_choice: 0,
            reply: String::new(),
            tick: 0,
            mission: None,
            gateway: None,
            action_cursor: 0,
            conn: None,
            library: Vec::new(),
            library_cursor: 0,
            library_detail: None,
            roster: Vec::new(),
            inbox: Vec::new(),
            inbox_open: false,
            inbox_cursor: 0,
            inbox_choice: 0,
            fleet: Fleet::demo(),
            map: MapState::new(), // starts at the Fleet level
            chat_log: vec![ChatLine {
                you: false,
                text: "Mission Control. Type to drive the map — a mission name, “back”, or “what needs me”. Arrow keys work too.".into(),
            }],
            convo: Vec::new(),
            chat_input: String::new(),
            // A ready default for programmatic / demo use; the live binary starts
            // this `None` to show the first-run setup gate.
            chat_model: Some(crate::llm::ChatModel {
                vendor: "anthropic".into(),
                model: "claude-opus-4-8".into(),
                reasoning_effort: "medium".into(),
            }),
            chat_phase: ChatPhase::Providers,
            chat_cursor: 0,
            chat_key_input: String::new(),
            // The shipped default catalog (deterministic for demo/tests); the live
            // binary swaps in the override-aware catalog in `fresh_detecting`.
            chat_options: crate::chat_catalog::default_chat_options(),
            chat_reasoning: "medium".into(),
            chat_requests_idx: 1, // the ~1k/day preset
            llm_enabled: false,
            thinking: false,
            streaming: None,
            pending_turn: None,
            // A demo default so tests/snapshots skip the embedding gate; the live
            // binary loads the persisted setting (or shows the gate if undecided).
            embedding: Some(praxec_embeddings::EmbeddingChoice {
                vendor: "openai".into(),
                model: "text-embedding-3-small".into(),
                dims: 1536,
            }),
            embedding_gate: false,
            embed_phase: EmbedPhase::Providers,
            embed_cursor: 0,
            embed_key_input: String::new(),
            // The shipped default catalog (deterministic for demo/tests); the live
            // binary swaps in the override-aware catalog in `fresh_detecting`.
            embed_options: praxec_embeddings::default_embedding_options(),
            // A decided default so demo/tests skip the first-run panel; the live
            // binary loads the persisted stance (or shows the panel) in
            // `fresh_detecting`.
            priorities: crate::priorities::Priorities::default(),
            priorities_gate: false,
            prio_cursor: 0,
            settings_open: false,
            settings_cursor: 0,
            settings_editing_priorities: false,
        }
    }

    /// Start unconfigured — the first-run setup gate at the vendor step. Pure
    /// (no config probing), so it is deterministic for tests and snapshots.
    pub fn fresh() -> Self {
        Self {
            chat_model: None,
            ..Self::new()
        }
    }

    /// The live first-run entry: start unconfigured, then apply detection of any
    /// existing praxec config — pre-filling or skipping the gate (ADR-0005 §5).
    pub fn fresh_detecting() -> Self {
        let mut app = Self::fresh();
        // The stance is the lens for every model recommendation, so it's the very
        // first gate — load the persisted one, or show the panel if undecided.
        match crate::priorities::Priorities::load() {
            Some(p) => app.priorities = p,
            None => app.priorities_gate = true,
        }
        // Load the catalogs (override-aware) for the live gates.
        app.embed_options = praxec_embeddings::embedding_options();
        app.chat_options = crate::chat_catalog::chat_options();
        // Semantic discovery is opt-in: show the gate only if the user hasn't
        // decided. A registered model is used; an explicit decline (or any prior
        // decision) keeps lexical search without nagging.
        match praxec_embeddings::load_setting() {
            None => {
                app.embedding = None;
                app.embedding_gate = true;
            }
            Some(praxec_embeddings::EmbeddingSetting::Lexical) => {
                app.embedding = None;
                app.embedding_gate = false;
            }
            Some(praxec_embeddings::EmbeddingSetting::Model(c)) => {
                app.embedding = Some(c);
                app.embedding_gate = false;
            }
        }
        app.apply_detection(crate::llm::detect());
        app
    }

    /// Apply a detection outcome to the fresh state: a complete config skips the
    /// gate; nothing usable leaves the chat gate to run. Split out so it is
    /// testable without touching real config files.
    fn apply_detection(&mut self, detected: crate::llm::Detected) {
        use crate::llm::Detected;
        match detected {
            Detected::Complete(model) => {
                self.chat_log.push(ChatLine {
                    you: false,
                    text: format!(
                        "Using {} · {}. What do you want to do?",
                        model.vendor, model.model
                    ),
                });
                self.chat_model = Some(model);
            }
            Detected::None => {}
        }
    }

    /// Enter directly at the Mission level over a single given view (the
    /// existing single-mission entry point). The reducer routes to the existing
    /// Mission handling; the active view is `self.mission` directly.
    pub fn with_mission(mut self, mission: MissionView) -> Self {
        self.mission = Some(mission);
        self.map.level = Level::Mission;
        self
    }

    /// Advance the spinner (called on each idle draw tick) and any in-flight
    /// zoom transition by one ~60fps frame's worth of progress.
    pub fn tick(&mut self) {
        self.tick = self.tick.wrapping_add(1);
        // One ~60fps frame of progress. `tick_transition` divides by
        // TRANSITION_SECS internally, so pass the frame dt in seconds.
        self.map.tick_transition(1.0 / 60.0);
    }

    fn selectable_count(&self) -> usize {
        self.mission
            .as_ref()
            .map(|m| m.selectable_count())
            .unwrap_or(0)
    }

    fn facet_max(&self) -> usize {
        nav::count(self.mode == Mode::Build).saturating_sub(1)
    }

    /// Populate the Build-mode library from the live gateway the first time it's
    /// needed. No-op if already loaded or there's no connection (demo/snapshot
    /// pre-seed `library` directly). A fetch failure leaves the list empty — the
    /// Build view renders an explicit "couldn't reach the library" state rather
    /// than inventing entries.
    fn load_library(&mut self) {
        if !self.library.is_empty() {
            return;
        }
        if let Some(conn) = self.conn.as_ref() {
            match conn.library() {
                Ok(entries) => self.library = entries,
                Err(e) => self.chat_log.push(ChatLine {
                    you: false,
                    text: format!("Couldn't load the library: {e}"),
                }),
            }
            self.library_cursor = 0;
        }
    }

    /// Count of library entries shown under the active Build facet — bounds the
    /// browse cursor.
    fn library_visible_len(&self) -> usize {
        let facet = nav::BUILD[self.nav_index.min(nav::BUILD.len() - 1)];
        crate::ui::library::visible_len(self, facet)
    }

    /// ⏎ on a library row: read the selected definition's current body from the
    /// gateway (the edit basis) and focus the chat spine on editing it. Edits
    /// themselves are chat-conducted — the author types the change, the model
    /// drives the edit workflow (read body → diff → hash-guarded publish).
    fn act_on_selected_definition(&mut self) {
        let facet = nav::BUILD[self.nav_index.min(nav::BUILD.len() - 1)];
        let Some(id) =
            crate::ui::library::nth_visible(self, facet, self.library_cursor).map(|e| e.id.clone())
        else {
            return;
        };
        // Read the current body when connected; in demo/offline mode just focus
        // the chat on the selection.
        if let Some(conn) = self.conn.as_ref() {
            match conn.read_definition(&id) {
                Ok(detail) => self.library_detail = Some(detail),
                Err(e) => self.chat_log.push(ChatLine {
                    you: false,
                    text: format!("Couldn't read {id}: {e}"),
                }),
            }
        }
        self.chat_input = format!("Edit {id}: ");
    }

    /// ⏎ on a Build-mode row: launch a workflow/agent (start a mission, zoom in);
    /// a non-launchable definition (skill/script/capability) has nothing to run,
    /// so ⏎ opens it to edit instead. Ctrl-E always edits (see [`Self::act_on_selected_definition`]).
    fn launch_or_edit_selected(&mut self) {
        let facet = nav::BUILD[self.nav_index.min(nav::BUILD.len() - 1)];
        let Some(entry) =
            crate::ui::library::nth_visible(self, facet, self.library_cursor).cloned()
        else {
            return;
        };
        if matches!(entry.kind.as_str(), "workflow" | "agent") {
            self.launch_definition(&entry.id);
        } else {
            self.act_on_selected_definition();
        }
    }

    /// Launch a workflow/agent: start an instance via the gateway, track its id in
    /// the roster (the self-tracked fleet — §32 has no list-instances verb), and
    /// zoom straight into the live mission. Narrated either way.
    fn launch_definition(&mut self, id: &str) {
        let Some(conn) = self.conn.as_ref() else {
            self.chat_log.push(ChatLine {
                you: false,
                text: format!("Can't launch {id}: not connected to a gateway."),
            });
            return;
        };
        match conn.launch(id, serde_json::json!({})) {
            Ok(resp) => {
                let wf_id = resp.workflow.id.clone();
                if !self.roster.contains(&wf_id) {
                    self.roster.push(wf_id.clone());
                }
                self.chat_log.push(ChatLine {
                    you: false,
                    text: format!("Launched {id} → mission {wf_id}."),
                });
                self.gateway = Some(resp);
                self.mode = Mode::Run;
                self.map.level = crate::map::Level::Mission;
                self.action_cursor = 0;
                self.refresh_fleet();
            }
            Err(e) => self.chat_log.push(ChatLine {
                you: false,
                text: format!("Couldn't launch {id}: {e}"),
            }),
        }
    }

    /// Rebuild the live fleet (L0 map) from the launch roster — poll each tracked
    /// instance's current state (ADR-0008 d1). No-op without a connection or an
    /// empty roster (the demo fleet stays). Best-effort: an instance that can't be
    /// fetched is omitted rather than failing the whole refresh.
    fn refresh_fleet(&mut self) {
        if self.roster.is_empty() {
            return;
        }
        let ids = self.roster.clone();
        let responses: Vec<crate::model::GatewayResponse> = match self.conn.as_ref() {
            Some(conn) => ids.iter().filter_map(|id| conn.get(id).ok()).collect(),
            None => return,
        };
        if !responses.is_empty() {
            self.fleet = Fleet::from_roster(&responses);
            let max = self.fleet.missions.len().saturating_sub(1);
            self.map.fleet_cursor = self.map.fleet_cursor.min(max);
        }
        // ADR-0009 — the mediator's cross-mission inbox: every `waiting` mission
        // with a human-actor move, themed into one place (no context-switching).
        self.inbox = crate::mediator::inbox(&responses);
    }

    /// ADR-0009 — answer a Needs-You inbox item: submit the chosen human
    /// transition to that mission via the gateway (the §32 answer), then refresh
    /// the inbox. Narrated; a no-op without a connection or a valid index.
    pub fn answer_inbox(&mut self, index: usize, transition: &str) {
        let Some(item) = self.inbox.get(index).cloned() else {
            return;
        };
        let result = match self.conn.as_ref() {
            Some(conn) => conn.command(&item.mission_id, item.version, transition),
            None => {
                self.chat_log.push(ChatLine {
                    you: false,
                    text: format!(
                        "Can't answer {}: not connected to a gateway.",
                        item.mission_id
                    ),
                });
                return;
            }
        };
        match result {
            Ok(_) => {
                self.chat_log.push(ChatLine {
                    you: false,
                    text: format!("Answered {} → {transition}.", item.mission_id),
                });
                self.refresh_fleet();
            }
            Err(e) => self.chat_log.push(ChatLine {
                you: false,
                text: format!("Couldn't answer {}: {e}", item.mission_id),
            }),
        }
    }

    /// Number of asks (the master list length).
    fn asks_len(&self) -> usize {
        self.mission
            .as_ref()
            .map(|m| m.needs_you_with_context().len())
            .unwrap_or(0)
    }

    /// Number of choices in the currently drilled-in ask.
    fn ask_choices_len(&self) -> usize {
        let Some(i) = self.drilled_ask else {
            return 0;
        };
        self.mission
            .as_ref()
            .and_then(|m| m.needs_you_with_context().into_iter().nth(i))
            .map(|(_, n)| n.actions.len())
            .unwrap_or(0)
    }

    /// `Enter` drives the drill-in: Tree/Nav → open the ask list; the list →
    /// drill into the selected ask; the detail → take the selected choice.
    fn on_enter(&mut self) {
        match self.focus {
            Focus::Needs => {
                if self.asks_len() > 0 {
                    self.drilled_ask = Some(self.needs_selected);
                    self.ask_choice = 0;
                    self.focus = Focus::AskDetail;
                }
            }
            Focus::AskDetail => self.send_reply(),
            _ => {
                if self.asks_len() > 0 {
                    self.focus = Focus::Needs;
                    self.needs_selected = 0;
                }
            }
        }
    }

    /// Submit the answer to the drilled-in ask: a typed reply if present
    /// (recorded as your turn), else the selected quick-reply choice. Either
    /// resolves the ask — a single-turn answer is the live capability; the
    /// multi-turn agent response is deferred (SPEC §29.7).
    fn send_reply(&mut self) {
        let Some(i) = self.drilled_ask else {
            return;
        };
        let typed = self.reply.trim().to_string();
        if let Some(m) = self.mission.as_mut() {
            if !typed.is_empty() {
                m.push_ask_turn(i, crate::view::Speaker::You, typed);
            }
            m.resolve_ask(i);
        }
        self.reply.clear();
        self.drilled_ask = None;
        let len = self.asks_len();
        if len == 0 {
            self.focus = Focus::Tree; // all asks handled
        } else {
            self.needs_selected = self.needs_selected.min(len - 1);
            self.focus = Focus::Needs;
        }
    }

    fn on_key_needs(&mut self, key: Key) {
        match key {
            Key::Up => self.needs_selected = self.needs_selected.saturating_sub(1),
            Key::Down => {
                let max = self.asks_len().saturating_sub(1);
                self.needs_selected = (self.needs_selected + 1).min(max);
            }
            // The list is left of the tree → Right leaves the list for the tree.
            Key::Right => self.focus = Focus::Tree,
            _ => {}
        }
    }

    /// The ask detail is a live text field: printable keys edit the reply,
    /// ↑↓ pick a quick-reply, ⏎ sends, ⎋/← backs out.
    fn on_key_ask_detail(&mut self, key: Key) {
        match key {
            Key::Char(c) => self.reply.push(c),
            Key::Backspace => {
                self.reply.pop();
            }
            Key::Enter => self.send_reply(),
            Key::Up => self.ask_choice = self.ask_choice.saturating_sub(1),
            Key::Down => {
                let max = self.ask_choices_len().saturating_sub(1);
                self.ask_choice = (self.ask_choice + 1).min(max);
            }
            Key::Escape | Key::Left => {
                self.drilled_ask = None;
                self.reply.clear();
                self.focus = Focus::Needs;
            }
            _ => {}
        }
    }

    /// Pure reducer, dispatched by map altitude. Input is ignored mid-zoom;
    /// otherwise the Fleet level pans/zooms the terrain, and the Mission level
    /// runs the single-mission reducer (with `⎋` zooming back out to the fleet).
    pub fn on_key(&mut self, key: Key) {
        if key == Key::Quit {
            self.should_quit = true;
            return;
        }
        // The priorities panel is the very first gate — the lens for every model
        // recommendation after it.
        if self.priorities_gate {
            self.on_key_priorities(key, true);
            return;
        }
        // The Settings overlay (opened with `g`) owns the keyboard while open.
        if self.settings_open {
            self.on_key_settings(key);
            return;
        }
        // H9 — the mediator inbox overlay (opened with `a`) owns the keyboard
        // while open: it's how the human answers a parked mission.
        if self.inbox_open {
            self.on_key_inbox(key);
            return;
        }
        // The embedding gate comes next (bootstrap order) — but it's skippable:
        // semantic discovery is an opt-in add-on.
        if self.embedding_gate {
            self.on_key_embedding(key);
            return;
        }
        // The chat gate owns the keyboard until a chat model is assigned.
        if self.chat_model.is_none() {
            self.on_key_chat(key);
            return;
        }
        if self.map.is_transitioning() {
            return; // a zoom is animating — swallow input
        }
        // A HITL reply owns the keyboard while you're drilled into an ask.
        if self.map.level == Level::Mission && self.focus == Focus::AskDetail {
            self.on_key_ask_detail(key);
            return;
        }
        // Build mode: the stage is the library browser. Arrows drive it (facets
        // ←→, browse ↑↓); everything else (⇥ to toggle mode, typing into the
        // chat spine) falls through to the shared handling below — authoring is
        // chat-conducted, so the spine stays live.
        if self.mode == Mode::Build {
            match key {
                Key::Left => {
                    self.nav_index = self.nav_index.saturating_sub(1);
                    self.library_cursor = 0;
                    self.library_detail = None;
                    return;
                }
                Key::Right => {
                    self.nav_index = (self.nav_index + 1).min(self.facet_max());
                    self.library_cursor = 0;
                    self.library_detail = None;
                    return;
                }
                Key::Up => {
                    self.library_cursor = self.library_cursor.saturating_sub(1);
                    self.library_detail = None;
                    return;
                }
                Key::Down => {
                    let max = self.library_visible_len().saturating_sub(1);
                    self.library_cursor = (self.library_cursor + 1).min(max);
                    self.library_detail = None;
                    return;
                }
                Key::Enter => {
                    self.launch_or_edit_selected();
                    return;
                }
                Key::Edit => {
                    self.act_on_selected_definition();
                    return;
                }
                _ => {}
            }
        }
        // The chat input is the always-on text target (ADR-0005): typing builds
        // a command, Enter submits it. Arrows / empty-Enter / Esc fall through to
        // map navigation, so keystrokes stay co-equal.
        match key {
            // `g` on an empty input opens Settings — a hotkey-when-not-typing,
            // the same convention by which arrows/empty-Enter drive the map.
            Key::Char('g') if self.chat_input.is_empty() => {
                self.open_settings();
                return;
            }
            // H9 — `a` on an empty input opens the mediator inbox to *answer* a
            // parked mission (the "✋ N need you" chip is otherwise unactionable).
            // No-op when nothing is waiting, so it can't open an empty overlay.
            Key::Char('a') if self.chat_input.is_empty() && !self.inbox.is_empty() => {
                self.open_inbox();
                return;
            }
            Key::Char(c) => {
                self.chat_input.push(c);
                return;
            }
            Key::Backspace => {
                self.chat_input.pop();
                return;
            }
            Key::Enter if !self.chat_input.trim().is_empty() => {
                self.submit_command();
                return;
            }
            _ => {}
        }
        match self.map.level {
            Level::Fleet => self.on_key_fleet(key),
            Level::Mission => self.on_key_mission(key),
        }
    }

    /// The first-run embedding gate — a three-screen flow: **Providers** (enter
    /// the keys you have) → **Recommend** (the single best model across your
    /// providers, with the cost magnitude) → **Browse** (the escape hatch). `⎋`
    /// skips to lexical-only from Providers or Recommend.
    fn on_key_embedding(&mut self, key: Key) {
        match self.embed_phase {
            EmbedPhase::Providers => {
                let providers = self.embed_providers();
                let rows = providers.len() + 1; // + the "Continue" row
                match key {
                    Key::Escape => self.embedding_skip(),
                    Key::Up => self.embed_cursor = self.embed_cursor.saturating_sub(1),
                    Key::Down => self.embed_cursor = (self.embed_cursor + 1).min(rows - 1),
                    Key::Enter => {
                        if self.embed_cursor >= providers.len() {
                            self.embed_phase = EmbedPhase::Recommend; // "Continue"
                        } else if !crate::llm::has_key(&providers[self.embed_cursor]) {
                            // Needs a key → key entry. (Keyless/local is already ready.)
                            self.embed_key_input.clear();
                            self.embed_phase = EmbedPhase::ProviderKey;
                        }
                    }
                    _ => {}
                }
            }
            EmbedPhase::ProviderKey => {
                let providers = self.embed_providers();
                let vendor = providers
                    .get(self.embed_cursor)
                    .cloned()
                    .unwrap_or_default();
                match key {
                    Key::Char(c) => self.embed_key_input.push(c),
                    Key::Backspace => {
                        self.embed_key_input.pop();
                    }
                    Key::Escape => {
                        self.embed_key_input.clear();
                        self.embed_phase = EmbedPhase::Providers;
                    }
                    Key::Enter if !self.embed_key_input.trim().is_empty() => {
                        crate::llm::store_key(&vendor, self.embed_key_input.trim());
                        self.embed_key_input.clear();
                        self.embed_phase = EmbedPhase::Providers;
                    }
                    _ => {}
                }
            }
            EmbedPhase::Recommend => match key {
                Key::Escape => self.embedding_skip(), // skip (e.g. cost too high)
                Key::Left => self.embed_phase = EmbedPhase::Providers,
                Key::Right => {
                    self.embed_cursor = 0;
                    self.embed_phase = EmbedPhase::Browse;
                }
                Key::Enter => {
                    let candidates = self.embed_candidates();
                    if let Some(opt) = praxec_embeddings::recommend(&candidates).cloned() {
                        self.register_embedding(&opt);
                    }
                }
                _ => {}
            },
            EmbedPhase::Browse => {
                let reachable = self.embed_reachable();
                match key {
                    Key::Escape | Key::Left => self.embed_phase = EmbedPhase::Recommend,
                    Key::Up => self.embed_cursor = self.embed_cursor.saturating_sub(1),
                    Key::Down => {
                        self.embed_cursor =
                            (self.embed_cursor + 1).min(reachable.len().saturating_sub(1))
                    }
                    Key::Enter => {
                        if let Some(opt) =
                            reachable.get(self.embed_cursor.min(reachable.len().saturating_sub(1)))
                        {
                            let opt = opt.clone();
                            self.register_embedding(&opt);
                        }
                    }
                    _ => {}
                }
            }
        }
    }

    /// Decline the add-on — lexical search only (the decision is remembered).
    fn embedding_skip(&mut self) {
        let _ = praxec_embeddings::save_setting(&praxec_embeddings::EmbeddingSetting::Lexical);
        self.chat_log.push(ChatLine {
            you: false,
            text: "Lexical search only — no embedding model.".into(),
        });
        self.embedding = None;
        self.embedding_gate = false;
    }

    /// Distinct embedding providers from the catalog, in catalog order.
    pub fn embed_providers(&self) -> Vec<String> {
        let mut out: Vec<String> = Vec::new();
        for o in &self.embed_options {
            if !out.iter().any(|v| v == &o.vendor) {
                out.push(o.vendor.clone());
            }
        }
        out
    }

    /// Embedding options eligible under the active stance — a `local_only`
    /// privacy stance restricts to local models (the budget ceiling doesn't bind
    /// for embeddings, which are pennies). The recommend/Browse views draw from
    /// this so the stance's hard constraints apply to embeddings too.
    pub fn embed_candidates(&self) -> Vec<praxec_embeddings::EmbeddingOption> {
        self.embed_options
            .iter()
            .filter(|o| !self.priorities.local_only || o.local)
            .cloned()
            .collect()
    }

    /// Reachable options (a key is set, or local), best MTEB-Retrieval first —
    /// within the stance's constraints.
    pub fn embed_reachable(&self) -> Vec<praxec_embeddings::EmbeddingOption> {
        let mut v: Vec<_> = self
            .embed_candidates()
            .into_iter()
            .filter(|o| praxec_embeddings::vendor_available(&o.vendor))
            .collect();
        v.sort_by(|a, b| {
            b.mteb_score
                .partial_cmp(&a.mteb_score)
                .unwrap_or(std::cmp::Ordering::Equal)
        });
        v
    }

    /// Persist an embedding-model choice and dismiss the gate.
    fn register_embedding(&mut self, opt: &praxec_embeddings::EmbeddingOption) {
        let choice = praxec_embeddings::EmbeddingChoice::from(opt);
        let _ = praxec_embeddings::save_choice(&choice);
        self.chat_log.push(ChatLine {
            you: false,
            text: format!("Embeddings: {} · {}.", choice.vendor, choice.model),
        });
        self.embedding = Some(choice);
        self.embedding_gate = false;
    }

    // ── priorities panel + Settings overlay ──────────────────────────────────

    /// Number of focusable rows in the priorities panel (4 stances + budget +
    /// local + Continue).
    const PRIO_ROWS: usize = crate::priorities::Stance::ALL.len() + 3;
    const PRIO_BUDGET_ROW: usize = crate::priorities::Stance::ALL.len();
    const PRIO_LOCAL_ROW: usize = crate::priorities::Stance::ALL.len() + 1;

    /// The priorities panel reducer — pick a stance (rows 0..=3), cycle the
    /// budget ceiling (←→), toggle local-only, then Continue. `first_run` routes
    /// the exit: the first-run gate proceeds to the next gate; from Settings it
    /// returns to the menu. Either way the choice is persisted on exit.
    fn on_key_priorities(&mut self, key: Key, first_run: bool) {
        use crate::priorities::{Stance, BUDGET_CAPS};
        let stance_rows = Stance::ALL.len();
        match key {
            Key::Up => self.prio_cursor = self.prio_cursor.saturating_sub(1),
            Key::Down => self.prio_cursor = (self.prio_cursor + 1).min(Self::PRIO_ROWS - 1),
            // Selecting a stance row (or Space/Right on it) makes it the stance.
            Key::Enter | Key::Char(' ') | Key::Right if self.prio_cursor < stance_rows => {
                self.priorities.stance = Stance::ALL[self.prio_cursor];
                // Enter on a stance confirms-and-selects; Space/Right just select.
                if key == Key::Enter {
                    self.finish_priorities(first_run);
                }
            }
            // Budget ceiling: cycle the magnitude ladder.
            Key::Left | Key::Right if self.prio_cursor == Self::PRIO_BUDGET_ROW => {
                let cur = BUDGET_CAPS
                    .iter()
                    .position(|c| *c == self.priorities.budget_cap)
                    .unwrap_or(0);
                let next = if key == Key::Right {
                    (cur + 1).min(BUDGET_CAPS.len() - 1)
                } else {
                    cur.saturating_sub(1)
                };
                self.priorities.budget_cap = BUDGET_CAPS[next];
            }
            // Local-only: toggle.
            Key::Left | Key::Right | Key::Char(' ') if self.prio_cursor == Self::PRIO_LOCAL_ROW => {
                self.priorities.local_only = !self.priorities.local_only;
            }
            // Continue (or Enter on any non-stance row) confirms.
            Key::Enter => self.finish_priorities(first_run),
            // Escape accepts the current selection (default Balanced at first run).
            Key::Escape => self.finish_priorities(first_run),
            _ => {}
        }
    }

    /// Persist the stance and leave the panel: at first run, fall through to the
    /// next gate; from Settings, return to the menu.
    fn finish_priorities(&mut self, first_run: bool) {
        let _ = self.priorities.save();
        if first_run {
            self.priorities_gate = false;
        } else {
            self.settings_editing_priorities = false;
        }
    }

    /// Open the Settings overlay (the persistent home for the stance + the model
    /// picks). Reached with `g` from the cockpit.
    fn open_settings(&mut self) {
        self.settings_open = true;
        self.settings_cursor = 0;
        self.settings_editing_priorities = false;
    }

    /// H9 — open the mediator inbox overlay to answer a parked mission. Reached
    /// with `a` from the cockpit when the "✋ N need you" chip is showing.
    fn open_inbox(&mut self) {
        self.inbox_open = true;
        self.inbox_cursor = 0;
        self.inbox_choice = 0;
    }

    /// Number of human-actor choices on the currently-selected inbox item.
    fn inbox_choices_len(&self) -> usize {
        self.inbox
            .get(self.inbox_cursor)
            .map(|it| it.choices.len())
            .unwrap_or(0)
    }

    /// The mediator inbox reducer: a two-axis pick — `↑↓` choose the parked
    /// mission, `←→` choose which human transition to submit, `⏎` answers it
    /// via [`App::answer_inbox`], `⎋` closes. The overlay closes once the inbox
    /// empties (answering removes the parked mission on the next refresh).
    fn on_key_inbox(&mut self, key: Key) {
        match key {
            Key::Up => {
                self.inbox_cursor = self.inbox_cursor.saturating_sub(1);
                self.inbox_choice = 0;
            }
            Key::Down => {
                let max = self.inbox.len().saturating_sub(1);
                self.inbox_cursor = (self.inbox_cursor + 1).min(max);
                self.inbox_choice = 0;
            }
            Key::Left => self.inbox_choice = self.inbox_choice.saturating_sub(1),
            Key::Right => {
                let max = self.inbox_choices_len().saturating_sub(1);
                self.inbox_choice = (self.inbox_choice + 1).min(max);
            }
            Key::Escape => self.inbox_open = false,
            Key::Enter => {
                // Resolve the chosen human transition, then dispatch the §32 answer.
                let choice = self
                    .inbox
                    .get(self.inbox_cursor)
                    .and_then(|it| it.choices.get(self.inbox_choice))
                    .cloned();
                if let Some(transition) = choice {
                    self.answer_inbox(self.inbox_cursor, &transition);
                }
                // answer_inbox refreshes the inbox; keep the cursor in range and
                // close the overlay once there's nothing left to answer.
                if self.inbox.is_empty() {
                    self.inbox_open = false;
                } else {
                    self.inbox_cursor = self.inbox_cursor.min(self.inbox.len() - 1);
                    self.inbox_choice = 0;
                }
            }
            _ => {}
        }
    }

    /// The Settings overlay reducer: a three-row menu (Priorities / Chat model /
    /// Embedding model). Priorities drills into the same panel the first-run gate
    /// uses; the model rows re-enter their existing gates.
    fn on_key_settings(&mut self, key: Key) {
        if self.settings_editing_priorities {
            self.on_key_priorities(key, false);
            return;
        }
        match key {
            Key::Up => self.settings_cursor = self.settings_cursor.saturating_sub(1),
            Key::Down => self.settings_cursor = (self.settings_cursor + 1).min(2),
            Key::Escape => self.settings_open = false,
            Key::Enter => match self.settings_cursor {
                0 => {
                    // Drill into the priorities panel, cursor on the current stance.
                    self.prio_cursor = crate::priorities::Stance::ALL
                        .iter()
                        .position(|s| *s == self.priorities.stance)
                        .unwrap_or(0);
                    self.settings_editing_priorities = true;
                }
                1 => {
                    // Re-pick the chat model: re-enter the chat gate.
                    self.settings_open = false;
                    self.chat_model = None;
                    self.chat_phase = ChatPhase::Providers;
                    self.chat_cursor = 0;
                }
                _ => {
                    // Re-pick the embedding model: re-enter the embedding gate.
                    self.settings_open = false;
                    self.embedding_gate = true;
                    self.embed_phase = EmbedPhase::Providers;
                    self.embed_cursor = 0;
                }
            },
            _ => {}
        }
    }

    // ── the chat gate (recommendation-first) ─────────────────────────────────

    /// The first-run **chat gate**: Providers (enter your keys) → Recommend (the
    /// best conductor for your stance, with reasoning-effort ↑↓ + requests/day
    /// ←→) → Browse (the escape hatch). Confirming assigns `chat_model`, which
    /// dismisses the gate. Unlike the embedding add-on, a chat model is required,
    /// so there is no skip.
    fn on_key_chat(&mut self, key: Key) {
        match self.chat_phase {
            ChatPhase::Providers => {
                let providers = self.chat_providers();
                let rows = providers.len() + 1; // + the "Continue" row
                match key {
                    Key::Up => self.chat_cursor = self.chat_cursor.saturating_sub(1),
                    Key::Down => self.chat_cursor = (self.chat_cursor + 1).min(rows - 1),
                    Key::Enter => {
                        if self.chat_cursor >= providers.len() {
                            self.enter_chat_recommend(); // "Continue"
                        } else if !crate::llm::has_key(&providers[self.chat_cursor]) {
                            self.chat_key_input.clear();
                            self.chat_phase = ChatPhase::ProviderKey;
                        }
                    }
                    _ => {}
                }
            }
            ChatPhase::ProviderKey => {
                let providers = self.chat_providers();
                let vendor = providers.get(self.chat_cursor).cloned().unwrap_or_default();
                match key {
                    Key::Char(c) => self.chat_key_input.push(c),
                    Key::Backspace => {
                        self.chat_key_input.pop();
                    }
                    Key::Escape => {
                        self.chat_key_input.clear();
                        self.chat_phase = ChatPhase::Providers;
                    }
                    Key::Enter if !self.chat_key_input.trim().is_empty() => {
                        crate::llm::store_key(&vendor, self.chat_key_input.trim());
                        self.chat_key_input.clear();
                        self.chat_phase = ChatPhase::Providers;
                    }
                    _ => {}
                }
            }
            ChatPhase::Recommend => match key {
                Key::Escape => self.chat_phase = ChatPhase::Providers,
                Key::Char('b') => {
                    self.chat_cursor = 0;
                    self.chat_phase = ChatPhase::Browse;
                }
                // ↑↓ cycles reasoning effort for the recommended model.
                Key::Up => self.step_chat_reasoning(-1),
                Key::Down => self.step_chat_reasoning(1),
                // ←→ cycles the requests/day the cost is shown at.
                Key::Left => self.chat_requests_idx = self.chat_requests_idx.saturating_sub(1),
                Key::Right => {
                    let max = crate::chat_catalog::requests_per_day_levels().len() - 1;
                    self.chat_requests_idx = (self.chat_requests_idx + 1).min(max);
                }
                Key::Enter => {
                    if let Some(opt) = self.chat_recommended().cloned() {
                        let reasoning = self.chat_reasoning.clone();
                        self.register_chat(&opt, &reasoning);
                    }
                }
                _ => {}
            },
            ChatPhase::Browse => {
                let reachable = self.chat_reachable();
                match key {
                    Key::Escape | Key::Left => self.chat_phase = ChatPhase::Recommend,
                    Key::Up => self.chat_cursor = self.chat_cursor.saturating_sub(1),
                    Key::Down => {
                        self.chat_cursor =
                            (self.chat_cursor + 1).min(reachable.len().saturating_sub(1))
                    }
                    Key::Enter => {
                        if let Some(opt) =
                            reachable.get(self.chat_cursor.min(reachable.len().saturating_sub(1)))
                        {
                            let opt = opt.clone();
                            // Browse picks use the model's own default effort.
                            let reasoning = crate::chat_catalog::default_reasoning(&opt);
                            self.register_chat(&opt, &reasoning);
                        }
                    }
                    _ => {}
                }
            }
        }
    }

    /// Move into the Recommend screen, seeding the reasoning effort from the
    /// recommended model's default.
    fn enter_chat_recommend(&mut self) {
        self.chat_phase = ChatPhase::Recommend;
        if let Some(opt) = self.chat_recommended() {
            self.chat_reasoning = crate::chat_catalog::default_reasoning(opt);
        }
    }

    /// Cycle the reasoning effort by `delta` within the recommended model's
    /// supported levels (keeping `chat_reasoning` valid for that model).
    fn step_chat_reasoning(&mut self, delta: isize) {
        let Some(opt) = self.chat_recommended() else {
            return;
        };
        let levels = &opt.reasoning_levels;
        if levels.is_empty() {
            return;
        }
        let cur = levels
            .iter()
            .position(|l| *l == self.chat_reasoning)
            .unwrap_or(0) as isize;
        let next = (cur + delta).clamp(0, levels.len() as isize - 1) as usize;
        self.chat_reasoning = levels[next].clone();
    }

    /// The requests/day currently selected on the Recommend screen.
    pub fn chat_requests_per_day(&self) -> usize {
        let levels = crate::chat_catalog::requests_per_day_levels();
        levels[self.chat_requests_idx.min(levels.len() - 1)]
    }

    /// Distinct chat providers from the catalog, in catalog order.
    pub fn chat_providers(&self) -> Vec<String> {
        let mut out: Vec<String> = Vec::new();
        for o in &self.chat_options {
            if !out.iter().any(|v| v == &o.vendor) {
                out.push(o.vendor.clone());
            }
        }
        out
    }

    /// The single recommended conductor for the active stance + requests/day.
    /// The conductor's job is **agentic** (intent → tool calls), so it's ranked on
    /// the agentic domain score — task-aware selection, the same suggestor a
    /// `kind: llm` step will use for its own capability tags.
    pub fn chat_recommended(&self) -> Option<&crate::chat_catalog::ChatModelOption> {
        crate::chat_catalog::recommend_chat_for_affinities(
            &self.chat_options,
            praxec_embeddings::vendor_available,
            &self.priorities,
            self.chat_requests_per_day(),
            &[crate::chat_catalog::Affinity::Agentic],
        )
    }

    /// Reachable, tool-capable chat models, best intelligence first (Browse).
    pub fn chat_reachable(&self) -> Vec<crate::chat_catalog::ChatModelOption> {
        crate::chat_catalog::reachable_chat(&self.chat_options)
    }

    /// Assign the chat model (dismissing the gate) at the chosen reasoning effort.
    fn register_chat(&mut self, opt: &crate::chat_catalog::ChatModelOption, reasoning: &str) {
        let model = crate::llm::ChatModel {
            vendor: opt.vendor.clone(),
            model: opt.model.clone(),
            reasoning_effort: reasoning.to_string(),
        };
        self.chat_log.push(ChatLine {
            you: false,
            text: format!(
                "Connected to {} · {} ({reasoning}). What do you want to do?",
                model.vendor, model.model
            ),
        });
        self.chat_model = Some(model);
    }

    /// Submit the typed chat input as a command. With the LLM enabled, log the
    /// user turn and record a `pending_turn` for the event loop to run
    /// off-thread; otherwise drive the deterministic `parse_command` inline (the
    /// offline fallback). Either way the chosen op flows through `apply`.
    fn submit_command(&mut self) {
        let text = std::mem::take(&mut self.chat_input).trim().to_string();
        if text.is_empty() {
            return;
        }
        self.chat_log.push(ChatLine {
            you: true,
            text: text.clone(),
        });
        // Record the real user turn for the model's context (display ≠ context).
        self.convo.push(crate::agent::Turn {
            you: true,
            text: text.clone(),
        });
        if self.llm_enabled {
            self.thinking = true;
            self.pending_turn = Some(text);
            return;
        }
        let (op, response) = crate::op::parse_command(&text, &self.fleet, self.map.level);
        self.chat_log.push(ChatLine {
            you: false,
            text: response,
        });
        if let Some(op) = op {
            self.apply(op);
        }
    }

    /// Ingest one streamed [`AgentEvent`] on the UI thread. `Text` chunks grow
    /// the in-progress reply (rendered live); `Done` commits the reply and routes
    /// the chosen tool call through the same `op_from_tool_call` dispatch the
    /// keyboard and external agents use; `Failed` narrates the error. Errors
    /// (provider, unknown mission, illegal move) are narrated, never panic.
    pub fn on_agent_event(&mut self, event: crate::agent::AgentEvent) {
        use crate::agent::AgentEvent;
        match event {
            AgentEvent::Text(chunk) => {
                self.streaming
                    .get_or_insert_with(String::new)
                    .push_str(&chunk);
            }
            AgentEvent::Done(tool_call) => {
                self.thinking = false;
                if let Some(text) = self.streaming.take() {
                    let text = text.trim();
                    if !text.is_empty() {
                        self.chat_log.push(ChatLine {
                            you: false,
                            text: text.to_string(),
                        });
                        // The model's own reply joins the conversation context.
                        self.convo.push(crate::agent::Turn {
                            you: false,
                            text: text.to_string(),
                        });
                    }
                }
                if let Some(call) = tool_call {
                    let args: serde_json::Value =
                        serde_json::from_str(&call.arguments).unwrap_or(serde_json::json!({}));
                    match crate::op::op_from_tool_call(
                        &call.name,
                        &args,
                        &self.fleet,
                        self.map.level,
                    ) {
                        Ok(op) => {
                            // Narrate what the conductor did so the transcript is a
                            // coherent intent → action log (the map moves; say so).
                            if let Some(note) = self.describe_op(&op) {
                                self.chat_log.push(ChatLine {
                                    you: false,
                                    text: note,
                                });
                            }
                            self.apply(op);
                        }
                        Err(reason) => self.chat_log.push(ChatLine {
                            you: false,
                            text: reason,
                        }),
                    }
                }
            }
            AgentEvent::Failed(e) => {
                self.thinking = false;
                self.streaming = None; // discard the partial reply; the error explains
                self.chat_log.push(ChatLine {
                    you: false,
                    text: e,
                });
            }
        }
    }

    /// The Mission-level (L1) reducer — the existing single-mission behavior,
    /// plus `⎋` at the mission root zooming back out to the fleet.
    fn on_key_mission(&mut self, key: Key) {
        // Escape zooms back out to the fleet (AskDetail's own Escape meaning is
        // handled before this, in `on_key`).
        if key == Key::Escape {
            self.apply(CockpitOp::ZoomOut);
            return;
        }
        // A real (gateway-backed) mission: ↑↓ select among the legal actions.
        // (Submitting a transition is `praxec.command` — a governed write —
        // deferred to a follow-up; this view is read-only for now.)
        if let Some(resp) = &self.gateway {
            let n = resp.legal_actions().len();
            match key {
                Key::Up => self.action_cursor = self.action_cursor.saturating_sub(1),
                Key::Down => self.action_cursor = (self.action_cursor + 1).min(n.saturating_sub(1)),
                Key::Enter => self.submit_action(),
                _ => {}
            }
            return;
        }
        match key {
            Key::ToggleMode => {
                self.mode = match self.mode {
                    Mode::Build => Mode::Run,
                    Mode::Run => Mode::Build,
                };
                self.nav_index = self.nav_index.min(self.facet_max());
                if self.mode == Mode::Build {
                    self.load_library();
                }
                return;
            }
            Key::Enter => {
                self.on_enter();
                return;
            }
            _ => {}
        }
        match self.focus {
            Focus::Nav => self.on_key_nav(key),
            Focus::Needs => self.on_key_needs(key),
            Focus::Tree => self.on_key_tree(key),
            Focus::AskDetail => {} // handled above
        }
    }

    /// Submit the selected legal action on a real mission — the governed write
    /// (`praxec.command`). Derives the command from the current response (the
    /// link's `rel` is the transition; the workflow's `version` is the optimistic
    /// guard), submits via the connection, and swaps in the post-transition
    /// response. Narrated either way; a stale-version / not-your-move rejection is
    /// surfaced, never silently dropped.
    fn submit_action(&mut self) {
        let cmd = self.gateway.as_ref().and_then(|resp| {
            let actions = resp.legal_actions();
            actions
                .get(self.action_cursor.min(actions.len().saturating_sub(1)))
                .map(|link| {
                    (
                        resp.workflow.id.clone(),
                        resp.workflow.version,
                        link.rel.clone(),
                    )
                })
        });
        let Some((workflow_id, version, transition)) = cmd else {
            return;
        };
        let result = match &self.conn {
            Some(conn) => conn.command(&workflow_id, version, &transition),
            None => {
                self.chat_log.push(ChatLine {
                    you: false,
                    text: "Not connected to a live gateway — this view is read-only.".into(),
                });
                return;
            }
        };
        match result {
            Ok(new_resp) => {
                self.chat_log.push(ChatLine {
                    you: false,
                    text: format!(
                        "→ submitted {transition} (now at {})",
                        new_resp.workflow.state
                    ),
                });
                self.gateway = Some(new_resp);
                self.action_cursor = 0;
            }
            Err(e) => self.chat_log.push(ChatLine {
                you: false,
                text: format!("Couldn't submit {transition}: {e}"),
            }),
        }
    }

    /// The Fleet-level (L0) reducer: arrows pan the tile cursor, `⏎` zooms in,
    /// `q` quits.
    fn on_key_fleet(&mut self, key: Key) {
        // Keys translate to ops on the shared surface (ADR-0005): the same ops
        // the chat LLM will emit via MCP.
        match key {
            Key::Left | Key::Up => self.apply(CockpitOp::Pan(-1)),
            Key::Right | Key::Down => self.apply(CockpitOp::Pan(1)),
            Key::Enter => self.apply(CockpitOp::ZoomInto(self.map.fleet_cursor)),
            _ => {}
        }
    }

    /// Zoom from the Fleet into the selected tile: take a working copy of that
    /// mission's view, reset the tree/HITL cursor state, and start the zoom-in
    /// transition from the tile's rect.
    fn zoom_into_fleet_cursor(&mut self) {
        let n = self.fleet.missions.len();
        let idx = self.map.fleet_cursor;
        let Some(mission) = self.fleet.missions.get(idx) else {
            return;
        };
        let tile = fleet_view::selected_tile_rect(MAP_BODY, n, idx);
        // Extract before mutating (releases the &self.fleet borrow).
        let workflow_id = mission.workflow_id.clone();
        let view = mission.view.clone();
        // ADR-0008 d1 — a live tile opens the real HATEOAS surface (fetched fresh);
        // a demo/fixture tile uses its task-spine view.
        match workflow_id {
            Some(wf_id) => match self.conn.as_ref().map(|c| c.get(&wf_id)) {
                Some(Ok(resp)) => {
                    self.gateway = Some(resp);
                    self.mission = None;
                    self.action_cursor = 0;
                }
                Some(Err(e)) => {
                    self.chat_log.push(ChatLine {
                        you: false,
                        text: format!("Couldn't open mission {wf_id}: {e}"),
                    });
                    return;
                }
                None => return,
            },
            None => {
                self.mission = Some(view);
                self.gateway = None;
            }
        }
        // Reset the single-mission cursor / HITL state for a fresh entry.
        self.selected = 0;
        self.focus = Focus::Tree;
        self.drilled_ask = None;
        self.needs_selected = 0;
        self.ask_choice = 0;
        self.reply.clear();
        self.map.zoom_in(tile, MAP_BODY);
    }

    /// Zoom back out from the active mission to the fleet (shrinking toward the
    /// originating tile). Refreshes the live fleet first so tile statuses are
    /// current (ADR-0008 d1).
    fn zoom_out_to_fleet(&mut self) {
        self.refresh_fleet();
        let n = self.fleet.missions.len();
        let tile = fleet_view::selected_tile_rect(MAP_BODY, n, self.map.fleet_cursor);
        self.map.zoom_out(tile, MAP_BODY);
    }

    /// A short past-tense note describing what `op` does, for narrating an
    /// LLM-driven action in the transcript. `None` for ops not worth logging.
    fn describe_op(&self, op: &CockpitOp) -> Option<String> {
        match op {
            CockpitOp::ZoomInto(idx) => {
                let name = self
                    .fleet
                    .missions
                    .get(*idx)
                    .map(|m| m.name.as_str())
                    .unwrap_or("a mission");
                Some(format!("→ zoomed into {name}"))
            }
            CockpitOp::ZoomOut => Some("→ back to the fleet".to_string()),
            CockpitOp::Pan(_) => Some("→ panned the map".to_string()),
            CockpitOp::Quit => None,
        }
    }

    /// Apply a [`CockpitOp`] — the single entry point both the keyboard and the
    /// chat LLM (via MCP tool call) drive, so the human and the LLM act on one
    /// identical surface (ADR-0005). A no-op while a zoom is animating.
    pub fn apply(&mut self, op: CockpitOp) {
        if self.map.is_transitioning() {
            return;
        }
        match op {
            CockpitOp::Pan(delta) => {
                let n = self.fleet.missions.len();
                self.map.pan(delta, n);
            }
            CockpitOp::ZoomInto(idx) => {
                let n = self.fleet.missions.len();
                if n == 0 {
                    return;
                }
                self.map.fleet_cursor = idx.min(n - 1);
                self.zoom_into_fleet_cursor();
            }
            CockpitOp::ZoomOut => self.zoom_out_to_fleet(),
            CockpitOp::Quit => self.should_quit = true,
        }
    }

    // ── test helpers ─────────────────────────────────────────────────────────

    /// Drive any in-flight zoom to completion (settle the transition).
    pub fn settle_transition(&mut self) {
        for _ in 0..200 {
            self.map.tick_transition(0.05);
        }
    }

    /// Begin a zoom into the selected fleet tile (leaves the transition mid-flight).
    pub fn begin_zoom_into_selected(&mut self) {
        self.on_key(Key::Enter);
    }

    /// Zoom into the selected fleet tile and settle the transition.
    pub fn zoom_into_selected(&mut self) {
        self.begin_zoom_into_selected();
        self.settle_transition();
    }

    fn on_key_nav(&mut self, key: Key) {
        match key {
            Key::Left => self.nav_index = self.nav_index.saturating_sub(1),
            Key::Right => self.nav_index = (self.nav_index + 1).min(self.facet_max()),
            Key::Down => self.focus = Focus::Tree, // descend into the body
            _ => {}
        }
    }

    fn on_key_tree(&mut self, key: Key) {
        match key {
            Key::Up => {
                if self.selected == 0 {
                    self.focus = Focus::Nav; // escape up into the menu
                } else {
                    self.selected -= 1;
                }
            }
            Key::Down => {
                let max = self.selectable_count().saturating_sub(1);
                self.selected = (self.selected + 1).min(max);
            }
            Key::Right => {
                let sel = self.selected;
                if let Some(n) = self
                    .mission
                    .as_mut()
                    .and_then(|m| m.nth_selectable_mut(sel))
                {
                    n.expanded = true;
                }
            }
            Key::Left => {
                let sel = self.selected;
                if let Some(n) = self
                    .mission
                    .as_mut()
                    .and_then(|m| m.nth_selectable_mut(sel))
                {
                    n.expanded = false;
                }
            }
            _ => {}
        }
    }
}

impl Default for App {
    fn default() -> Self {
        Self::new()
    }
}

/// The cockpit's semantic keys (physical arrows; meaning depends on focus).
/// Printable `Char`s feed the always-on chat input (or a HITL reply); `Quit`
/// (Ctrl-C) is the global exit since letters now type.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Key {
    Char(char),
    Backspace,
    ToggleMode,
    Enter,
    /// Ctrl-E — open the selected Build-mode definition to edit (distinct from
    /// ⏎, which launches a workflow). A control chord so it never collides with
    /// the always-live chat input.
    Edit,
    Up,
    Down,
    Left,
    Right,
    Escape,
    Quit,
    Other,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn starts_on_the_status_facet_focused_on_the_tree() {
        let app = App::new();
        assert_eq!((app.nav_index, app.focus), (nav::TREE, Focus::Tree));
    }

    #[test]
    fn selection_starts_at_the_top() {
        let app = App::new().with_mission(MissionView::demo());
        assert_eq!(app.selected, 0);
    }

    #[test]
    fn up_at_the_top_of_the_tree_focuses_the_nav() {
        let mut app = App::new().with_mission(MissionView::demo());
        app.on_key(Key::Up); // selected == 0
        assert_eq!(app.focus, Focus::Nav);
    }

    #[test]
    fn down_in_the_nav_returns_to_the_tree() {
        let mut app = App::new().with_mission(MissionView::demo());
        app.focus = Focus::Nav;
        app.on_key(Key::Down);
        assert_eq!(app.focus, Focus::Tree);
    }

    #[test]
    fn right_in_the_run_nav_cannot_select_a_dead_facet() {
        // COCKPIT-02 — Run advertises a single rendered facet (Status/Tree), so
        // Right in the nav has nowhere to advance: the cursor can never land on
        // an unimplemented "— coming soon" target.
        let mut app = App::new().with_mission(MissionView::demo());
        app.focus = Focus::Nav; // nav_index = TREE (0), the only Run facet
        app.on_key(Key::Right);
        assert_eq!(app.nav_index, nav::TREE);
        assert_eq!(app.nav_index, 0);
    }

    #[test]
    fn down_then_up_returns_to_the_same_node() {
        let mut app = App::new().with_mission(MissionView::demo());
        app.on_key(Key::Down);
        app.on_key(Key::Up);
        assert_eq!((app.selected, app.focus), (0, Focus::Tree));
    }

    #[test]
    fn right_in_the_tree_expands_the_selected_node() {
        let mut app = App::new().with_mission(MissionView::demo());
        app.selected = 0; // D2 (collapsed by default)
        app.on_key(Key::Right);
        assert!(app.mission.as_ref().unwrap().nodes[0].expanded);
    }

    #[test]
    fn tab_toggles_mode() {
        // Mode toggle is a Mission-level key (the single-mission reducer).
        let mut app = App::new().with_mission(MissionView::demo());
        app.on_key(Key::ToggleMode);
        assert_eq!(app.mode, Mode::Build);
    }

    #[test]
    fn tick_advances_the_spinner() {
        let mut app = App::new();
        app.tick();
        assert_eq!(app.tick, 1);
    }

    #[test]
    fn ctrl_c_quits() {
        let mut app = App::new();
        app.on_key(Key::Quit);
        assert!(app.should_quit);
    }

    #[test]
    fn fresh_starts_at_the_chat_gate() {
        let app = App::fresh();
        assert!(app.chat_model.is_none());
        assert_eq!(app.chat_phase, ChatPhase::Providers);
    }

    /// Serializes the embedding-gate tests, which share the global
    /// `PRAXEC_EMBEDDING_FILE` env var.
    fn emb_lock() -> &'static std::sync::Mutex<()> {
        use std::sync::{Mutex, OnceLock};
        static L: OnceLock<Mutex<()>> = OnceLock::new();
        L.get_or_init(|| Mutex::new(()))
    }

    #[test]
    fn embedding_gate_recommend_registers_best_reachable() {
        with_isolated_keys(|| {
            let _g = emb_lock().lock().unwrap();
            let dir = tempfile::tempdir().unwrap();
            let path = dir.path().join("embedding.json");
            // SAFETY: test-local env override, serialized via emb_lock.
            unsafe { std::env::set_var("PRAXEC_EMBEDDING_FILE", &path) };
            let mut app = App::new();
            app.embedding = None;
            app.embedding_gate = true;
            app.embed_phase = EmbedPhase::Recommend;
            app.on_key(Key::Enter); // use the recommendation
            assert!(!app.embedding_gate);
            // No API keys configured → only local reachable → recommends a local model.
            assert_eq!(
                app.embedding.as_ref().map(|c| c.vendor.as_str()),
                Some("ollama")
            );
            assert!(praxec_embeddings::load_choice().is_some());
            unsafe { std::env::remove_var("PRAXEC_EMBEDDING_FILE") };
        });
    }

    #[test]
    fn embedding_gate_provider_key_entry_persists() {
        with_isolated_keys(|| {
            let _g = emb_lock().lock().unwrap();
            let mut app = App::new();
            app.embedding = None;
            app.embedding_gate = true;
            app.embed_phase = EmbedPhase::Providers;
            app.embed_cursor = app
                .embed_providers()
                .iter()
                .position(|p| p == "openai")
                .unwrap();
            app.on_key(Key::Enter); // openai needs a key → key screen
            assert_eq!(app.embed_phase, EmbedPhase::ProviderKey);
            for c in "sk-test".chars() {
                app.on_key(Key::Char(c));
            }
            app.on_key(Key::Enter); // save → back to providers
            assert_eq!(app.embed_phase, EmbedPhase::Providers);
            assert_eq!(std::env::var("OPENAI_API_KEY").ok(), Some("sk-test".into()));
        });
    }

    #[test]
    fn embedding_gate_skip_on_recommend_screen() {
        let _g = emb_lock().lock().unwrap();
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("embedding.json");
        // SAFETY: test-local env override, serialized via emb_lock.
        unsafe { std::env::set_var("PRAXEC_EMBEDDING_FILE", &path) };
        let mut app = App::new();
        app.embedding = None;
        app.embedding_gate = true;
        app.embed_phase = EmbedPhase::Recommend;
        app.on_key(Key::Escape); // skip from screen 2 (e.g. cost too high)
        assert!(app.embedding.is_none());
        assert!(!app.embedding_gate);
        assert_eq!(
            praxec_embeddings::load_setting(),
            Some(praxec_embeddings::EmbeddingSetting::Lexical)
        );
        unsafe { std::env::remove_var("PRAXEC_EMBEDDING_FILE") };
    }

    /// Run `f` with provider keys isolated to a fresh temp file and the gate
    /// vendors' env keys cleared, so setup-flow tests neither read nor write
    /// real provider config. Serialized via a process-global lock (env is
    /// global to the test binary).
    fn with_isolated_keys(f: impl FnOnce()) {
        use std::sync::{Mutex, OnceLock};
        static LOCK: OnceLock<Mutex<()>> = OnceLock::new();
        let _g = LOCK.get_or_init(|| Mutex::new(())).lock().unwrap();

        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("providers.env");
        let keys = [
            "ANTHROPIC_API_KEY",
            "OPENAI_API_KEY",
            "GEMINI_API_KEY",
            "OPENROUTER_API_KEY",
        ];
        let saved: Vec<(&str, Option<String>)> =
            keys.iter().map(|k| (*k, std::env::var(k).ok())).collect();
        // SAFETY: the lock serializes env access across this test binary.
        unsafe {
            std::env::set_var("PRAXEC_PROVIDER_KEYS_FILE", &path);
            for k in &keys {
                std::env::remove_var(k);
            }
        }

        f();

        unsafe {
            std::env::remove_var("PRAXEC_PROVIDER_KEYS_FILE");
            for (k, v) in saved {
                match v {
                    Some(v) => std::env::set_var(k, v),
                    None => std::env::remove_var(k),
                }
            }
        }
    }

    #[test]
    fn chat_gate_provider_key_entry_persists() {
        with_isolated_keys(|| {
            let mut app = App::fresh();
            app.chat_phase = ChatPhase::Providers;
            app.chat_cursor = app
                .chat_providers()
                .iter()
                .position(|p| p == "openai")
                .unwrap();
            app.on_key(Key::Enter); // openai needs a key → key screen
            assert_eq!(app.chat_phase, ChatPhase::ProviderKey);
            for c in "sk-test".chars() {
                app.on_key(Key::Char(c));
            }
            app.on_key(Key::Enter); // save → back to providers
            assert_eq!(app.chat_phase, ChatPhase::Providers);
            assert_eq!(std::env::var("OPENAI_API_KEY").ok(), Some("sk-test".into()));
        });
    }

    #[test]
    fn chat_gate_recommend_registers_the_stance_pick() {
        with_isolated_keys(|| {
            // SAFETY: inside the isolation lock.
            unsafe { std::env::set_var("OPENROUTER_API_KEY", "sk-or") };
            let mut app = App::fresh();
            app.chat_phase = ChatPhase::Recommend;
            // A reachable tool-calling model exists → Enter registers it with the
            // shown reasoning effort.
            let expected = app.chat_recommended().cloned().expect("a recommendation");
            app.on_key(Key::Enter);
            let cm = app.chat_model.as_ref().expect("a chat model was assigned");
            assert_eq!(cm.vendor, expected.vendor);
            assert_eq!(cm.model, expected.model);
            assert!(!cm.reasoning_effort.is_empty());
        });
    }

    #[test]
    fn chat_gate_reasoning_cycles_within_the_model_levels() {
        with_isolated_keys(|| {
            // SAFETY: inside the isolation lock.
            unsafe { std::env::set_var("OPENROUTER_API_KEY", "sk-or") };
            let mut app = App::fresh();
            app.chat_cursor = app.chat_providers().len(); // "Continue"
            app.on_key(Key::Enter); // → Recommend, reasoning seeded to the default
            let levels = app.chat_recommended().unwrap().reasoning_levels.clone();
            if levels.len() > 1 {
                let before = app.chat_reasoning.clone();
                app.on_key(Key::Down); // step effort up the ladder
                assert_ne!(app.chat_reasoning, before);
                assert!(levels.contains(&app.chat_reasoning));
            }
        });
    }

    #[test]
    fn chat_gate_requests_per_day_changes_the_cost_preset() {
        let mut app = App::new();
        app.chat_model = None;
        app.chat_phase = ChatPhase::Recommend;
        app.chat_requests_idx = 0;
        app.on_key(Key::Right);
        assert_eq!(app.chat_requests_idx, 1);
        app.on_key(Key::Left);
        assert_eq!(app.chat_requests_idx, 0);
    }

    #[test]
    fn detected_complete_config_skips_the_gate() {
        let mut app = App::fresh();
        app.apply_detection(crate::llm::Detected::Complete(crate::llm::ChatModel {
            vendor: "anthropic".into(),
            model: "claude-opus-4-8".into(),
            reasoning_effort: "medium".into(),
        }));
        assert_eq!(
            app.chat_model.as_ref().map(|m| m.model.as_str()),
            Some("claude-opus-4-8")
        );
    }

    // ── priorities panel + Settings overlay ──────────────────────────────────

    /// Run `f` with the priorities file isolated to a temp path (serialized, as
    /// the env var is process-global).
    fn with_isolated_priorities(f: impl FnOnce()) {
        use std::sync::{Mutex, OnceLock};
        static LOCK: OnceLock<Mutex<()>> = OnceLock::new();
        let _g = LOCK.get_or_init(|| Mutex::new(())).lock().unwrap();
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("priorities.json");
        // SAFETY: serialized by LOCK; env is global to the test binary.
        unsafe { std::env::set_var("PRAXEC_PRIORITIES_FILE", &path) };
        f();
        unsafe { std::env::remove_var("PRAXEC_PRIORITIES_FILE") };
    }

    #[test]
    fn g_on_empty_input_opens_settings() {
        let mut app = App::new();
        app.on_key(Key::Char('g'));
        assert!(app.settings_open);
        assert_eq!(app.chat_input, ""); // 'g' was a hotkey, not typed
    }

    #[test]
    fn g_after_typing_is_just_a_character() {
        let mut app = App::new();
        app.on_key(Key::Char('p'));
        app.on_key(Key::Char('g')); // input non-empty → 'g' types
        assert!(!app.settings_open);
        assert_eq!(app.chat_input, "pg");
    }

    #[test]
    fn priorities_panel_selects_a_stance_and_persists() {
        with_isolated_priorities(|| {
            use crate::priorities::Stance;
            let mut app = App::new();
            app.priorities_gate = true;
            // Cursor to the "Fastest responses" row (index 3) and choose it.
            let fastest = Stance::ALL
                .iter()
                .position(|s| *s == Stance::Fastest)
                .unwrap();
            for _ in 0..fastest {
                app.on_key(Key::Down);
            }
            app.on_key(Key::Enter); // select + finish (first run)
            assert_eq!(app.priorities.stance, Stance::Fastest);
            assert!(!app.priorities_gate); // gate dismissed
            assert_eq!(
                crate::priorities::Priorities::load().map(|p| p.stance),
                Some(Stance::Fastest)
            );
        });
    }

    #[test]
    fn priorities_panel_cycles_the_budget_ceiling() {
        with_isolated_priorities(|| {
            let mut app = App::new();
            app.priorities_gate = true;
            app.prio_cursor = crate::priorities::Stance::ALL.len(); // the budget row
            assert!(app.priorities.budget_cap.is_none());
            app.on_key(Key::Right); // → first non-None cap
            assert!(app.priorities.budget_cap.is_some());
            app.on_key(Key::Left); // back to no cap
            assert!(app.priorities.budget_cap.is_none());
        });
    }

    #[test]
    fn escape_at_first_run_accepts_the_default_balanced() {
        with_isolated_priorities(|| {
            let mut app = App::new();
            app.priorities_gate = true;
            app.on_key(Key::Escape);
            assert!(!app.priorities_gate);
            assert_eq!(
                crate::priorities::Priorities::load().map(|p| p.stance),
                Some(crate::priorities::Stance::Balanced)
            );
        });
    }

    #[test]
    fn settings_drills_into_priorities_then_escapes_back_to_the_menu() {
        with_isolated_priorities(|| {
            let mut app = App::new();
            app.on_key(Key::Char('g')); // open settings (cursor on Priorities)
            app.on_key(Key::Enter); // drill into the priorities panel
            assert!(app.settings_editing_priorities);
            app.on_key(Key::Escape); // back to the menu
            assert!(!app.settings_editing_priorities);
            assert!(app.settings_open); // still in Settings
        });
    }

    #[test]
    fn settings_embedding_row_reopens_the_embedding_gate() {
        let mut app = App::new();
        app.on_key(Key::Char('g'));
        app.on_key(Key::Down); // → Chat model
        app.on_key(Key::Down); // → Embedding model
        app.on_key(Key::Enter);
        assert!(!app.settings_open);
        assert!(app.embedding_gate);
        assert_eq!(app.embed_phase, EmbedPhase::Providers);
    }

    #[test]
    fn settings_chat_model_row_reopens_the_setup_gate() {
        let mut app = App::new();
        app.on_key(Key::Char('g'));
        app.on_key(Key::Down); // → Chat model
        app.on_key(Key::Enter);
        assert!(!app.settings_open);
        assert!(app.chat_model.is_none()); // setup gate now owns the keyboard
    }

    #[test]
    fn printable_keys_build_the_chat_input() {
        let mut app = App::new();
        app.on_key(Key::Char('h'));
        app.on_key(Key::Char('i'));
        assert_eq!(app.chat_input, "hi");
    }

    #[test]
    fn enter_runs_the_typed_command_and_navigates() {
        let mut app = App::new(); // Fleet
        for c in "postgres".chars() {
            app.on_key(Key::Char(c));
        }
        app.on_key(Key::Enter); // submit
        assert_eq!(app.map.mission, Some(2)); // zoomed into the postgres mission
    }

    #[test]
    fn llm_mode_records_a_pending_turn_instead_of_parsing() {
        let mut app = App::new();
        app.llm_enabled = true;
        for c in "anything".chars() {
            app.on_key(Key::Char(c));
        }
        app.on_key(Key::Enter);
        assert_eq!(app.pending_turn.as_deref(), Some("anything"));
        assert!(app.thinking);
        // The user line is logged; no deterministic response was appended.
        assert_eq!(app.chat_log.last().map(|l| l.you), Some(true));
    }

    #[test]
    fn launching_a_workflow_tracks_it_and_zooms_into_the_live_mission() {
        use crate::gateway::FakeGateway;
        let mut app = App::new();
        app.conn = Some(Box::new(FakeGateway::editing_demo()));
        app.launch_definition("cognitive/flow.safe-refactor"); // a workflow in the fixture library
        assert_eq!(app.roster.len(), 1, "the launched mission joins the roster");
        assert!(
            app.gateway.is_some(),
            "the live mission becomes the active view"
        );
        assert_eq!(app.map.level, crate::map::Level::Mission, "we zoom into it");
        assert_eq!(app.mode, Mode::Run);
        assert!(app.chat_log.last().unwrap().text.contains("Launched"));
    }

    #[test]
    fn launched_missions_populate_the_live_fleet() {
        use crate::gateway::FakeGateway;
        let mut app = App::new();
        app.conn = Some(Box::new(FakeGateway::editing_demo()));
        app.launch_definition("cognitive/flow.safe-refactor"); // refresh_fleet runs inside
        assert_eq!(
            app.fleet.missions.len(),
            1,
            "the live fleet is built from the roster"
        );
        assert!(
            app.fleet.missions[0].workflow_id.is_some(),
            "a live tile carries its instance id so zoom fetches the real surface"
        );
    }

    fn inbox_item(id: &str) -> crate::mediator::InboxItem {
        crate::mediator::InboxItem {
            mission_id: id.into(),
            definition_id: "f".into(),
            version: 1,
            prompt: "?".into(),
            choices: vec!["approve".into()],
        }
    }

    #[test]
    fn answering_an_inbox_item_narrates_the_answer() {
        use crate::gateway::FakeGateway;
        let mut app = App::new();
        app.conn = Some(Box::new(FakeGateway::editing_demo()));
        app.inbox = vec![inbox_item("m1")];
        app.answer_inbox(0, "approve");
        assert!(app.chat_log.last().unwrap().text.contains("Answered"));
    }

    #[test]
    fn answering_an_inbox_item_without_a_connection_narrates_not_connected() {
        let mut app = App::new();
        app.conn = None;
        app.inbox = vec![inbox_item("m1")];
        app.answer_inbox(0, "approve");
        assert!(app.chat_log.last().unwrap().text.contains("not connected"));
    }

    #[test]
    fn answering_an_out_of_range_inbox_index_is_a_no_op() {
        use crate::gateway::FakeGateway;
        let mut app = App::new();
        app.conn = Some(Box::new(FakeGateway::editing_demo()));
        let before = app.chat_log.len();
        app.answer_inbox(99, "approve");
        assert_eq!(app.chat_log.len(), before);
    }

    // ── H9: the mediator inbox is answerable from the keyboard ────────────────

    #[test]
    fn pressing_a_opens_the_inbox_when_something_is_waiting() {
        let mut app = App::new();
        app.inbox = vec![inbox_item("m1")];
        app.on_key(Key::Char('a'));
        assert!(app.inbox_open, "`a` opens the mediator inbox overlay");
    }

    #[test]
    fn pressing_a_with_an_empty_inbox_does_not_open_an_empty_overlay() {
        let mut app = App::new();
        assert!(app.inbox.is_empty());
        app.on_key(Key::Char('a'));
        assert!(!app.inbox_open, "no parked missions → nothing to open");
        // …and it doesn't get swallowed as a hotkey: it types into chat instead.
        assert_eq!(app.chat_input, "a");
    }

    #[test]
    fn answering_from_the_inbox_overlay_dispatches_the_chosen_transition() {
        use crate::gateway::ScriptedGateway;
        let gw = ScriptedGateway::new(vec![mission_response(
            "m1",
            "waiting",
            serde_json::json!([{ "rel": "reject", "actor": "human" }]),
        )]);
        let log = gw.log();
        let mut app = App::new();
        app.conn = Some(Box::new(gw));
        // Two human choices; the handler must submit the *highlighted* one.
        app.inbox = vec![crate::mediator::InboxItem {
            mission_id: "m1".into(),
            definition_id: "f".into(),
            version: 1,
            prompt: "?".into(),
            choices: vec!["approve".into(), "reject".into()],
        }];
        app.on_key(Key::Char('a')); // open the overlay
        assert!(app.inbox_open);
        app.on_key(Key::Right); // move choice → "reject"
        app.on_key(Key::Enter); // answer
        assert_eq!(
            log.commands().first().cloned(),
            Some(("m1".to_string(), 1, "reject".to_string())),
            "the overlay must dispatch the highlighted human transition via answer_inbox"
        );
    }

    #[test]
    fn escape_closes_the_inbox_overlay() {
        let mut app = App::new();
        app.inbox = vec![inbox_item("m1")];
        app.on_key(Key::Char('a'));
        assert!(app.inbox_open);
        app.on_key(Key::Escape);
        assert!(!app.inbox_open);
    }

    // ── S1: Cockpit ↔ Backend integration (the §32 seam, scriptable mock) ─────

    fn mission_response(
        id: &str,
        status: &str,
        links: serde_json::Value,
    ) -> crate::model::GatewayResponse {
        serde_json::from_value(serde_json::json!({
            "workflow": { "id": id, "definitionId": "flow.x", "state": "s", "version": 3 },
            "result": { "status": status },
            "links": links,
        }))
        .unwrap()
    }

    #[test]
    fn launch_sends_the_definition_to_the_gateway() {
        use crate::gateway::ScriptedGateway;
        let gw = ScriptedGateway::new(vec![mission_response(
            "m1",
            "running",
            serde_json::json!([]),
        )]);
        let log = gw.log();
        let mut app = App::new();
        app.conn = Some(Box::new(gw));
        app.launch_definition("flow.x");
        assert_eq!(
            log.launches().first().map(|(d, _)| d.clone()),
            Some("flow.x".to_string())
        );
    }

    #[test]
    fn launch_makes_the_returned_mission_the_active_one() {
        use crate::gateway::ScriptedGateway;
        let gw = ScriptedGateway::new(vec![mission_response(
            "m1",
            "running",
            serde_json::json!([]),
        )]);
        let mut app = App::new();
        app.conn = Some(Box::new(gw));
        app.launch_definition("flow.x");
        assert_eq!(
            app.gateway.as_ref().map(|g| g.workflow.id.clone()),
            Some("m1".to_string())
        );
    }

    #[test]
    fn answer_inbox_sends_the_exact_command_to_the_gateway() {
        use crate::gateway::ScriptedGateway;
        let gw = ScriptedGateway::new(vec![mission_response(
            "m1",
            "waiting",
            serde_json::json!([{ "rel": "approve", "actor": "human" }]),
        )]);
        let log = gw.log();
        let mut app = App::new();
        app.conn = Some(Box::new(gw));
        app.inbox = vec![inbox_item("m1")]; // version 1
        app.answer_inbox(0, "approve");
        assert_eq!(
            log.commands().first().cloned(),
            Some(("m1".to_string(), 1, "approve".to_string()))
        );
    }

    #[rstest::rstest]
    #[case("waiting", true, 1)] // waiting + a human move → the human's concern
    #[case("running", true, 0)] // running → not yet
    #[case("succeeded", false, 0)] // resolved → done
    #[case("waiting", false, 0)] // waiting but only an agent move → not the human's
    fn refresh_fleet_derives_inbox_membership(
        #[case] status: &str,
        #[case] human: bool,
        #[case] expected: usize,
    ) {
        use crate::gateway::ScriptedGateway;
        let links = if human {
            serde_json::json!([{ "rel": "approve", "actor": "human" }])
        } else {
            serde_json::json!([{ "rel": "go", "actor": "agent" }])
        };
        let gw = ScriptedGateway::new(vec![mission_response("m1", status, links)]);
        let mut app = App::new();
        app.conn = Some(Box::new(gw));
        app.roster = vec!["m1".into()];
        app.refresh_fleet();
        assert_eq!(app.inbox.len(), expected);
    }

    #[test]
    fn launching_without_a_connection_is_narrated_not_panicked() {
        let mut app = App::new();
        app.conn = None;
        app.launch_definition("cognitive/flow.safe-refactor");
        assert!(app.roster.is_empty());
        assert!(app.chat_log.last().unwrap().text.contains("not connected"));
    }

    #[test]
    fn launch_surfaces_a_gateway_rejection_without_rostering() {
        use crate::gateway::ScriptedGateway;
        // An exhausted queue makes the gateway return Err — the launch path must
        // narrate it and leave the roster untouched (no phantom mission).
        let gw = ScriptedGateway::new(vec![]);
        let mut app = App::new();
        app.conn = Some(Box::new(gw));
        app.launch_definition("flow.x");
        assert!(app.roster.is_empty());
    }

    #[test]
    fn launch_rejection_is_narrated() {
        use crate::gateway::ScriptedGateway;
        let gw = ScriptedGateway::new(vec![]);
        let mut app = App::new();
        app.conn = Some(Box::new(gw));
        app.launch_definition("flow.x");
        assert!(app
            .chat_log
            .last()
            .is_some_and(|l| l.text.contains("Couldn't launch")));
    }

    #[test]
    fn answer_inbox_without_a_connection_is_narrated() {
        let mut app = App::new();
        app.conn = None;
        app.inbox = vec![inbox_item("m1")];
        app.answer_inbox(0, "approve");
        assert!(app
            .chat_log
            .last()
            .is_some_and(|l| l.text.contains("not connected")));
    }

    #[test]
    fn answer_inbox_surfaces_a_gateway_rejection() {
        use crate::gateway::ScriptedGateway;
        // Empty queue → the command call returns Err (e.g. a stale-version reject).
        let gw = ScriptedGateway::new(vec![]);
        let mut app = App::new();
        app.conn = Some(Box::new(gw));
        app.inbox = vec![inbox_item("m1")];
        app.answer_inbox(0, "approve");
        assert!(app
            .chat_log
            .last()
            .is_some_and(|l| l.text.contains("Couldn't answer")));
    }

    #[test]
    fn answer_inbox_with_an_out_of_range_index_is_a_no_op() {
        use crate::gateway::ScriptedGateway;
        let gw = ScriptedGateway::new(vec![]);
        let log = gw.log();
        let mut app = App::new();
        app.conn = Some(Box::new(gw));
        app.inbox = vec![inbox_item("m1")];
        app.answer_inbox(5, "approve"); // no item at index 5
        assert!(
            log.commands().is_empty(),
            "no command is sent for a bad index"
        );
    }

    #[test]
    fn the_full_flow_auto_refreshes_a_launched_mission_into_the_inbox() {
        use crate::gateway::ScriptedGateway;
        // launch → running; the auto refresh_fleet get → waiting+human gate.
        let gw = ScriptedGateway::new(vec![
            mission_response("m1", "running", serde_json::json!([])),
            mission_response(
                "m1",
                "waiting",
                serde_json::json!([{ "rel": "approve", "actor": "human" }]),
            ),
        ]);
        let mut app = App::new();
        app.conn = Some(Box::new(gw));
        app.launch_definition("flow.x");
        assert_eq!(
            app.inbox.len(),
            1,
            "the launched mission's human gate lands in the inbox"
        );
    }

    #[test]
    fn the_full_flow_records_launch_then_answer_as_two_gateway_calls() {
        use crate::gateway::ScriptedGateway;
        let gw = ScriptedGateway::new(vec![
            mission_response("m1", "running", serde_json::json!([])), // launch
            mission_response(
                "m1",
                "waiting",
                serde_json::json!([{ "rel": "approve", "actor": "human" }]),
            ), // auto-refresh get
            mission_response("m1", "succeeded", serde_json::json!([])), // answer command
        ]);
        let log = gw.log();
        let mut app = App::new();
        app.conn = Some(Box::new(gw));
        app.launch_definition("flow.x");
        app.answer_inbox(0, "approve");
        // The seam end-to-end: one launch (flow.x) and one command (approve on m1).
        assert_eq!(
            (
                log.launches().len(),
                log.commands().first().map(|(_, _, t)| t.clone())
            ),
            (1, Some("approve".to_string()))
        );
    }

    #[test]
    fn real_mission_enter_submits_the_selected_action() {
        use crate::gateway::{FakeGateway, Gateway};
        let mut app = App::new();
        app.gateway = Some(
            FakeGateway::editing_demo()
                .get("wf_safe_refactor_01")
                .unwrap(),
        );
        app.conn = Some(Box::new(FakeGateway::editing_demo()));
        app.map.level = crate::map::Level::Mission;
        let before = app.chat_log.len();
        app.on_key(Key::Enter); // submit the selected legal action
        assert!(app.chat_log.len() > before);
        assert!(app.chat_log.last().unwrap().text.contains("submitted"));
    }

    #[test]
    fn real_mission_submit_without_a_connection_is_read_only() {
        use crate::gateway::{FakeGateway, Gateway};
        let mut app = App::new();
        app.gateway = Some(
            FakeGateway::editing_demo()
                .get("wf_safe_refactor_01")
                .unwrap(),
        );
        app.conn = None; // snapshot/demo: no live connection
        app.map.level = crate::map::Level::Mission;
        app.on_key(Key::Enter);
        assert!(app.chat_log.last().unwrap().text.contains("read-only"));
    }

    #[test]
    fn convo_holds_only_real_turns_not_ui_chrome() {
        use crate::agent::{AgentEvent, ToolCallRequest};
        let mut app = App::new();
        app.llm_enabled = true;
        for c in "what needs me".chars() {
            app.on_key(Key::Char(c));
        }
        app.on_key(Key::Enter); // submit → one real user turn in convo
        app.on_agent_event(AgentEvent::Text("On it.".into()));
        app.on_agent_event(AgentEvent::Done(Some(ToolCallRequest {
            id: "c1".into(),
            name: "zoom_into".into(),
            arguments: "{\"mission\":\"postgres\"}".into(),
        })));
        // convo = [user, assistant("On it.")] — the greeting + the "→ zoomed into"
        // narration are chrome and stay out of the model's context.
        assert_eq!(app.convo.len(), 2);
        assert!(app.convo[0].you && app.convo[0].text == "what needs me");
        assert!(!app.convo[1].you && app.convo[1].text == "On it.");
        assert!(
            app.chat_log.len() > app.convo.len(),
            "display log carries chrome convo does not"
        );
    }

    #[test]
    fn streamed_text_accumulates_then_commits_on_done() {
        use crate::agent::AgentEvent;
        let mut app = App::new();
        app.thinking = true;
        app.on_agent_event(AgentEvent::Text("Look".into()));
        app.on_agent_event(AgentEvent::Text("ing…".into()));
        assert_eq!(app.streaming.as_deref(), Some("Looking…")); // live, not yet committed
        app.on_agent_event(AgentEvent::Done(None));
        assert!(!app.thinking);
        assert!(app.streaming.is_none());
        assert_eq!(
            app.chat_log.last().map(|l| l.text.as_str()),
            Some("Looking…")
        );
    }

    #[test]
    fn done_routes_a_tool_call_through_apply() {
        use crate::agent::{AgentEvent, ToolCallRequest};
        let mut app = App::new();
        app.thinking = true;
        app.on_agent_event(AgentEvent::Done(Some(ToolCallRequest {
            id: "c1".into(),
            name: "zoom_into".into(),
            arguments: "{\"mission\":\"postgres\"}".into(),
        })));
        assert!(!app.thinking);
        assert_eq!(app.map.mission, Some(2));
        // The action is narrated into the transcript (intent → action log).
        assert!(
            app.chat_log
                .last()
                .map(|l| l.text.contains("zoomed into"))
                .unwrap_or(false),
            "the conductor's zoom should be narrated"
        );
    }

    #[test]
    fn failed_discards_the_partial_and_narrates() {
        use crate::agent::AgentEvent;
        let mut app = App::new();
        app.thinking = true;
        app.on_agent_event(AgentEvent::Text("half a tho".into()));
        app.on_agent_event(AgentEvent::Failed("couldn't reach the model".into()));
        assert!(!app.thinking);
        assert!(app.streaming.is_none());
        assert_eq!(
            app.chat_log.last().map(|l| l.text.as_str()),
            Some("couldn't reach the model")
        );
    }

    #[test]
    fn typing_in_the_ask_detail_fills_the_reply() {
        let mut app = App::new().with_mission(MissionView::demo());
        app.on_key(Key::Enter); // open the asks list
        app.on_key(Key::Enter); // drill into the ask
        app.on_key(Key::Char('h'));
        app.on_key(Key::Char('i'));
        assert_eq!(app.reply, "hi");
    }

    #[test]
    fn q_in_the_ask_detail_types_not_quits() {
        let mut app = App::new().with_mission(MissionView::demo());
        app.on_key(Key::Enter);
        app.on_key(Key::Enter); // in AskDetail
        app.on_key(Key::Char('q'));
        assert!(!app.should_quit);
    }

    #[test]
    fn sending_a_typed_reply_resolves_the_ask() {
        let mut app = App::new().with_mission(MissionView::demo());
        app.on_key(Key::Enter);
        app.on_key(Key::Enter); // drill in
        app.on_key(Key::Char('y'));
        app.on_key(Key::Char('o'));
        app.on_key(Key::Enter); // send
        assert!(app
            .mission
            .as_ref()
            .unwrap()
            .needs_you_with_context()
            .is_empty());
    }

    #[test]
    fn enter_focuses_the_needs_queue() {
        let mut app = App::new().with_mission(MissionView::demo());
        app.on_key(Key::Enter);
        assert_eq!(app.focus, Focus::Needs);
    }

    #[test]
    fn enter_on_an_ask_drills_into_its_detail() {
        let mut app = App::new().with_mission(MissionView::demo());
        app.on_key(Key::Enter); // Tree → the asks list
        app.on_key(Key::Enter); // list → drill into the ask
        assert_eq!(app.focus, Focus::AskDetail);
    }

    #[test]
    fn left_backs_out_of_the_ask_detail_to_the_list() {
        let mut app = App::new().with_mission(MissionView::demo());
        app.on_key(Key::Enter);
        app.on_key(Key::Enter); // now in AskDetail
        app.on_key(Key::Left);
        assert_eq!(app.focus, Focus::Needs);
    }

    #[test]
    fn enter_in_the_detail_takes_the_choice_and_drains_the_ask() {
        let mut app = App::new().with_mission(MissionView::demo());
        app.on_key(Key::Enter); // → list
        app.on_key(Key::Enter); // → drill in
        app.on_key(Key::Enter); // → take the choice, resolving the ask
        assert!(app
            .mission
            .as_ref()
            .unwrap()
            .needs_you_with_context()
            .is_empty());
    }

    // ── map altitude (Fleet ⇄ Mission) ───────────────────────────────────────

    #[test]
    fn fleet_arrows_pan_the_cursor() {
        let mut app = App::new();
        app.on_key(Key::Right);
        assert_eq!(app.map.fleet_cursor, 1);
    }

    #[test]
    fn enter_at_fleet_zooms_into_the_mission() {
        let mut app = App::new();
        app.on_key(Key::Enter);
        assert_eq!(app.map.level, Level::Mission);
    }

    #[test]
    fn apply_pan_moves_the_fleet_cursor() {
        let mut app = App::new();
        app.apply(crate::op::CockpitOp::Pan(1));
        assert_eq!(app.map.fleet_cursor, 1);
    }

    #[test]
    fn apply_zoom_into_enters_that_mission_by_index() {
        let mut app = App::new();
        app.apply(crate::op::CockpitOp::ZoomInto(2));
        assert_eq!(app.map.mission, Some(2));
    }

    #[test]
    fn apply_quit_sets_should_quit() {
        let mut app = App::new();
        app.apply(crate::op::CockpitOp::Quit);
        assert!(app.should_quit);
    }

    #[test]
    fn esc_at_mission_root_zooms_back_to_fleet() {
        let mut app = App::new();
        app.zoom_into_selected();
        app.on_key(Key::Escape);
        app.settle_transition();
        assert_eq!(app.map.level, Level::Fleet);
    }

    #[test]
    fn input_is_ignored_while_transitioning() {
        let mut app = App::new();
        app.begin_zoom_into_selected();
        let before = app.map.fleet_cursor;
        app.on_key(Key::Right);
        assert_eq!(app.map.fleet_cursor, before);
    }
}
