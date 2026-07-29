# Entry Gate (Fallible Render) Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Stop an agent step from being dispatched with a goal/prompt that silently contains an unresolved `(x: unset)` template stub — the failure that let a surface-name (`"organization-payment"`) reach a coder where real values were expected.

**Architecture:** Add a *stub-tracking* render alongside the existing infallible `render_template` (non-breaking, additive). At the agent-dispatch site, use it to detect unresolved tokens in the rendered goal. Ship enforcement behind a config flag defaulting to **shadow mode** (emit a typed audit anomaly, do NOT block) so the blast radius across the diverse workflow fleet is measured before any flip to fail-fast. This is Plan A of the L1 "evidence-gated boundaries" design (`docs/design/2026-07-28-change-building-block-design.md`); the continuation delta-gate and the full fallback-ledger are separate plans.

**Tech Stack:** Rust (workspace crates `praxec-core`, `praxec-agents`); existing `AuditSink` for observability; `serde_json`.

## Global Constraints

- **Non-breaking:** `render_template`'s public signature and behavior MUST NOT change (it has ~10 call sites across `praxec-llm-executor`, `praxec-agents`, `praxec-core`, `praxec-executors`). Add a new function; do not alter the old one's contract.
- **Fail-fast with diagnostics:** an enforced refusal must be a typed error naming the exact unresolved path(s) and the workflow/transition — never a generic message.
- **Shadow-mode default:** enforcement is OFF by default; the gate emits an audit anomaly and proceeds. Enforcement is opt-in via one gateway config flag.
- **Reuse existing spine:** use the existing `AuditSink` and `permanent(...)`/`AgentErrorCode` conventions in `praxec-agents`; do not introduce a parallel error/telemetry path.
- **One cargo invocation at a time** (they serialize on the target lock).

---

### Task 1: Stub-tracking render in `praxec-core::templating`

**Files:**
- Modify: `crates/praxec-core/src/templating.rs` (add `render_template_tracked` + refactor `resolve_template_path` internals)
- Test: `crates/praxec-core/src/templating.rs` (inline `#[cfg(test)]`)

**Interfaces:**
- Consumes: `WorkflowInstance` (existing).
- Produces: `pub fn render_template_tracked(template: &str, instance: &WorkflowInstance) -> (String, Vec<String>)` — element 0 is byte-identical to `render_template(template, instance)`; element 1 is the de-duplicated list of `$.`-paths that stubbed (resolved to `(…: unset)`). Also `render_template` is re-expressed as `render_template_tracked(..).0` so the two can never drift.

- [ ] **Step 1: Write the failing test**

Add to the existing test module in `templating.rs`:

```rust
#[test]
fn tracked_render_reports_unresolved_paths_and_matches_render_template() {
    let inst = crate::model::WorkflowInstance::for_test_with_context(
        serde_json::json!({ "present": "ok" }),
    );
    let tmpl = "A={{ $.context.present }} B={{ $.context.missing }} C={{ $.context.also_missing }}";
    let (rendered, unresolved) = render_template_tracked(tmpl, &inst);
    // element 0 is byte-identical to the infallible renderer
    assert_eq!(rendered, render_template(tmpl, &inst));
    // both unresolved paths are reported, de-duplicated, in encounter order
    assert_eq!(unresolved, vec!["$.context.missing".to_string(), "$.context.also_missing".to_string()]);
    // a fully-resolved template reports nothing
    let (_r, none) = render_template_tracked("A={{ $.context.present }}", &inst);
    assert!(none.is_empty());
}
```

> If `WorkflowInstance::for_test_with_context` does not exist, first add a minimal test constructor in `model.rs` (`#[cfg(test)] pub fn for_test_with_context(context: serde_json::Value) -> Self`) that fills `context`, empty `input`, a stub id/state/version, and `RunEnv::for_test()`. Fold that into this task; do not create a separate task.

- [ ] **Step 2: Run test to verify it fails**

Run: `cargo test -p praxec-core --lib templating::tests::tracked_render_reports_unresolved 2>&1 | tail -20`
Expected: FAIL — `render_template_tracked` not found.

