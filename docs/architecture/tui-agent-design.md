# WIP: TUI Agent Runtime — Deterministic Interpreter + Sub-Agent Orchestration

**Date:** 2026-05-25
**Status:** FMECA-validated plan — v1 scope reduced to minimal justifiable components.

## Summary

Add a `crates/praxec-tui/` crate that wraps the Aether agent
framework (TUI, model calling, session management), routes ALL model
tool calls through the existing `praxec` MCP server as its sole
tool surface, and adds a **deterministic graph-walking interpreter**
that eliminates flow-LLM context accumulation by spawning
isolated sub-agent sessions per workflow state.

### v1 scope (after FMECA reduction)

| Component | Classification | v1? |
|---|---|---|
| `delegate` on Praxec states (pass-through string) | Essential | **Yes** |
| Deterministic interpreter (merged core + error handling) | Essential | **Yes** |
| Sub-agent spawner (Aether session per delegate) | Essential | **Yes** |
| Timeout poka-yoke (required, no defaults) | Essential | **Yes** |
| Example `swe-agent.yaml` + skills | Essential | **Yes** |
| Agent config (CLI args) | Useful | **Yes** |
| Thin LLM branch picker | Useful | **Deferred** — deterministic fallback for v1 |
| `model` hints on states/transitions | Speculative | **Removed** — operators pick models in agent configs |
| `challenge` field convention | Useful | **Deferred** — critic already has blackboard values to compare |
| `replan` transition in example | Useful | **Deferred** — users add when needed; example shows retry-only |

### What was removed and why

- **`model` hints (`cheap`/`medium`/`frontier`):** Speculative. No evidence
  that workflow authors will correctly choose tiers, or that 3 tiers is the
  right granularity. Operators configure model per agent config instead.
- **Separate `walk_one_step` + `walk_workflow` functions:** Cosmetic
  separation. Merged into one function with error handling inline.
- **Thin LLM branch picker:** Unvalidated mechanism. When guards don't
  resolve to a single path, v1 picks the first non-escalation link. The
  critic + retry cycle corrects wrong picks. Thin LLM is a v2 optimization.
- **`challenge` field convention:** Critic already sees `originalIssue`
  and `normalizedProblem` on the blackboard. Good critic guidance covers
  this. Formal `challenge` field is v2.
- **`replan` transition in example:** Users add it when needed. v1 example
  shows `retry → editing` and `escalate → human_review` only.
- **Agent config TOML files:** CLI args (`--agent planning=anthropic/claude-sonnet`)
  for v1. Less ergonomic but lower complexity. TOML files are v2.

---

## Architecture

```
praxec-agent (TUI binary)
  │
  ├─ Aether framework (TUI, model calling, session management)
  │
  ├─ Deterministic interpreter (one function, ~100 lines):
  │     loop { get_state → delegate? spawn_sub_agent : single_link? auto_advance : first_link }
  │
  ├─ Sub-agent spawner (wires agent configs to delegate states)
  │
  └─ Sole MCP server: praxec (child process, 2 stable tools)
       │  praxec.query  (home / search / describe / get / explain)
       │  praxec.command (start / submit / define)
       │
       └─ Executor proxying via connections:
            ├─ kind: cli → shell, lint, build, constrained-edit
            ├─ kind: mcp → scip-mcp, verifier-harness-mcp, structureos
            ├─ kind: rest → external APIs
            ├─ kind: workflow → nested governed workflows
            └─ kind: human → approval gates
```

---

## Implementation Plan

### Phase 1: Praxec schema additions (pass-through, no logic)

Praxec stays model-agnostic. It carries one new field that the TUI
interpreter consumes.

#### 1.1 Config schema: `delegate` on states

**File:** `schemas/gateway-config.schema.json`

Add optional `delegate` (string) property to the state schema under
`$defs/state/properties`.

```json
"delegate": {
  "type": "string",
  "description": "Agent config name for sub-agent delegation. Surfaced verbatim on the response; the TUI interpreter spawns a sub-agent session using this name."
}
```

#### 1.2 Response schema: `delegate` on response

**File:** `schemas/workflow-response.schema.json`

Add optional `delegate` (string) to the top-level response properties.

#### 1.3 Runtime: surface `delegate` on response

