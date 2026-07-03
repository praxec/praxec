//! SPEC §21 — FMECA-style atomic assertions for the deterministic
//! interpreter (`walk_workflow`). Uses a `ScriptedMcpCaller` and
//! `ScriptedSpawner` so tests run without spawning real
//! `praxec` or real LLM processes.
//!
//! One behavior per test. The interpreter has small surface but high
//! consequence (it decides whether to escalate, retry, or auto-advance)
//! so each branch gets a dedicated assertion.

use std::collections::HashMap;
use std::sync::Mutex;

use async_trait::async_trait;
use praxec_tui::interpreter::{
    InterpreterError, LegacyAgentRegistry, McpToolCaller, ResolutionError, ResolvedAgent,
    SUB_AGENT_RETRY_BUDGET, SubAgentSpawner, walk_workflow,
};
use serde_json::{Value, json};

// ── test doubles ───────────────────────────────────────────────────────────

/// Scripted MCP backend. Each `expect` call queues a (tool, response)
/// pair; calls are matched in order. Mismatch is a hard failure.
struct ScriptedMcpCaller {
    queue: Mutex<Vec<(String, Value)>>,
    /// Track every call for assertions.
    calls: Mutex<Vec<(String, Value)>>,
}

impl ScriptedMcpCaller {
    fn new() -> Self {
        Self {
            queue: Mutex::new(Vec::new()),
            calls: Mutex::new(Vec::new()),
        }
    }

    fn expect(&self, tool: &str, response: Value) {
        self.queue
            .lock()
            .unwrap()
            .push((tool.to_string(), response));
    }

    fn call_count(&self, tool: &str) -> usize {
        self.calls
            .lock()
            .unwrap()
            .iter()
            .filter(|(t, _)| t == tool)
            .count()
    }

    fn calls_to(&self, tool: &str) -> Vec<Value> {
        self.calls
            .lock()
            .unwrap()
            .iter()
            .filter(|(t, _)| t == tool)
            .map(|(_, args)| args.clone())
            .collect()
    }
}

#[async_trait]
impl McpToolCaller for ScriptedMcpCaller {
    async fn call(&self, tool: &str, args: Value) -> anyhow::Result<Value> {
        self.calls.lock().unwrap().push((tool.to_string(), args));
        let mut queue = self.queue.lock().unwrap();
        if queue.is_empty() {
            anyhow::bail!("ScriptedMcpCaller: unexpected call to '{tool}' (queue empty)");
        }
        let (expected_tool, response) = queue.remove(0);
        assert_eq!(
            expected_tool, tool,
            "ScriptedMcpCaller: queued call was for '{expected_tool}' but got '{tool}'"
        );
        Ok(response)
    }
}

/// Scripted sub-agent spawner. Each `expect_spawn` call queues one
/// outcome (Ok or Err); spawns consume the queue in order.
struct ScriptedSpawner {
    outcomes: Mutex<Vec<Result<(), InterpreterError>>>,
    spawns: Mutex<u32>,
}

impl ScriptedSpawner {
    fn new() -> Self {
        Self {
            outcomes: Mutex::new(Vec::new()),
            spawns: Mutex::new(0),
        }
    }

    fn expect_spawn(&self, outcome: Result<(), InterpreterError>) {
        self.outcomes.lock().unwrap().push(outcome);
    }

    fn spawn_count(&self) -> u32 {
        *self.spawns.lock().unwrap()
    }
}

#[async_trait]
impl SubAgentSpawner for ScriptedSpawner {
    async fn spawn_and_wait(
        &self,
        agent: &ResolvedAgent,
        _system_prompt: &str,
        _workflow_response: &Value,
    ) -> Result<(), InterpreterError> {
        let _ = agent;
        *self.spawns.lock().unwrap() += 1;
        let mut outcomes = self.outcomes.lock().unwrap();
        if outcomes.is_empty() {
            // Test rigging error: a spawn was made without a queued outcome.
            // Surface a distinctive error so the failing test names itself.
            return Err(InterpreterError::SubAgentTimeout {
                agent: "SCRIPT_OUTCOME_QUEUE_EXHAUSTED".into(),
                state: "SCRIPT".into(),
            });
        }
        outcomes.remove(0)
    }
}

// ── fixtures ───────────────────────────────────────────────────────────────

