# The TUI agent — deterministic interpreter + sub-agent orchestration

`praxec-tui` (also installed as `praxec`) wraps the Aether agent
framework with one architectural rule: **praxec is the sole MCP
server**. Aether's built-in tool surface is replaced. The model's only
tools are the two Praxec tools — `praxec.query` for reads
(home/search/describe/get/explain) and `praxec.command` for writes
(start/submit/define). Every action is schema-
validated, guard-checked, and audited.

Layered on top: a **deterministic graph-walking interpreter** (SPEC §21)
that drives a workflow to completion by routing each state to the right
actor. Most states never invoke an LLM. The ones that do invoke an
**isolated sub-agent session** with scoped context.

## Architecture

```
praxec (TUI binary)
  │
  ├─ Aether framework (TUI, model calling, session management)
  │
  ├─ Deterministic interpreter (walk_workflow, ~150 LOC):
  │     loop {
  │       praxec.query({ workflowId })
  │       if completed: return context
  │       if delegate: spawn_and_wait
  │       elif single actionable link: auto-submit
  │       else: pick first non-escalate
  │     }
  │
  ├─ Sub-agent spawner (one Aether session per delegate state)
  │
  └─ Sole MCP server: praxec (child process, 2 stable tools)
       │  praxec.query   (home / search / describe / get / explain)
       │  praxec.command (start / submit / define)
       │
       └─ Executor proxying via connections:
            ├─ kind: cli   → shell, lint, build, constrained-edit
            ├─ kind: mcp   → scip-mcp, verifier-harness-mcp, structureos
            ├─ kind: rest  → external APIs
            ├─ kind: workflow → nested governed workflows
            └─ kind: human → approval gates
```

## The `delegate` field (pass-through pattern)

A workflow state can declare:

```yaml
states:
  planning:
    delegate: planning-agent     # ← SPEC §21
    goal: Normalise the change request
    skills: [plan.specify.change-request]
    transitions:
      ready:
        target: retrieving
```

The gateway treats `delegate` as **pass-through only** — it surfaces
the string verbatim at the top level of every workflow response, never
reads or branches on it. The TUI interpreter is the sole consumer.

This separation is deliberate: workflow authors declare *where the work
is done* (which state); operators declare *who does it* (which
provider/model). A workflow shipped in the `cognitive-architectures`
library names `planning-agent` / `editing-agent` / etc., and any
operator plugs in any combination of providers behind those names
without editing the workflow.

## The deterministic interpreter algorithm

`walk_workflow` is one function, ~150 lines. It loops:

1. **`praxec.query({ workflowId })`** → fetch current state + links.
2. **Completed?** → return `context` (terminal state, walk done).
3. **`delegate` present?** → look up the agent config; spawn a
   sub-agent session; wait for it to return.
   - **Sub-agent succeeded** → re-fetch `praxec.query({ workflowId })`;
     if `version` advanced, reset retry counter, continue.
   - **Sub-agent timed out** or didn't advance the workflow →
     increment retry counter. If under 3 retries, loop and try again.
     If exhausted, submit the `escalate` transition (when declared)
     or propagate `SubAgentTimeout`.
4. **No `delegate`** → pick an actionable link:
   - Filter out `actor: deterministic` links (the gateway auto-chains
     those itself — SPEC §6).
   - If exactly one remains, submit it.
   - If multiple, submit the first non-`escalate`. The critic + retry
     cycle corrects wrong picks on the next iteration.

The whole loop is structurally simple by design. No clever
metaprogramming, no plugin system, no policy hooks. Adding extension
points without a clear use case adds drift surface area.

## Sub-agent lifecycle

For each `delegate` state visited:

1. **Build the system prompt** from the response's `guidance.goal` +
   `guidance.instructions` + serialized `context` blackboard.