**File:** `crates/praxec-core/src/runtime_response.rs`

In `response()`, read `delegate` from the current state definition and
surface it at the top level of the response body. Pure pass-through.

#### 1.4 Config loading: accept `delegate` on states

**File:** `crates/praxec-core/src/config.rs`

Accept `delegate` as a valid state-level key. Reject empty or non-string
values with `INVALID_DELEGATE`.

#### 1.5 `check`: validate delegate references (soft)

**File:** `crates/praxec-core/src/validate.rs`

Warn when a `delegate` references a name not in any known registry.
Soft check — Praxec can't verify agent configs exist in the TUI.

#### 1.6 Config additions & error codes

**File:** `docs/reference/spec.md §13`

| Key | Location | Notes |
|---|---|---|
| `delegate` | workflow state | optional string — agent config name for sub-agent delegation |

| Code | When |
|---|---|
| `INVALID_DELEGATE` | `delegate` is present but empty or not a string |

#### 1.7 docs/reference/spec.md §21 (new section)

Document `delegate` semantics: pass-through field, surfaced on response,
consumed by TUI interpreter. The sub-agent inherits guidance, blackboard,
and the 2 Praxec tools.

### Phase 2: TUI interpreter — `crates/praxec-tui/`

The crate already exists as `praxec-agent` with Aether deps and
Praxec MCP wiring. Phase 2 adds the interpreter.

#### 2.1 Deterministic interpreter (`src/interpreter.rs`)

```rust
/// Walk a workflow to completion. One function — no wrapper/core split.
async fn walk_workflow(runtime: &mut TuiRuntime, workflow_id: &str) -> Result<Value> {
    let mut retries = 0;
    loop {
        let resp = runtime.mcp.call("praxec.query", json!({"workflowId": workflow_id})).await?;

        // Terminal state
        if resp["result"]["status"] == "completed" {
            return Ok(resp["context"].clone());
        }

        // Sub-agent delegation
        if let Some(agent_name) = resp["delegate"].as_str() {
            match runtime.spawn_and_wait(agent_name, &resp).await {
                Ok(()) => { retries = 0; continue; }
                Err(InterpreterError::SubAgentTimeout) if retries < 3 => {
                    retries += 1;
                    runtime.escalate_model_tier().await?;
                    continue;
                }
                Err(InterpreterError::SubAgentTimeout) => {
                    runtime.submit_deterministic_transition(workflow_id, "escalate").await?;
                    continue;
                }
                Err(e) => return Err(e.into()),
            }
        }

        // Deterministic advancement: single actionable link → auto-advance
        let links = resp["links"].as_array().unwrap_or(&vec![]);
        let actionable: Vec<_> = links.iter()
            .filter(|l| l["actor"].as_str() != Some("deterministic"))
            .collect();

        if actionable.len() == 1 {
            runtime.mcp.call("praxec.command", actionable[0]["args"].clone()).await?;
            retries = 0;
            continue;
        }

        // Multiple links, guards can't resolve → deterministic fallback:
        // pick the first non-escalation link. The critic cycle corrects
        // wrong picks on the next iteration.
        let first = actionable.iter()
            .find(|l| l["rel"].as_str() != Some("escalate"))
            .unwrap_or(&actionable[0]);
        runtime.mcp.call("praxec.command", first["args"].clone()).await?;
        retries = 0;
        continue;
    }
}
```

#### 2.2 Sub-agent spawner (`src/sub_agent.rs`)