fn agent_registry() -> LegacyAgentRegistry {
    let mut m = HashMap::new();
    m.insert(
        "planner".to_string(),
        praxec_tui::agent_config::AgentConfig {
            name: "planner".into(),
            provider: "anthropic".into(),
            model: "claude-sonnet-4".into(),
        },
    );
    LegacyAgentRegistry::new(m)
}

fn resp_completed() -> Value {
    json!({
        "workflow": { "id": "wf_x", "definitionId": "demo", "state": "done", "version": 5 },
        "result":   { "status": "succeeded" },
        "context":  { "summary": "all good" },
        "links":    []
    })
}

fn resp_at_state(state: &str, version: u64, links: Vec<Value>, delegate: Option<&str>) -> Value {
    let mut body = json!({
        "workflow": { "id": "wf_x", "definitionId": "demo", "state": state, "version": version },
        "result":   { "status": "running" },
        "context":  {},
        "links":    links,
    });
    if let Some(d) = delegate {
        body["delegate"] = Value::String(d.to_string());
    }
    body
}

fn link(rel: &str, args: Value) -> Value {
    json!({ "rel": rel, "method": "praxec.command", "args": args, "actor": "agent" })
}

fn link_deterministic(rel: &str, args: Value) -> Value {
    json!({ "rel": rel, "method": "praxec.command", "args": args, "actor": "deterministic" })
}

fn link_human(rel: &str, args: Value) -> Value {
    json!({ "rel": rel, "method": "praxec.command", "args": args, "actor": "human" })
}

// ── 1. Terminal state — returns context ───────────────────────────────────

#[tokio::test]
async fn terminal_state_returns_context_immediately() {
    let mcp = ScriptedMcpCaller::new();
    mcp.expect("praxec.query", resp_completed());
    let spawner = ScriptedSpawner::new();
    let agents = agent_registry();

    let ctx = walk_workflow(&mcp, &spawner, "wf_x", &agents)
        .await
        .expect("walk completes");
    assert_eq!(ctx, json!({ "summary": "all good" }));
    assert_eq!(mcp.call_count("praxec.query"), 1);
    assert_eq!(mcp.call_count("praxec.command"), 0);
    assert_eq!(spawner.spawn_count(), 0);
}

// ── 2. Single non-deterministic link — auto-submit ────────────────────────

#[tokio::test]
async fn single_actionable_link_auto_submits() {
    let mcp = ScriptedMcpCaller::new();
    mcp.expect(
        "praxec.query",
        resp_at_state("ready", 1, vec![link("go", json!({ "x": 1 }))], None),
    );
    mcp.expect("praxec.command", json!({})); // accepted
    mcp.expect("praxec.query", resp_completed());

    let spawner = ScriptedSpawner::new();
    let agents = agent_registry();
    let _ = walk_workflow(&mcp, &spawner, "wf_x", &agents)
        .await
        .expect("walk completes");
    let submit_calls = mcp.calls_to("praxec.command");
    assert_eq!(submit_calls.len(), 1);
    assert_eq!(submit_calls[0], json!({ "x": 1 }));
}

// ── 3. Deterministic-actor links are filtered out ─────────────────────────

#[tokio::test]
async fn deterministic_links_are_ignored_by_interpreter() {
    // The gateway auto-chains deterministic transitions itself (SPEC §6),
    // so the interpreter MUST skip them. Here we provide one
    // deterministic link + one agent link; the interpreter should pick
    // the agent link.
    let mcp = ScriptedMcpCaller::new();
    mcp.expect(
        "praxec.query",
        resp_at_state(
            "branch",
            1,
            vec![
                link_deterministic("auto", json!({ "auto": true })),
                link("go", json!({ "manual": true })),
            ],
            None,
        ),
    );
    mcp.expect("praxec.command", json!({}));
    mcp.expect("praxec.query", resp_completed());

    let spawner = ScriptedSpawner::new();
    let agents = agent_registry();
    let _ = walk_workflow(&mcp, &spawner, "wf_x", &agents)
        .await
        .expect("walk completes");
    let submit_calls = mcp.calls_to("praxec.command");
    assert_eq!(submit_calls.len(), 1);
    assert_eq!(
        submit_calls[0],
        json!({ "manual": true }),
        "interpreter must skip deterministic link and pick the agent link"
    );
}