- [ ] **Step 3: Refactor `resolve_template_path` to report resolution, then add the tracked renderer**

Introduce a private helper that returns whether a path resolved, keep `resolve_template_path` as its stubbing wrapper, and add the tracked scan:

```rust
/// `Ok(value)` when the path resolved; `Err(stub)` carries the `(last: unset)`
/// string the infallible renderer emits. Single source of truth for both.
fn resolve_or_stub(path: &str, instance: &WorkflowInstance) -> Result<String, String> {
    let s = resolve_template_path(path, instance);
    // A resolved value can legitimately look like anything EXCEPT the reserved
    // stub shape `(<segment>: unset)`; that shape is only produced on the
    // unresolved branches of resolve_template_path.
    let last = path.rsplit('.').next().unwrap_or(path);
    if s == format!("({last}: unset)") { Err(s) } else { Ok(s) }
}

/// Render, and collect the `$.`-paths that stubbed. Element 0 is byte-identical
/// to `render_template`. See the entry gate (Plan A) for the consumer.
pub fn render_template_tracked(template: &str, instance: &WorkflowInstance) -> (String, Vec<String>) {
    let mut output = String::with_capacity(template.len());
    let mut unresolved: Vec<String> = Vec::new();
    let mut remaining = template;
    while let Some(start) = remaining.find("{{") {
        output.push_str(&remaining[..start]);
        let after_open = &remaining[start + 2..];
        let Some(end_rel) = after_open.find("}}") else {
            output.push_str(&remaining[start..]);
            return (output, unresolved);
        };
        let inner = after_open[..end_rel].trim();
        if inner.is_empty() {
            output.push_str("{{}}");
        } else {
            match resolve_or_stub(inner, instance) {
                Ok(v) => output.push_str(&v),
                Err(stub) => {
                    output.push_str(&stub);
                    let p = inner.to_string();
                    if !unresolved.contains(&p) { unresolved.push(p); }
                }
            }
        }
        remaining = &after_open[end_rel + 2..];
    }
    output.push_str(remaining);
    (output, unresolved)
}
```

Then make `render_template` delegate so they can never drift:

```rust
pub fn render_template(template: &str, instance: &WorkflowInstance) -> String {
    render_template_tracked(template, instance).0
}
```

- [ ] **Step 4: Run the tests to verify they pass**

Run: `cargo test -p praxec-core --lib templating 2>&1 | tail -20`
Expected: PASS — the new test plus all existing `templating` tests (the delegation must not change any existing rendered output).

- [ ] **Step 5: Commit**

```bash
git add crates/praxec-core/src/templating.rs crates/praxec-core/src/model.rs
git commit -m "feat(templating): render_template_tracked reports unresolved template paths"
```

---

### Task 2: Typed anomaly event for an unresolved-input dispatch

**Files:**
- Modify: `crates/praxec-agents/src/error.rs` (add `AgentErrorCode::InputUnresolved`)
- Test: `crates/praxec-agents/src/error.rs` (inline tests)

**Interfaces:**
- Produces: `AgentErrorCode::InputUnresolved` with wire code `AGENT_INPUT_UNRESOLVED`, surfaced via `permanent(AgentErrorCode::InputUnresolved, ctx)`. Consumed by Task 3.

- [ ] **Step 1: Write the failing test**

```rust
#[test]
fn input_unresolved_wire_code_is_stable() {
    assert_eq!(
        AgentErrorCode::InputUnresolved.as_wire_code(),
        "AGENT_INPUT_UNRESOLVED"
    );
}
```

- [ ] **Step 2: Run test to verify it fails**

Run: `cargo test -p praxec-agents --lib error::tests::input_unresolved 2>&1 | tail -12`
Expected: FAIL — variant not found.

- [ ] **Step 3: Add the variant + wire code**