```rust
/// Spawn an isolated sub-agent session, wait for completion.
/// The sub-agent gets: guidance from the state, full blackboard,
/// same 2 Praxec tools. Runs until it calls praxec.command (submit) or
/// hits timeout/step limit.
async fn spawn_and_wait(&mut self, agent_name: &str, praxec_resp: &Value) -> Result<()> {
    let agent_config = self.agent_configs.get(agent_name)
        .ok_or(InterpreterError::UnknownAgent(agent_name.to_string()))?;

    // Build system prompt from guidance
    let guidance = &praxec_resp["guidance"];
    let system = format!(
        "{}\n\nGoal: {}\n\nInstructions: {}\n\nAvailable blackboard: {}",
        agent_config.system_prompt,
        guidance["goal"].as_str().unwrap_or(""),
        guidance["instructions"].as_str().unwrap_or(""),
        praxec_resp["context"]
    );

    // Warn if blackboard is large but pass all slots — timeout catches overload
    let ctx_size = praxec_resp["context"].to_string().len();
    if ctx_size > self.config.max_blackboard_bytes {
        tracing::warn!(agent = agent_name, size = ctx_size, "large blackboard for sub-agent");
    }

    // Spawn fresh session with scoped context
    let session = aether_cli::session::Session::new()
        .with_model(&agent_config.model)
        .with_provider(&agent_config.provider)
        .with_system_prompt(&system)
        .with_tools(self.mcp_tools.clone())  // same 2 Praxec tools
        .with_max_steps(self.config.max_sub_agent_steps)
        .with_timeout(self.config.max_sub_agent_seconds);

    session.run_until_submitted().await?;
    Ok(())
}
```

#### 2.3 Agent config (`src/agent_config.rs`)

v1 uses CLI args. The TUI binary accepts:

```
praxec-agent \
  --agent planning=anthropic/claude-sonnet-4 \
  --agent editing=openrouter/qwen-2.5-coder-7b \
  --agent retrieval=openrouter/qwen-2.5-coder-7b \
  --agent critique=anthropic/claude-opus-4
```

Format: `name=provider/model`. The TUI resolves `delegate: planning-agent`
against these. Default provider/model when no match: the first agent
configured, or error if none.

TOML files (`~/.praxec/agents/*.toml`) are a v2 UX improvement.

#### 2.4 TUI configuration (`src/config.rs`)

```rust
struct TuiConfig {
    /// Maximum seconds a sub-agent can run before timeout.
    /// NO DEFAULT — startup rejects missing value.
    max_sub_agent_seconds: u64,

    /// Maximum tool calls a sub-agent can make before forced stop.
    /// NO DEFAULT — startup rejects missing value.
    max_sub_agent_steps: usize,

    /// Warn-level threshold for sub-agent blackboard size (bytes).
    max_blackboard_bytes: usize,  // default: 16384
}
```

Poka-yoke: `max_sub_agent_seconds` and `max_sub_agent_steps` have NO
defaults. The TUI refuses to start without them — error message names
both fields and explains why they must be set.

### Phase 3: Example workflow — `examples/swe-agent.yaml`

A complete governed coding-agent workflow. v1 uses retry + escalate
only (no replan — users add it when they need it).

