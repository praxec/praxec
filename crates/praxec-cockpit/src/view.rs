//! The cockpit's view model — a live mission **tree** (the "Status" facet),
//! organized **task-first**: the spine is the plan's deliverables (CPM tasks,
//! with critical-path / parallel / scope markers), and the agent(s) + steps
//! executing each task nest beneath it. Each node carries its state and (for
//! running nodes) its executor *kind*, which drives a distinct spinner.
//! Human-in-the-loop actions are pulled into a separate needs-you queue
//! (rendered as a dynamic sidebar), not inline.

#[derive(Clone)]
pub struct MissionView {
    pub name: String,
    pub orchestrator: String,
    pub nodes: Vec<TaskNode>,
}

/// What a tree node *is*. The spine is `Task`s (plan deliverables); the work
/// under each task is the executing `Agent` (a skill on a model) and its
/// `Step`s. Keeping these distinct is what makes the tree task-first, not
/// agent-centric.
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum NodeRole {
    /// A plan deliverable — the CPM task spine.
    Task,
    /// A worker: a skill running on a model binding.
    Agent,
    /// A unit of work an agent executes (skill / tool / llm / script).
    Step,
}

#[derive(Clone)]
pub struct TaskNode {
    pub name: String,
    pub role: NodeRole,
    pub state: NodeState,
    /// The kind of executor running this node — picks the spinner style.
    pub kind: Option<ExecutorKind>,
    /// Compact executor descriptor (a model binding, or a skill name).
    pub detail: Option<String>,
    /// Task spine: on the plan's critical path.
    pub critical: bool,
    /// Task spine: owned files (the lock scope), summarized.
    pub scope: Option<String>,
    /// Task spine: a deliverable this runs in parallel with (same CPM batch).
    pub parallel_with: Option<String>,
    pub harness: Vec<Harness>,
    /// For NeedsYou nodes: the kind of interaction required (gate / form /
    /// question / discuss). Drives the typed "your move" panel.
    pub hitl: Option<Hitl>,
    /// The ask's embedded conversation — rendered in the drill-in detail. The
    /// reply input appends to this; seeded with the agent's opening turn(s).
    pub thread: Vec<ChatTurn>,
    /// "Your move" affordances — the selectable choices in the panel.
    pub actions: Vec<String>,
    /// For Blocked nodes: what it waits on (an upstream task or external dep).
    pub waits_on: Vec<String>,
    pub children: Vec<TaskNode>,
    pub expanded: bool,
}

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum NodeState {
    Done,
    Running,
    NeedsYou,
    Blocked,
    Pending,
    Failed,
}

/// Executor kind — each gets its own spinner so you can read *what kind* of
/// work is happening at a glance (a thinking LLM vs a turning agent vs a
/// churning tool).
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum ExecutorKind {
    Llm,
    Agent,
    Tool,
    Script,
}

#[derive(Clone)]
pub struct Harness {
    pub name: String,
    pub verdict: Verdict,
}

#[derive(Clone)]
pub enum Verdict {
    Ok,
    Warn(String),
}

/// The *kind* of human-in-the-loop interaction a `NeedsYou` node requires.
/// Human-in-the-loop is a spectrum, not a click — this grounds the cockpit's
/// "your move" surface in the system's real HITL modes:
///
/// - [`Hitl::Approve`] — a gate (`actor: human`): approve, or send back.
/// - [`Hitl::Form`] — fill a typed form (a transition `inputSchema`).
/// - [`Hitl::Answer`] — answer a question the agent asked (SPEC §29 `ask_human`,
///   the one-way LLM→human channel).
/// - [`Hitl::Discuss`] — open-ended back-and-forth, handed off to the chat
///   surface (the deferred human→LLM dialogue, SPEC §29.7).
#[derive(Clone)]
pub enum Hitl {
    Approve,
    Form { fields: Vec<String> },
    Answer { question: String },
    Discuss { topic: String },
}

impl Hitl {
    /// Short uppercase tag shown in the panel so the interaction kind is
    /// legible at a glance.
    pub fn tag(&self) -> &'static str {
        match self {
            Hitl::Approve => "APPROVE",
            Hitl::Form { .. } => "FORM",
            Hitl::Answer { .. } => "ASK",
            Hitl::Discuss { .. } => "DISCUSS",
        }
    }
}

/// Who said a line in an ask's embedded conversation.
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Speaker {
    Agent,
    You,
}