// ── 4. Multi-link + escalate present — picks non-escalate ─────────────────

#[tokio::test]
async fn multi_link_with_escalate_picks_non_escalate() {
    let mcp = ScriptedMcpCaller::new();
    mcp.expect(
        "praxec.query",
        resp_at_state(
            "branch",
            1,
            vec![
                link("escalate", json!({ "escalated": true })),
                link("retry", json!({ "retry": true })),
                link("continue", json!({ "continued": true })),
            ],
            None,
        ),
    );
    mcp.expect("praxec.command", json!({}));
    mcp.expect("praxec.query", resp_completed());

    let spawner = ScriptedSpawner::new();
    let agents = agent_registry();
    let _ = walk_workflow(&mcp, &spawner, "wf_x", &agents)
        .await
        .expect("walk completes");
    let submit_calls = mcp.calls_to("praxec.command");
    assert_eq!(submit_calls.len(), 1);
    assert_eq!(
        submit_calls[0],
        json!({ "retry": true }),
        "must pick first non-escalate link, not the first link"
    );
}

// ── 5. Multi-link, no escalate — picks first link (deterministic fallback)─

#[tokio::test]
async fn multi_link_no_escalate_picks_first_link() {
    let mcp = ScriptedMcpCaller::new();
    mcp.expect(
        "praxec.query",
        resp_at_state(
            "branch",
            1,
            vec![
                link("path_a", json!({ "path": "a" })),
                link("path_b", json!({ "path": "b" })),
            ],
            None,
        ),
    );
    mcp.expect("praxec.command", json!({}));
    mcp.expect("praxec.query", resp_completed());

    let spawner = ScriptedSpawner::new();
    let agents = agent_registry();
    let _ = walk_workflow(&mcp, &spawner, "wf_x", &agents)
        .await
        .expect("walk completes");
    let submit_calls = mcp.calls_to("praxec.command");
    assert_eq!(submit_calls[0], json!({ "path": "a" }));
}

// ── 5b. H10 — a human-actor HITL gate is NEVER auto-submitted ──────────────

#[tokio::test]
async fn human_actor_link_is_never_auto_submitted() {
    // A `waiting` state whose only legal move is an `actor: "human"`
    // approval gate. The no-delegate auto-advance path must NOT submit
    // it unattended (that would be a silent auto-approve of a human
    // gate — H10). With no agent-actor link to drive, the interpreter
    // has nothing it may auto-advance and must halt with WorkflowStuck
    // so the gate surfaces for a human.
    let mcp = ScriptedMcpCaller::new();
    mcp.expect(
        "praxec.query",
        resp_at_state(
            "awaiting_approval",
            1,
            vec![link_human("approve", json!({ "approved": true }))],
            None,
        ),
    );

    let spawner = ScriptedSpawner::new();
    let agents = agent_registry();
    let err = walk_workflow(&mcp, &spawner, "wf_x", &agents)
        .await
        .expect_err("a human gate must halt the interpreter, not auto-advance");
    assert!(
        matches!(err, InterpreterError::WorkflowStuck { ref state } if state == "awaiting_approval"),
        "expected WorkflowStuck at the human gate, got: {err:?}"
    );
    // The decisive assertion: the human transition was never submitted.
    assert_eq!(
        mcp.call_count("praxec.command"),
        0,
        "interpreter must not submit a human-actor link unattended"
    );
    assert_eq!(spawner.spawn_count(), 0);
}

#[tokio::test]
async fn human_gate_alongside_agent_link_does_not_get_picked() {
    // When a human gate and an agent link are both legal, the
    // interpreter drives the agent link only — the human gate stays
    // for the human and is never auto-submitted.
    let mcp = ScriptedMcpCaller::new();
    mcp.expect(
        "praxec.query",
        resp_at_state(
            "branch",
            1,
            vec![
                link_human("approve", json!({ "approved": true })),
                link("continue", json!({ "continued": true })),
            ],
            None,
        ),
    );
    mcp.expect("praxec.command", json!({}));
    mcp.expect("praxec.query", resp_completed());

    let spawner = ScriptedSpawner::new();
    let agents = agent_registry();
    let _ = walk_workflow(&mcp, &spawner, "wf_x", &agents)
        .await
        .expect("walk completes by driving the agent link");
    let submit_calls = mcp.calls_to("praxec.command");
    assert_eq!(submit_calls.len(), 1);
    assert_eq!(
        submit_calls[0],
        json!({ "continued": true }),
        "must drive the agent link, never the human gate"
    );
}