2. **Warn (don't block)** if context exceeds the configured
   `max_blackboard_bytes` (default 16 KiB). Threshold breach signals
   an architecture leaking previous-phase data into the downstream
   sub-agent — fix it in YAML by scoping the upstream output mapping.
3. **Spawn an Aether session** with the agent's `provider/model` + the
   two Praxec MCP tools (no extra tools, no out-of-band access) +
   the configured `max_steps` and `timeout`.
4. **Wait** for the session to either call `praxec.command` (advancing
   the workflow) or hit timeout / step limit.
5. **Return**. The interpreter checks `workflow.version` after every
   spawn — a session that returns Ok but didn't advance is treated as
   a soft timeout (the retry path covers it).

## Agent configuration

Provided via repeated CLI flags:

```bash
px walk \
  --workflow swe_agent \
  --agent planning=anthropic/claude-sonnet-4 \
  --agent editing=openrouter/qwen-2.5-coder-7b \
  --agent critique=anthropic/claude-opus-4 \
  --max-sub-agent-seconds 120 \
  --max-sub-agent-steps 20
```

Format: `name=provider/model`. The interpreter resolves `delegate: <name>`
against this map at spawn time. Missing name → an actionable
`InterpreterError::UnknownAgent` naming both the state and the
agent so the operator sees exactly which `--agent` flag they forgot.

TOML agent config files (`~/.praxec/agents/*.toml`) are a planned
v2 UX improvement.

## Timeout poka-yoke — no defaults by design

```rust
pub struct TuiConfig {
    pub max_sub_agent_seconds: u64,     // NO DEFAULT
    pub max_sub_agent_steps: usize,     // NO DEFAULT
    pub max_blackboard_bytes: usize,    // default 16 KiB
}
```

`--max-sub-agent-seconds` and `--max-sub-agent-steps` are **required by
design**. The TUI rejects startup if either is missing, with an error
message naming both fields and explaining why:

> TUI config requires both --max-sub-agent-seconds and --max-sub-agent-steps.
> These have no defaults by design: an unbounded sub-agent is a foot-gun
> (orphan tasks, runaway cost, looping critic). Set them explicitly per
> your tolerance, then run again.

This is the same FMECA discipline you'll see throughout the codebase
(see the no-shortcuts lint at `crates/praxec-core/tests/no_shortcuts.rs`):
**don't write a default that hides what an operator must consciously
decide.** Pick values you can defend.

## Why this beats single-frontier-model loops

A single-flow coding agent accumulates context as it works:
the issue → the plan → the file reads → the diff attempts → the test
output → the critique → the retry. Token cost compounds; the model
also drifts as the context grows.

The deterministic interpreter inverts the pattern:

- **The flow is YAML**, not an LLM. It never accumulates.
- **Each phase gets a scoped session** with only the blackboard slots
  it needs. The editor sees the plan + the evidence pack — not the
  conversation history that produced them.
- **The right model for each role.** Frontier model for planning and
  critique (where reasoning quality matters most). Commodity model for
  retrieval and editing (where speed + cost matter).

Concrete: a Qwen 7B editor directed by Sonnet-grade planning reports
and reviewed by an Opus-grade critic costs a small fraction of running
Opus for the whole session. And produces better results, because each
model does only the task it's best at, with only the context it needs.

Quantitative comparison is deferred to a separate benchmark spike;
the qualitative argument is grounded in `docs/architecture/research.md` (the
cognitive-architecture thesis).

## What's deferred to v2

The `AetherSubAgentSpawner` now invokes `aether_cli::headless::run_headless`
directly (GAP-C closed in v0.3 cycle). The interpreter remains fully
tested via the scripted-double pattern in `tests/interpreter.rs`
(11 atomic assertions, one per branch). End-to-end `px walk`
against a live `praxec` is now wired: `run_walk` spawns a
`PraxecChildCaller` (the production rmcp child-process `McpToolCaller`
in `mcp_caller.rs`), starts the workflow via `praxec.command`, and
drives it through `walk_workflow` to completion. The earlier
`WALK_NOT_WIRED` fail-loud placeholder is gone.

Other v2 work: a thin LLM-based branch picker when guards can't resolve
to a single path (v1 uses the deterministic first-non-escalate fallback,
which works in practice because the critic cycle corrects wrong picks);
TOML agent config files; a real benchmark spike of sub-agent ensemble
vs single frontier model (scaffold + methodology lives at
`docs/BENCHMARK-COGNITIVE-ARCHITECTURE.md`; running it requires API
budget).

## See also

- [docs/reference/spec.md §21](../reference/spec.md) — the `delegate` field contract.
- [`crates/praxec-tui/src/interpreter.rs`](../crates/praxec-tui/src/interpreter.rs)
  — the interpreter implementation.
- [`crates/praxec-tui/tests/interpreter.rs`](../crates/praxec-tui/tests/interpreter.rs)
  — the FMECA-style test suite.
- [`cognitive-architectures`](https://github.com/praxec/cognitive-architectures)
  — sibling repo with curated Praxec workflows + skills + agents.
- [docs/architecture/research.md](../architecture/research.md) — the underlying thesis.