```yaml
version: "1.0.0"

skills:
  plan.specify.change-request:
    verb: plan
    lifecycle: stable
    body: |
      Normalise the incoming issue into a typed problem statement.
      Write to normalizedProblem, acceptanceCriteria, and risk (low|medium|high|critical).
      The critic will compare your normalization against the original issue — be precise.

  diagnose.codebase.search:
    verb: diagnose
    lifecycle: stable
    body: |
      Compose precise graph queries against the codebase. Return an evidence pack:
      target files, symbols, test targets, and CODEOWNERS.

  implement.edit.constrained:
    verb: implement
    lifecycle: stable
    body: |
      Produce only structured edit operations. Never propose raw shell commands.
      Verify your edits satisfy the acceptance criteria from the plan.

  review.code.adversarial:
    verb: review
    lifecycle: stable
    body: |
      Attack the candidate patch:
      1. Compare originalIssue against normalizedProblem — did the planner get it right?
      2. Compare normalizedProblem against candidateDiff — does the fix match the plan?
      3. Check verifierResult for regressions or untested paths.
      Choose: accept (all clear), retry (edit is wrong), or escalate (plan is wrong or retries exhausted).

workflows:
  swe_agent:
    version: "2026-05-25"
    skills: [plan.specify.change-request, review.code.adversarial]
    blackboard:
      originalIssue: { type: string }
      normalizedProblem: { type: string }
      acceptanceCriteria: { type: array }
      risk: { type: string, enum: [low, medium, high, critical] }
      evidencePack: { type: array }
      candidateDiff: { type: string }
      verifierResult: { type: object }
      critique: { type: object }
      retryCount: { type: integer }
      summary: { type: string }

    states:
      planning:
        delegate: planning-agent
        goal: Normalise the change request
        skills: [plan.specify.change-request]
        output:
          normalizedProblem: {}
          acceptanceCriteria: {}
          risk: {}
        transitions:
          plan_ready:
            target: retrieving
            inputSchema:
              required: [normalizedProblem, acceptanceCriteria, risk]

      retrieving:
        delegate: retrieval-agent
        goal: Assemble evidence pack
        skills: [diagnose.codebase.search]
        output:
          evidencePack: {}
        transitions:
          evidence_ready:
            target: editing
            inputSchema:
              required: [evidencePack]

      editing:
        delegate: editing-agent
        goal: Produce constrained edits
        skills: [implement.edit.constrained]
        output:
          candidateDiff: {}
        transitions:
          edits_produced:
            target: verifying
            inputSchema:
              required: [candidateDiff]

      verifying:
        # No delegate — deterministic executor, model doesn't participate
        executor: { kind: mcp, connection: verifier_harness, tool: run_harness }
        output:
          verifierResult: {}
        transitions:
          verifier_passed:
            target: critiquing
            guards: [{ expr: "$.context.verifierResult.passed == true" }]
          verifier_failed:
            target: critiquing
            guards: [{ expr: "$.context.verifierResult.passed == false" }]

      critiquing:
        delegate: critique-agent
        goal: Adversarial review of the patch
        skills: [review.code.adversarial]
        output:
          critique: {}
          summary: {}
        transitions:
          accept:
            target: human_review
            guards: [{ expr: "$.context.critique.verdict == 'accept' && $.context.risk == 'high'" }]
          accept_low_risk:
            target: done
            guards: [{ expr: "$.context.critique.verdict == 'accept' && $.context.risk != 'high'" }]
          retry:
            target: editing
            guards: [{ expr: "$.context.critique.verdict == 'retry' && $.context.retryCount < 2" }]
            output:
              retryCount: { add: ["$.context.retryCount", 1] }
          escalate:
            target: human_review
            guards: [{ expr: "$.context.retryCount >= 2 || $.context.critique.verdict == 'escalate'" }]

      human_review:
        actor: human
        transitions:
          approve:
            target: done
            actor: human
          reject:
            target: planning
            actor: human

      done:
        terminal: true
```

### Phase 4: Documentation updates

#### 4.1 `CHANGELOG.md`

```markdown
### Added

- **praxec-agent TUI binary** (`crates/praxec-tui/`). Wraps
  the Aether agent framework with praxec as its sole MCP server.
  Adds a deterministic graph-walking interpreter that spawns isolated
  sub-agent sessions per workflow `delegate` state, eliminating
  flow-LLM context accumulation.
- **`delegate` on workflow states.** Config schema addition — states
  can optionally name an agent config for sub-agent delegation.
- **Example `swe-agent.yaml`** — a complete governed coding-agent
  workflow with planning, retrieval, editing, verification, critique,
  and human-review states.
```

#### 4.2 `README.md`

Add after the "Coding-agent recipe" section:

```markdown
## The TUI agent — commodity models outperform frontier

Install `praxec-agent` (or `cargo install praxec --bin praxec-agent`):

    praxec-agent \
      --agent planning=anthropic/claude-sonnet-4 \
      --agent editing=openrouter/qwen-2.5-coder-7b \
      --agent critique=anthropic/claude-opus-4 \
      --max-sub-agent-seconds 120 \
      --max-sub-agent-steps 20

The TUI interpreter walks the workflow graph deterministically:

- **States with `delegate`** spawn isolated sub-agent sessions. Each
  sub-agent sees only its scoped guidance + blackboard — no accumulated
  context from previous phases.
- **States with a single deterministic path** auto-advance without
  LLM involvement. No tokens spent on decisions that guards can make.
- **When multiple paths remain after guard resolution**, the interpreter
  picks the first non-escalation link. The critic + retry cycle
  corrects any wrong picks automatically.

The result: a Qwen 7B editor directed by Sonnet-grade planning reports
and reviewed by an Opus-grade critic costs a fraction of running Opus
for the whole session — and produces better results because each model
does only the task it's best at, with only the context it needs.
```

#### 4.3 New doc: `docs/guides/tui-agent.md`