In the `AgentErrorCode` enum add (with a doc comment matching the file's style):

```rust
    /// Entry gate — a `required` input rendered to an unresolved `(x: unset)`
    /// stub, so the agent would have been dispatched on non-truth (the
    /// surface-name-as-path class). Classifies `ContentOther` (an author/data
    /// error that must surface to a human, NOT a model-capability failure — do
    /// not chain-escalate it to another model).
    InputUnresolved,
```

In `as_wire_code`'s match add:

```rust
            AgentErrorCode::InputUnresolved => "AGENT_INPUT_UNRESOLVED",
```

- [ ] **Step 4: Run test to verify it passes**

Run: `cargo test -p praxec-agents --lib error:: 2>&1 | tail -12`
Expected: PASS (all error-code tests).

- [ ] **Step 5: Commit**

```bash
git add crates/praxec-agents/src/error.rs
git commit -m "feat(agents): AGENT_INPUT_UNRESOLVED entry-gate error code"
```

---

### Task 3: Entry gate at the agent goal render — shadow mode

**Files:**
- Modify: `crates/praxec-agents/src/executor.rs` (around the `render_template(&cfg.goal, …)` call at ~line 724, inside the governed `execute` path)
- Test: `crates/praxec-agents/src/executor.rs` (inline `#[cfg(test)]`, reuse the existing `MockSessionRunner`/`exec_with` harness)

**Interfaces:**
- Consumes: `render_template_tracked` (Task 1), `AgentErrorCode::InputUnresolved` (Task 2), the executor's existing `AuditSink` (`self.audit`, `Option<Arc<dyn AuditSink>>`).
- Produces: an `agent.input_unresolved` audit event on every dispatch whose rendered goal has ≥1 unresolved path; in shadow mode the dispatch still proceeds.

- [ ] **Step 1: Write the failing test**

```rust
#[tokio::test]
async fn entry_gate_shadow_emits_anomaly_but_does_not_block() {
    let audit = praxec_core::audit::MemoryAuditSink::new();
    let exec = AgentExecutor::new(
        Arc::new(MockSessionRunner::completed(AgentResult {
            status: AgentStatus::Success,
            output: json!({ "verdict": "pass" }),
            internal_monologue: None,
        })),
        Arc::new(MockModelResolver("anthropic:claude-sonnet-4-6".into())),
    )
    .with_audit_sink(Arc::new(audit.clone()));
    // goal references a context key that is NOT present → renders "(missing: unset)"
    let res = exec
        .execute(request(json!({ "affinity": "coding", "goal": "do {{ $.context.missing }}" }), bare_def()))
        .await;
    // shadow mode: the run still succeeds (does NOT block)
    assert!(res.is_ok(), "shadow mode must not block: {res:?}");
    // but an anomaly was recorded, naming the unresolved path
    let anomalies: Vec<_> = audit.snapshot().into_iter()
        .filter(|e| e.event_type == "agent.input_unresolved").collect();
    assert_eq!(anomalies.len(), 1);
    assert!(anomalies[0].payload["unresolved"].to_string().contains("$.context.missing"));
    assert_eq!(anomalies[0].payload["enforced"], json!(false));
}
```

- [ ] **Step 2: Run test to verify it fails**

Run: `cargo test -p praxec-agents --lib entry_gate_shadow 2>&1 | tail -20`
Expected: FAIL — no such event emitted.

- [ ] **Step 3: Implement the shadow-mode gate**

At the goal-render site, replace `let user_prompt = render_template(&cfg.goal, &request.workflow);` with a tracked render + anomaly emission (proceed regardless in shadow mode):

```rust
let (user_prompt, unresolved) = praxec_core::templating::render_template_tracked(
    &cfg.goal, &request.workflow,
);
if !unresolved.is_empty() {
    if let Some(sink) = &self.audit {
        let event = praxec_core::audit::AuditEvent::new("agent.input_unresolved")
            .with_correlation(request.correlation_id.clone().unwrap_or_default())
            .with_payload(json!({
                "workflow_id": request.workflow.id,
                "transition": request.transition,
                "unresolved": unresolved,
                "enforced": false,
            }));
        let _ = sink.record(event).await;
    }
    // Shadow mode: proceed. Enforcement is Task 4.
}
```

> Match the exact `AuditEvent` builder methods used elsewhere in `executor.rs` (`emit_model_attempt` is the reference — copy its `.new(...).with_correlation(...).with_payload(...)` shape and its `.await` on `sink.record`).

- [ ] **Step 4: Run the test to verify it passes**

Run: `cargo test -p praxec-agents --lib entry_gate_shadow 2>&1 | tail -20`
Expected: PASS.

- [ ] **Step 5: Commit**

```bash
git add crates/praxec-agents/src/executor.rs
git commit -m "feat(agents): entry gate emits agent.input_unresolved in shadow mode"
```

---

### Task 4: Enforcement mode behind a gateway flag

**Files:**
- Modify: `crates/praxec-agents/src/config.rs` (add `enforce_input_grounding: bool` to `AgentExecutorConfig`, `#[serde(default)]`)
- Modify: `crates/praxec-agents/src/executor.rs` (branch on the flag; refuse when enforced)
- Test: `crates/praxec-agents/src/executor.rs` (inline)

**Interfaces:**
- Consumes: Task 3's gate.
- Produces: when `enforce_input_grounding == true` and `unresolved` is non-empty, `execute` returns `Err(permanent(AgentErrorCode::InputUnresolved, …))` naming the paths — before any model dispatch. Default `false` (shadow).

- [ ] **Step 1: Write the failing test**

```rust
#[tokio::test]
async fn entry_gate_enforced_refuses_before_dispatch() {
    let runner = Arc::new(MockSessionRunner::completed(AgentResult {
        status: AgentStatus::Success, output: json!({}), internal_monologue: None,
    }));
    let exec = exec_with_runner(runner.clone());
    let err = exec.execute(request(
        json!({ "affinity": "coding", "goal": "do {{ $.context.missing }}",
                "enforce_input_grounding": true }),
        bare_def(),
    )).await.expect_err("enforced gate must refuse");
    assert!(format!("{err:?}").contains("AGENT_INPUT_UNRESOLVED"));
    assert!(format!("{err:?}").contains("$.context.missing"));
    // the runner was NEVER called — refusal is pre-dispatch
    assert_eq!(runner.run_count(), 0);
}
```

> If `MockSessionRunner` lacks `run_count()`, add a `#[cfg(any(test, feature = "test-util"))]` atomic counter incremented in its `run(..)` and a `run_count()` accessor. Fold into this task.

- [ ] **Step 2: Run test to verify it fails**

Run: `cargo test -p praxec-agents --lib entry_gate_enforced 2>&1 | tail -20`
Expected: FAIL — currently proceeds (shadow only).

- [ ] **Step 3: Add the flag and the enforced branch**

In `config.rs`:

```rust
    /// Entry gate (Plan A). When true, a step whose rendered goal contains an
    /// unresolved `(x: unset)` template stub is REFUSED before dispatch
    /// (AGENT_INPUT_UNRESOLVED). Default false = shadow mode (emit anomaly,
    /// proceed). Flip per-step, or gateway-wide via the auto-drive composer.
    #[serde(default)]
    pub enforce_input_grounding: bool,
```

In `executor.rs`, after emitting the anomaly, set `"enforced": cfg.enforce_input_grounding` in the payload, and add:

```rust
    if cfg.enforce_input_grounding {
        return Err(permanent(
            AgentErrorCode::InputUnresolved,
            format!(
                "goal for transition {:?} of workflow '{}' has unresolved input path(s) {:?} \
                 (rendered as `(…: unset)` stubs) — refusing to dispatch on non-truth",
                request.transition, request.workflow.id, unresolved
            ),
        ));
    }
```

- [ ] **Step 4: Run the tests to verify they pass**

Run: `cargo test -p praxec-agents --lib entry_gate 2>&1 | tail -20`
Expected: PASS (both shadow and enforced tests).

- [ ] **Step 5: Commit**

```bash
git add crates/praxec-agents/src/config.rs crates/praxec-agents/src/executor.rs
git commit -m "feat(agents): enforce_input_grounding flips the entry gate from shadow to fail-fast"
```

---

### Task 5: Classify `AGENT_INPUT_UNRESOLVED` as ContentOther (surface, don't escalate)

**Files:**
- Modify: `crates/praxec-core/src/model_resolver/classify.rs` (the `from_executor_error` match)
- Test: `crates/praxec-core/src/model_resolver/classify.rs` (inline)

**Interfaces:**
- Consumes: the `AGENT_INPUT_UNRESOLVED` wire prefix (Task 2).
- Produces: `FailureClass::ContentOther` for that prefix (an author/data error surfaces to a human; it must NOT chain-escalate to another model — a different model can't fix an unresolved binding).

- [ ] **Step 1: Write the failing test**

```rust
#[test]
fn input_unresolved_surfaces_not_escalates() {
    let err = ExecutorError::Permanent("AGENT_INPUT_UNRESOLVED: goal has unresolved path".into());
    let class = FailureClass::from_executor_error(&err);
    assert_eq!(class, FailureClass::ContentOther);
    assert!(!class.is_infrastructure(), "must not chain-escalate an unresolved input");
}
```

- [ ] **Step 2: Run test to verify it fails**

Run: `cargo test -p praxec-core --lib classify::tests::input_unresolved 2>&1 | tail -12`
Expected: PASS *by default fall-through*? Confirm: `from_executor_error` sends unknown `Permanent` prefixes to `ContentOther` already (the `_ => FailureClass::ContentOther` arm). If the test PASSES with no code change, this task is a **pinning test only** — keep it (it locks the behavior so a future edit to the Capability arm can't accidentally sweep this prefix in). If it FAILS, add nothing to the Capability arm; the default already covers it.

- [ ] **Step 3: (only if Step 2 failed) do not add to the Capability arm**

No production change needed — the default arm is correct. This task exists to *pin* that `AGENT_INPUT_UNRESOLVED` never joins the escalatable prefixes (`AGENT_NO_RESULT`/`NOT_CONVERGING`/`RESULT_FAILED`/`NO_FILE_WRITES`).

- [ ] **Step 4: Run the test to verify it passes**

Run: `cargo test -p praxec-core --lib classify 2>&1 | tail -12`
Expected: PASS.

- [ ] **Step 5: Commit**

```bash
git add crates/praxec-core/src/model_resolver/classify.rs
git commit -m "test(classify): pin AGENT_INPUT_UNRESOLVED as ContentOther (surface, not escalate)"
```

---

## Self-Review

- **Spec coverage:** Plan A covers the *entry gate* only (design §2 entry gate + §9 SHIP "fallible render + non-empty-consume, required-only, shadow-mode first"). Explicitly OUT of scope here (separate plans): the continuation delta-gate (design §2), the full fallback-ledger/cost-report anomaly column (design §8), the admissibility/RunCommand validator, the acceptance-criteria `outcomes` extension (design §3.0), and the entire L3 apply-strategy tool.
- **"required-only":** this plan gates the **goal** render at the agent dispatch — the single highest-value, incident-matching site. Extending the gate to every `required` inputSchema binding (vs `$optional`) is a follow-on within the continuation/handoff plan, where the `required` vs `$optional` distinction is already threaded (`runtime_chain.rs` builds `required_keys` from `/inputSchema/required`).
- **Shadow-mode / blast radius (FMECA #12):** default `enforce_input_grounding = false`; the gate only *emits* until an operator flips the flag after reading the `agent.input_unresolved` rate. Rollback = the flag.
- **Placeholder scan:** none — every step has concrete code or an explicit "pinning only" note (Task 5).
- **Type consistency:** `render_template_tracked -> (String, Vec<String>)` (Task 1) is consumed with that exact shape in Task 3; `AgentErrorCode::InputUnresolved` / `AGENT_INPUT_UNRESOLVED` consistent across Tasks 2/4/5.

## Notes for the implementer
- Confirm the exact `AuditEvent` builder API by copying `emit_model_attempt` in `executor.rs` (it is the in-file reference for event shape + `sink.record(..).await`).
- Do not change `render_template`'s output for any existing template — Task 1 Step 4 guards this by re-running all `templating` tests after the delegation refactor.