/// One turn in the scoped chat embedded in an ask's detail. The discuss
/// interaction lives *inside* the cockpit (a thread about this one ask), not
/// handed off to a separate surface — the reply input grows into this thread.
/// Live multi-turn human↔agent dialogue is the deferred capability (SPEC
/// §29.7); a single reply (the answer) is what resolves the ask today.
#[derive(Clone)]
pub struct ChatTurn {
    pub speaker: Speaker,
    pub text: String,
}

impl ChatTurn {
    fn agent(text: &str) -> Self {
        ChatTurn {
            speaker: Speaker::Agent,
            text: text.into(),
        }
    }
    fn you(text: &str) -> Self {
        ChatTurn {
            speaker: Speaker::You,
            text: text.into(),
        }
    }
}

pub struct Counts {
    pub running: usize,
    pub blocked: usize,
    pub needs_you: usize,
    pub failed: usize,
}

impl TaskNode {
    fn base(name: &str, role: NodeRole, state: NodeState) -> Self {
        TaskNode {
            name: name.into(),
            role,
            state,
            kind: None,
            detail: None,
            critical: false,
            scope: None,
            parallel_with: None,
            harness: Vec::new(),
            hitl: None,
            thread: Vec::new(),
            actions: Vec::new(),
            waits_on: Vec::new(),
            children: Vec::new(),
            expanded: false,
        }
    }

    /// A plan deliverable — the task spine. Its children are the agent(s) and
    /// steps executing it.
    fn task(name: &str, state: NodeState, children: Vec<TaskNode>) -> Self {
        let mut n = Self::base(name, NodeRole::Task, state);
        n.children = children;
        n
    }

    /// An agent node — a worker identified by its model binding, with steps
    /// and sub-agents as children.
    fn agent(name: &str, model: &str, state: NodeState, children: Vec<TaskNode>) -> Self {
        let mut n = Self::base(name, NodeRole::Agent, state);
        n.kind = Some(ExecutorKind::Agent);
        n.detail = Some(model.into());
        n.children = children;
        n
    }

    /// A step node — a unit of work an agent executes (skill / tool / llm).
    fn step(name: &str, kind: ExecutorKind, state: NodeState) -> Self {
        let mut n = Self::base(name, NodeRole::Step, state);
        n.kind = Some(kind);
        n.detail = Some(name.into());
        n
    }

    // ── chainable task-spine annotations ─────────────────────────────────────
    fn critical(mut self) -> Self {
        self.critical = true;
        self
    }
    fn scope(mut self, s: &str) -> Self {
        self.scope = Some(s.into());
        self
    }
    fn parallel_with(mut self, s: &str) -> Self {
        self.parallel_with = Some(s.into());
        self
    }
    fn waiting_on(mut self, w: &[&str]) -> Self {
        self.waits_on = w.iter().map(|s| (*s).to_string()).collect();
        self
    }
    fn needs(mut self, actions: &[&str]) -> Self {
        self.actions = actions.iter().map(|s| (*s).to_string()).collect();
        self
    }

    // ── chainable HITL kind (the typed "your move") ──────────────────────────
    fn ask(mut self, question: &str) -> Self {
        self.hitl = Some(Hitl::Answer {
            question: question.into(),
        });
        self
    }
    #[allow(dead_code)]
    fn approve_gate(mut self) -> Self {
        self.hitl = Some(Hitl::Approve);
        self
    }
    #[allow(dead_code)]
    fn form(mut self, fields: &[&str]) -> Self {
        self.hitl = Some(Hitl::Form {
            fields: fields.iter().map(|s| (*s).to_string()).collect(),
        });
        self
    }
    #[allow(dead_code)]
    fn discuss(mut self, topic: &str) -> Self {
        self.hitl = Some(Hitl::Discuss {
            topic: topic.into(),
        });
        self
    }
    fn thread(mut self, turns: Vec<ChatTurn>) -> Self {
        self.thread = turns;
        self
    }

    /// Append a turn to this node's embedded conversation.
    pub fn push_turn(&mut self, speaker: Speaker, text: String) {
        self.thread.push(ChatTurn { speaker, text });
    }
}

impl MissionView {
    pub fn counts(&self) -> Counts {
        let mut c = Counts {
            running: 0,
            blocked: 0,
            needs_you: 0,
            failed: 0,
        };
        fn walk(nodes: &[TaskNode], c: &mut Counts) {
            for n in nodes {
                match n.state {
                    NodeState::Running => c.running += 1,
                    NodeState::Blocked => c.blocked += 1,
                    NodeState::NeedsYou => c.needs_you += 1,
                    NodeState::Failed => c.failed += 1,
                    _ => {}
                }
                walk(&n.children, c);
            }
        }
        walk(&self.nodes, &mut c);
        c
    }