- Architecture diagram
- Agent config format (CLI args for v1)
- The deterministic interpreter algorithm
- Sub-agent lifecycle (spawn → guidance → submit → collect)
- Timeout poka-yoke (required fields, no defaults)
- Performance comparison: single-model vs sub-agent token costs

#### 4.4 `docs/development/internals.md`

Add `crates/praxec-tui/` to the workspace layout section.

#### 4.5 `docs/reference/stability.md`

Add under Tier 2:

| Artifact | Notes |
|---|---|
| `delegate` on workflow states | Config field and response surface. May be refined based on usage. |
| TUI interpreter behaviour | Spawning, timeout, retry, escalation policies. |
| Agent config CLI format | Provider/model string format. |

#### 4.6 `docs/reference/spec.md §21`

Document `delegate` pass-through semantics.

### Phase 5: Cognitive architectures library — `cognitive-architectures` repo

A sibling open-source repository that serves as the curated library of
Praxec configurations — skills, workflows, agent configs, and connection
definitions. This is the adoption mechanism: the collection of proven
cognitive architectures that operators copy-paste to compose their own
governed agent systems.

Relationship to `mattpocock/skills`:

| mattpocock/skills | cognitive-architectures |
|---|---|
| Static markdown files | Praxec YAML — skills, workflows, connections |
| No runtime semantics | Structured: verb, lifecycle, hash, subject namespace |
| Model reads raw text | Gateway surfaces via HATEOAS; hash-invalidated cache |
| One mental model per file | Skills reference each other; workflows compose skills |

The cognitive architecture thesis, grounded in docs/architecture/research.md:

> A cheap or open-weight model, directed by a precise cognitive architecture
> and governed by a deterministic harness, can match or beat a frontier model
> that operates without structure.

Each architecture encodes: what to think about (guidance scoped to one of the
8 cognitive verbs), when to think it (workflow states in sequence), how to
enforce it (guards + blackboard + deterministic executors), and how to audit
it (transition records).

The `ingest` executor (SPEC §19) adapts mattpocock-style `.claude/skills/*.md`
into Praxec fragments, making `cognitive-architectures` a superset — it can
both ship original Praxec-native architectures and ingest/adopt the best of
what exists in the broader skills ecosystem, wrapping them in governance.

#### 5.1 Repository structure

```
cognitive-architectures/
  README.md
  skills/                        # Reusable guidance fragments (Praxec format)
    plan.specify.change-request.yaml
    diagnose.codebase.search.yaml
    implement.edit.constrained.yaml
    review.code.adversarial.yaml
    review.code.final-approval.yaml
    deploy.safety.checklist.yaml
    debug.reproduction.yaml
    triage.issue.yaml
    compose.integration.yaml
  workflows/                     # Complete governed workflow definitions
    swe-agent.yaml
    pr-review.yaml
    deploy-pipeline.yaml
    tdd.yaml
    triage-router.yaml
    content-publish.yaml
  agents/                        # Reference agent configs
    planning-agent.toml
    retrieval-agent.toml
    editing-agent.toml
    critique-agent.toml
    thin-llm.toml
  connections/                   # Common connection definitions
    structureos.yaml
    github-mcp.yaml
    verifier-harness.yaml
    codebase-graph.yaml
    constrained-edit.yaml
  examples/                      # End-to-end gateway configs composing everything
    full-swe-pipeline.yaml
    review-only.yaml
    deploy-with-governance.yaml
```

#### 5.2 Execution order

After Phase 4 (Documentation):

5a. **Create `cognitive-architectures` repo** with README positioning the
    cognitive architecture thesis and linking to docs/architecture/research.md
5b. **Migrate the 4 skill fragments from Phase 3** into `skills/` as
    standalone YAML files
5c. **Add 5 additional skills** covering remaining docs/architecture/research.md roles:
    `review.code.final-approval`, `deploy.safety.checklist`,
    `debug.reproduction`, `triage.issue`, `compose.integration`
5d. **Add 5 additional workflows** beyond swe-agent: `pr-review.yaml`,
    `deploy-pipeline.yaml`, `tdd.yaml`, `triage-router.yaml`,
    `content-publish.yaml`
5e. **Add README-driven instructions** for how to ingest mattpocock-style
    skills into the Praxec format via the `ingest` executor