// ── 6. ModelRef state without registered agent → UnknownAgent ─────────────

#[tokio::test]
async fn unknown_delegate_agent_surfaces_actionable_error() {
    let mcp = ScriptedMcpCaller::new();
    mcp.expect(
        "praxec.query",
        resp_at_state("planning", 1, vec![], Some("ghost-agent")),
    );
    let spawner = ScriptedSpawner::new();
    let agents = agent_registry(); // contains "planner", NOT "ghost-agent"

    let err = walk_workflow(&mcp, &spawner, "wf_x", &agents)
        .await
        .expect_err("unknown agent must error");
    match err {
        InterpreterError::AgentResolution { state, source } => {
            assert_eq!(state, "planning");
            assert!(
                matches!(source, ResolutionError::UnknownLegacyAgent { delegate } if delegate == "ghost-agent"),
                "expected UnknownLegacyAgent for ghost-agent"
            );
        }
        other => panic!("expected AgentResolution, got: {other:?}"),
    }
}

// ── 7. Sub-agent advances workflow → walk continues ───────────────────────

#[tokio::test]
async fn sub_agent_success_advances_workflow_and_continues() {
    let mcp = ScriptedMcpCaller::new();
    // Initial get → delegate state (version 1).
    mcp.expect(
        "praxec.query",
        resp_at_state("planning", 1, vec![], Some("planner")),
    );
    // After sub-agent returns Ok: interpreter re-fetches to confirm
    // the workflow advanced (version 2 means it did).
    mcp.expect(
        "praxec.query",
        resp_at_state("editing", 2, vec![link("done", json!({}))], None),
    );
    // Loop back to top: interpreter calls praxec.query AGAIN before
    // deciding what to do at the new state.
    mcp.expect(
        "praxec.query",
        resp_at_state("editing", 2, vec![link("done", json!({}))], None),
    );
    // Single-link auto-advance.
    mcp.expect("praxec.command", json!({}));
    mcp.expect("praxec.query", resp_completed());

    let spawner = ScriptedSpawner::new();
    spawner.expect_spawn(Ok(())); // sub-agent claims success
    let agents = agent_registry();
    let _ = walk_workflow(&mcp, &spawner, "wf_x", &agents)
        .await
        .expect("walk completes");
    assert_eq!(spawner.spawn_count(), 1);
}

// ── 8. Sub-agent timeout, budget exhausts, no escalate → propagates ───────

#[tokio::test]
async fn sub_agent_timeout_exhausting_budget_without_escalate_propagates() {
    let mcp = ScriptedMcpCaller::new();
    // The interpreter will retry the sub-agent SUB_AGENT_RETRY_BUDGET times.
    // Each iteration: get → spawn (fails) → repeat. After budget exhausts
    // it re-fetches once more and tries to find an escalate link.
    for _ in 0..SUB_AGENT_RETRY_BUDGET {
        mcp.expect(
            "praxec.query",
            resp_at_state("planning", 1, vec![], Some("planner")),
        );
    }
    // After budget exhaust, the interpreter re-fetches before trying
    // escalate.
    mcp.expect(
        "praxec.query",
        resp_at_state("planning", 1, vec![], Some("planner")),
    );

    let spawner = ScriptedSpawner::new();
    for _ in 0..SUB_AGENT_RETRY_BUDGET {
        spawner.expect_spawn(Err(InterpreterError::SubAgentTimeout {
            agent: "planner".into(),
            state: "planning".into(),
        }));
    }
    let agents = agent_registry();
    let err = walk_workflow(&mcp, &spawner, "wf_x", &agents)
        .await
        .expect_err("budget exhaust with no escalate must propagate");
    assert!(
        matches!(err, InterpreterError::SubAgentTimeout { .. }),
        "expected SubAgentTimeout, got: {err:?}"
    );
}

// ── 9. Sub-agent timeout, budget exhausts, escalate link present → submits ─