    /// (node name, first action) for the first node awaiting the human.
    pub fn first_needs_you(&self) -> Option<(String, String)> {
        self.needs_you_items()
            .into_iter()
            .find_map(|(name, actions)| actions.into_iter().next().map(|a| (name, a)))
    }

    /// Every node awaiting the human, with its actions — the sidebar queue.
    pub fn needs_you_items(&self) -> Vec<(String, Vec<String>)> {
        let mut out = Vec::new();
        fn walk(nodes: &[TaskNode], out: &mut Vec<(String, Vec<String>)>) {
            for n in nodes {
                if n.state == NodeState::NeedsYou {
                    out.push((n.name.clone(), n.actions.clone()));
                }
                walk(&n.children, out);
            }
        }
        walk(&self.nodes, &mut out);
        out
    }

    /// Each ask with its **breadcrumb** — the ancestor `task › agent` names —
    /// so the drill-in detail can show where the ask came from rather than
    /// stranding it out of context. Depth-first, same order as `needs_*`.
    pub fn needs_you_with_context(&self) -> Vec<(String, &TaskNode)> {
        let mut out = Vec::new();
        fn walk<'a>(
            nodes: &'a [TaskNode],
            trail: &mut Vec<String>,
            out: &mut Vec<(String, &'a TaskNode)>,
        ) {
            for n in nodes {
                if n.state == NodeState::NeedsYou {
                    out.push((trail.join(" › "), n));
                }
                trail.push(n.name.clone());
                walk(&n.children, trail, out);
                trail.pop();
            }
        }
        let mut trail = Vec::new();
        walk(&self.nodes, &mut trail, &mut out);
        out
    }

    /// Append a turn to the `ask_index`-th ask's embedded conversation.
    pub fn push_ask_turn(&mut self, ask_index: usize, speaker: Speaker, text: String) {
        fn walk(
            nodes: &mut [TaskNode],
            target: usize,
            cur: &mut usize,
            sp: Speaker,
            text: &str,
        ) -> bool {
            for n in nodes {
                if n.state == NodeState::NeedsYou {
                    if *cur == target {
                        n.push_turn(sp, text.to_string());
                        return true;
                    }
                    *cur += 1;
                }
                if walk(&mut n.children, target, cur, sp, text) {
                    return true;
                }
            }
            false
        }
        let mut cur = 0;
        walk(&mut self.nodes, ask_index, &mut cur, speaker, &text);
    }

    /// Resolve the `ask_index`-th ask (depth-first) — clears its HITL state.
    /// In the live cockpit this submits the chosen transition / typed reply;
    /// here it makes the interaction loop visible (the ask leaves the list).
    pub fn resolve_ask(&mut self, ask_index: usize) {
        fn walk(nodes: &mut [TaskNode], target: usize, cur: &mut usize) -> bool {
            for n in nodes {
                if n.state == NodeState::NeedsYou {
                    if *cur == target {
                        n.state = NodeState::Done;
                        n.actions.clear();
                        n.hitl = None;
                        return true;
                    }
                    *cur += 1;
                }
                if walk(&mut n.children, target, cur) {
                    return true;
                }
            }
            false
        }
        let mut cur = 0;
        walk(&mut self.nodes, ask_index, &mut cur);
    }

    /// Count of currently-visible (ancestors-expanded) nodes — the selectable set.
    pub fn selectable_count(&self) -> usize {
        fn walk(nodes: &[TaskNode], acc: &mut usize) {
            for n in nodes {
                *acc += 1;
                if n.expanded {
                    walk(&n.children, acc);
                }
            }
        }
        let mut n = 0;
        walk(&self.nodes, &mut n);
        n
    }