5f. **Add reference agent configs** showing the tier structure for
    planning/retrieval/editing/critique agents

---

## Sequential implementation order

1. **Praxec schema + runtime** (Phase 1 — ~1.5 hours)
   - `delegate` on gateway-config, workflow-response schemas
   - Surface in `runtime_response.rs`
   - Accept in `config.rs`
   - `check` warning
   - Error code
   - docs/reference/spec.md §21
   - Tests: config round-trips, response contains delegate when declared

2. **TUI interpreter** (Phase 2 — ~5 hours)
   - `walk_workflow` (merged interpreter + error handling)
   - `spawn_and_wait` sub-agent spawner
   - Agent config CLI parsing
   - TUI config with required timeout poka-yoke
   - Connect Aether's session API to Praxec MCP
   - Tests: delegate state advances, deterministic auto-advance, terminal
     detection, timeout triggers escalation, retry budget exhausts

3. **Example workflow** (Phase 3 — ~1.5 hours)
   - `examples/swe-agent.yaml`
   - Four skill fragments
   - Dogfood: run the TUI against the example, verify it completes

4. **Documentation** (Phase 4 — ~2 hours)
   - CHANGELOG, README, ../guides/tui-agent.md, ../development/internals.md, docs/reference/stability.md

5. **Cognitive architectures library** (Phase 5 — ~3 hours)
   - Create `cognitive-architectures` repo with README
   - Migrate Phase 3 skills + add 5 more (9 total)
   - Add 5 additional workflows (6 total)
   - Add reference agent configs and connection definitions
   - Write README: cognitive architecture thesis, usage instructions,
     mattpocock ingest path
   - Add `examples/` composing skills + workflows + connections into
     copy-paste gateway configs

6. **Cleanup**
   - Delete `SPEC_RESEARCH_GAPS.md` (superseded by this WIP and the
     updated README's coding-agent recipe section)

---

## What does NOT change

- Praxec's 2-tool surface (stable)
- Praxec's non-goal of "no LLM calling" (unchanged — the TUI calls
  models, Praxec governs)
- Existing executors, stores, audit sinks (unchanged)
- Existing workflow definitions (backward compatible — `delegate` is
  optional)

---

## Appendix: FMECA Risk Summary

| # | Domain | Failure Mode | Sev (pre) | Prob (pre) | Mitigation | Sev (post) | Prob (post) |
|---|---|---|---|---|---|---|---|
| F1 | Runtime | Sub-agent never submits | High | Medium | Timeout + step limit (required, no defaults) | Low | Low |
| F2 | Runtime | Sub-agent submits to wrong transition | Medium | Medium | Praxec rejects `INVALID_TRANSITION`; sub-agent retries | Low | Low |
| F3 | Runtime | Sub-agent writes to wrong blackboard slot | High | Medium | Output mapping + typed slots catch most; critic guidance covers rest | Medium | Low |
| F4 | Architecture | Ambiguous branch, wrong pick | Medium | Medium | Deterministic fallback (first non-escalate); critic cycle corrects | Low | Low |
| F5 | Runtime | Praxec process crashes | High | Low | WorkflowStore persistence; TUI reconnects + resume from snapshot | Medium | Low |
| F6 | Runtime | Sub-agent output fails inputSchema | Medium | Medium | Praxec rejects before advancing; sub-agent retries or times out | Low | Low |
| F7 | UX | Operator doesn't set timeout | High | High | No defaults; startup rejects with named error | Low | Low |
| F8 | UX | Agent config misconfigured | Medium | Medium | Startup validation; runtime failures → retry + escalate | Medium | Low |
| F9 | Architecture | Sub-agent context too large | Medium | Medium | Size warning at spawn; timeout catches overload | Medium | Low |
| F10 | Architecture | Interpreter advances to wrong state | High | Low | Interpreter never computes state — reads from praxec.query | Low | Low |
| F11 | Delivery | Aether API breaks | Medium | Low | Pinned Cargo deps; CI catches | Low | Low |
| F12 | Runtime | Stale praxec.query response | Medium | Low | Praxec's save_if_version optimistic locking | Low | Low |

**Result:** 0 High risks remain. 3 Medium risks (F3, F8, F9) all at Low probability — accepted with clear detection mechanisms and v2 improvements planned.