#[tokio::test]
async fn sub_agent_timeout_exhausting_budget_with_escalate_submits_escalate() {
    let mcp = ScriptedMcpCaller::new();
    for _ in 0..SUB_AGENT_RETRY_BUDGET {
        mcp.expect(
            "praxec.query",
            resp_at_state(
                "planning",
                1,
                vec![link("escalate", json!({ "esc": true }))],
                Some("planner"),
            ),
        );
    }
    // Re-fetch before escalate.
    mcp.expect(
        "praxec.query",
        resp_at_state(
            "planning",
            1,
            vec![link("escalate", json!({ "esc": true }))],
            Some("planner"),
        ),
    );
    // Escalate submit accepted.
    mcp.expect("praxec.command", json!({}));
    // Next loop iteration sees completed (post-escalate workflow done).
    mcp.expect("praxec.query", resp_completed());

    let spawner = ScriptedSpawner::new();
    for _ in 0..SUB_AGENT_RETRY_BUDGET {
        spawner.expect_spawn(Err(InterpreterError::SubAgentTimeout {
            agent: "planner".into(),
            state: "planning".into(),
        }));
    }
    let agents = agent_registry();
    let _ = walk_workflow(&mcp, &spawner, "wf_x", &agents)
        .await
        .expect("escalate path completes walk");
    let submits = mcp.calls_to("praxec.command");
    assert_eq!(submits.len(), 1);
    assert_eq!(submits[0], json!({ "esc": true }));
}

// ── 10. No delegate, no actionable links → WorkflowStuck ──────────────────

#[tokio::test]
async fn no_delegate_no_links_returns_workflow_stuck() {
    let mcp = ScriptedMcpCaller::new();
    mcp.expect("praxec.query", resp_at_state("stuck", 1, vec![], None));
    let spawner = ScriptedSpawner::new();
    let agents = agent_registry();
    let err = walk_workflow(&mcp, &spawner, "wf_x", &agents)
        .await
        .expect_err("no links + no delegate must error");
    match err {
        InterpreterError::WorkflowStuck { state } => assert_eq!(state, "stuck"),
        other => panic!("expected WorkflowStuck, got: {other:?}"),
    }
}

// ── 11. Gateway submit rejection surfaces as SubmitRejected ───────────────

#[tokio::test]
async fn gateway_submit_rejection_surfaces_as_submit_rejected() {
    let mcp = ScriptedMcpCaller::new();
    mcp.expect(
        "praxec.query",
        resp_at_state("ready", 1, vec![link("go", json!({}))], None),
    );
    // Gateway returns body-level error (INVALID_TRANSITION-style).
    mcp.expect(
        "praxec.command",
        json!({
            "error": { "code": "INVALID_TRANSITION", "message": "no such txn" }
        }),
    );

    let spawner = ScriptedSpawner::new();
    let agents = agent_registry();
    let err = walk_workflow(&mcp, &spawner, "wf_x", &agents)
        .await
        .expect_err("body-level error must surface as SubmitRejected");
    // CMP-040: the reported state must be the real pre-submit state
    // ("ready"), not the "?" placeholder that resulted from deriving
    // state off the link object (which carries no `/workflow/state`).
    match err {
        InterpreterError::SubmitRejected { state, reason } => {
            assert_eq!(state, "ready", "SubmitRejected must carry the real state");
            assert_eq!(reason, "no such txn");
        }
        other => panic!("expected SubmitRejected, got: {other:?}"),
    }
}

// ── STUB-109: missing version fails loud, not silent 0 ────────────────────

#[tokio::test]
async fn missing_workflow_version_surfaces_malformed_response() {
    let mcp = ScriptedMcpCaller::new();
    // Running (not completed) but `/workflow/version` is absent — schema
    // drift. The old code defaulted to 0, corrupting advance detection; now
    // it must surface as a MalformedResponse.
    mcp.expect(
        "praxec.query",
        json!({
            "workflow": { "id": "wf_x", "definitionId": "demo", "state": "ready" },
            "result":   { "status": "running" },
            "context":  {},
            "links":    [],
        }),
    );

    let spawner = ScriptedSpawner::new();
    let agents = agent_registry();
    let err = walk_workflow(&mcp, &spawner, "wf_x", &agents)
        .await
        .expect_err("missing version must fail loud");
    assert!(
        matches!(err, InterpreterError::MalformedResponse { .. }),
        "expected MalformedResponse, got: {err:?}"
    );
}