    /// The `index`-th visible node (depth-first, ancestors-expanded), mutably.
    pub fn nth_selectable_mut(&mut self, index: usize) -> Option<&mut TaskNode> {
        fn walk<'a>(
            nodes: &'a mut [TaskNode],
            target: usize,
            cur: &mut usize,
        ) -> Option<&'a mut TaskNode> {
            for n in nodes {
                if *cur == target {
                    return Some(n);
                }
                *cur += 1;
                if n.expanded
                    && let Some(hit) = walk(&mut n.children, target, cur)
                {
                    return Some(hit);
                }
            }
            None
        }
        let mut cur = 0;
        walk(&mut self.nodes, index, &mut cur)
    }

    /// The `index`-th visible node (depth-first, ancestors-expanded), immutably.
    pub fn nth_selectable(&self, index: usize) -> Option<&TaskNode> {
        fn walk<'a>(nodes: &'a [TaskNode], target: usize, cur: &mut usize) -> Option<&'a TaskNode> {
            for n in nodes {
                if *cur == target {
                    return Some(n);
                }
                *cur += 1;
                if n.expanded
                    && let Some(hit) = walk(&n.children, target, cur)
                {
                    return Some(hit);
                }
            }
            None
        }
        let mut cur = 0;
        walk(&self.nodes, index, &mut cur)
    }

    /// A minimal placeholder mission for fixture siblings on the Fleet map.
    pub fn stub(name: &str, orchestrator: &str) -> Self {
        MissionView {
            name: name.into(),
            orchestrator: orchestrator.into(),
            nodes: vec![TaskNode::task("overview", NodeState::Running, vec![])],
        }
    }

    /// A mission tree for the demo / snapshots, organized **task-first** and
    /// grounded in the real CPM plan the cockpit's own planner produced (the
    /// agent→model alignment + caching work). The spine is the deliverables
    /// (D1–D5) with their critical-path / parallel / scope markers; the
    /// agent(s) + steps executing each one nest beneath. Captured mid-flight:
    /// the docs batch is done, D4 is running (a step awaits your review), D1
    /// is blocked upstream, D5 is queued. Starts fully collapsed.
    pub fn demo() -> Self {
        // D2 (done, critical) — docs sync, run by a sub-agent.
        let d2 = TaskNode::task(
            "D2 · SPEC/README doc-sync",
            NodeState::Done,
            vec![TaskNode::agent(
                "docs-agent",
                "sonnet",
                NodeState::Done,
                vec![],
            )],
        )
        .critical()
        .scope("docs/, README.md");

        // D3 (done, ran parallel with D2) — cognitive-architectures, a no-op.
        let mut ca_agent = TaskNode::agent("ca-sync-agent", "sonnet", NodeState::Done, vec![]);
        ca_agent.detail = Some("sonnet · no-op".into());
        let d3 = TaskNode::task(
            "D3 · cognitive-architectures sync",
            NodeState::Done,
            vec![ca_agent],
        )
        .parallel_with("D2")
        .scope("../cognitive-architectures");

        // D4 (running, critical) — the agent untangling delegate:, mid-edit,
        // with a human review gate pending.
        let mut refactor = TaskNode::step(
            "refactor.delegate-untangle",
            ExecutorKind::Llm,
            NodeState::Running,
        );
        refactor.detail = Some("skill · model_resolver".into());
        refactor.harness = vec![Harness {
            name: "structure".into(),
            verdict: Verdict::Warn("god-file".into()),
        }];
        let apply = TaskNode::step("apply-patch", ExecutorKind::Tool, NodeState::Running);
        // Grounded as a real ask_human (SPEC §29): the agent surfaces the
        // genuine decision it hit — interpreter.rs is shared with D5 — and asks
        // rather than guessing. The "discuss" is the embedded thread itself
        // (seeded with a couple of turns); the choices are quick-replies.
        let q = "interpreter.rs is shared with D5 (Delegate→ModelRef). Fold D5 into this task, or keep separate?";
        let review = TaskNode::base("review-edits", NodeRole::Step, NodeState::NeedsYou)
            .ask(q)
            .needs(&["fold D5 in", "keep separate"])
            .thread(vec![
                ChatTurn::agent(q),
                ChatTurn::you("what's the risk if we keep them separate?"),
                ChatTurn::agent(
                    "Both edit interpreter.rs, so as separate batches they can't share a parallel window — and if D5 slips past D4 they race on the file. Folding D5 in removes the conflict.",
                ),
            ]);
        let backend = TaskNode::agent(
            "backend-engineer",
            "coding-frontier",
            NodeState::Running,
            vec![refactor, apply, review],
        );
        let d4 = TaskNode::task("D4 · untangle delegate:", NodeState::Running, vec![backend])
            .critical()
            .scope("core/, tui/interpreter.rs");

        // D1 (blocked) — kind:agent caching, waiting on an upstream aether flag.
        let d1 = TaskNode::task("D1 · kind:agent prompt caching", NodeState::Blocked, vec![])
            .waiting_on(&["aether: prompt_cache_key"]);

        // D5 (pending) — Delegate→ModelRef, queued to fold into D4.
        let d5 = TaskNode::task("D5 · Delegate→ModelRef", NodeState::Pending, vec![])
            .parallel_with("D4");

        MissionView {
            name: "Complete alignment + caching".into(),
            orchestrator: "cognitive/flow.cpm-execute".into(),
            nodes: vec![d2, d3, d4, d1, d5],
        }
    }
}
