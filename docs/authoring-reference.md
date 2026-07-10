# Praxec Authoring Reference

How to author the four praxec extension types — **deterministic tools** (scripts/MCP), **workflows** (flows), **capabilities** (leaves), and **skills** — without hitting the traps. This is the field guide; the SPEC is the law. Every rule below is one that has actually bitten a real drive.

## The mental model

- A **workflow** is a state machine. States have `transitions`; each transition has a `target` state, an `actor` (`deterministic` | `agent` | `human`), optional `guards`, and optionally an `executor`.
- A **flow** (`orchestrators/flow.*.yaml`) is a workflow that may compose *other* workflows (`kind: workflow`). Only flows may nest workflows.
- A **capability** (`capabilities/cap.<verb>.*.yaml`) is a composition **leaf** — a single-purpose workflow that may NOT invoke `kind: workflow`. Its `snippet` block declares typed `inputs`/`outputs` that the slot table (V13) recognizes.
- A **skill** (`skills/*.yaml`) is static, hash-pinned instructions injected as an agent's **system message**. It carries method, not task and not model.
- The trio: **agent** = the model binding (no instructions). **skill** = the how (system message). **prompt_template** = the task (user message).

## Blessed roots (or config load fails)

Capability verbs and skill/flow namespaces must be blessed roots. Blessed verbs: `inspect`, `plan`, `implement`, `compose`, `review`, `verify`, `coordinate`. A multi-stage thing that calls sub-workflows is a **flow in `orchestrators/`**, not a capability — e.g. a Vee "meta" workflow is `flow.sebok`, never `cap.sebok`. (`strict_namespacing: false` in gateway config is an escape hatch for WIP namespaces, not a license to misplace files.)

## The #1 trap: sub-workflow input resolution

**A snippet-input `default:` is NOT merged into `$.workflow.input` for `$.workflow.input.X` path resolution when a sub-workflow is invoked via `use.inputs`.** Every input a callee reads, the caller must pass **explicitly**, AND the callee must declare it. A declared default alone → `unresolved arg path '$.workflow.input.X'` **permanent error** that kills the run.

```yaml
# CALLER (flow.implement.deliverable)
use:
  inputs:
    cargo_scope: "$.workflow.input.deliverable.cargo_scope"  # explicit — resolves
    min_tests:   "1"                                          # literal — also fine
# CALLEE (cap.implement.build-loop) must ALSO declare it:
snippet:
  inputs:
    cargo_scope: { type: string, default: "" }   # default only used if the caller passes it
```

If the value rides in a plan/spec (e.g. `deliverable.cargo_scope`), the planning step must **always emit the key** (empty string when N/A) — a missing key is the same permanent error.

## Guards must be exhaustive (or the validator warns)

When a state branches on a `$.context.X` value across `actor: deterministic` guarded transitions, add an **unguarded default** transition (no `guards:` key at all — not an empty list) routing out-of-domain values to an explicit error/human state. Otherwise an unexpected value falls through to recovery links. The unguarded arm is lowest-precedence, so it's the safe default.

```yaml
verdict_gate:
  transitions:
    pass:  { target: next,   actor: deterministic, guards: [ { kind: expr, expr: "$.context.v == 'pass'" } ] }
    unexpected: { target: failed, actor: deterministic }   # no guards → default
```

## Terminal outcomes: mark your failures

The engine derives `MissionStatus::Succeeded` for a terminal reached with **no `outcome:` marker** ("unmarked terminal → success"). So a state named `failed` with just `terminal: true` reports **success** — the inverse of what you want. Always:

```yaml
done:   { terminal: true, outcome: success }
failed: { terminal: true, outcome: failure }   # REQUIRED or a guard-routed abort reports success
```

(`done` may stay unmarked since unmarked == success, but be explicit if the flow declares `outcomes:`.)

## Executors

- `kind: script` — a hash-pinned curated script (`scripts-library/*.yaml`). `workingDirectory` + positional `args:` (each an arg-path or literal). Parsed stdout JSON is available as `$.output.json.*`. Use `treatNonZeroAsFailure: false` to capture a `status: fail` payload instead of erroring.
- `kind: mcp` — call a wired connection's tool. `connection` + `tool` + a `map:` of tool-arg → arg-path. An internally-tagged object arg (e.g. `{"status":"complete"}`) can't be built by `map:` — seed it in `initialContext` and bind by path.
- `kind: workflow` — nest a sub-workflow (flows only). `use.inputs` (pass in) + `use.outputs` (bind child outputs to `$.context.*`). Remember: a transition `output:` block does NOT seed a typed slot for a later `use.inputs` — only `inputs:` and a child's `use.outputs` do.
- `kind: agent` — an autonomous sub-agent (Option A). Under `auto_drive`, `actor: agent` moves are driven headlessly; `actor: human` moves genuinely **park** (mission `Waiting`) for the operator.
- `kind: noop` — a gate marker / a state whose only job is to record a transition.

## verify slots + hop_slot

A `verify` capability is category-Deterministic (its primary executor must be `script`/`mcp`, per V6) — an agent-judged check must be a `review` cap, not `verify`. The engine injects the canonical `verifyIn`/`verifyOut` (`praxec://hop#/$defs/verifyOut`, with `status`/`summary`/`criteria`/`findings`). `$.context.verify` is an **engine-owned slot key** — only `hop_slot: verify` transitions may produce it; a plain `kind: workflow` call of a verify cap lands on your own slot (e.g. `$.context.verify_report`). Note `hop_slot:` forces strict-blackboard mode (a known ergonomic sharp edge).

## Agent output contract (the ceremony trap)

An `actor: agent` transition with an `inputSchema` requires the agent to call the submit/final_answer tool with all required fields. **The more required fields, the more likely the model does the work but fails to sign off → `AGENT_NO_RESULT`** (observed across the whole model chain on real deliverables). Keep the required set minimal; make optional what can be defaulted or harvested deterministically (e.g. `files_written` from git-diff). A slice with correct code+tests should not be discarded for a missing bookkeeping call.

## Cargo verdict scoping (staged verdict)

Scope the per-slice `cargo test` to the deliverable's crate (`-p <crate>`) for fast RED/GREEN feedback; run one full `cargo test --workspace` gate **once per deliverable** before `mark_status` (never mark complete on scoped evidence alone). Per-slice RED (compile + assertion-fail) and GREEN (pass) determinism is preserved — only the incidental rebuilt/linked binaries shrink. `cargo test -p <leaf-crate>` is ~0.85s vs minutes for `--workspace`.

## Parallel drives

The "one cargo at a time" rule is scoped to **one cargo per target dir**. Git worktrees have their own `target/`, so N parallel worktree drives don't contend on the build lock — parallel drives are safe (cap N≈2 on a 16-core/23G box for RAM). The control-plane runaway that this rule was blamed for is bounded by cpm-planner's `MAX_ATTEMPTS=3` circuit-breaker.

## Checklist before you ship a workflow

1. `praxec check --config <gateway>` → **0 errors, 0 warnings** (warnings are exhaustiveness gaps).
2. Every sub-workflow input is passed explicitly (no reliance on defaults merging).
3. Every branching state has an unguarded default.
4. Every `failed` terminal has `outcome: failure`.
5. verify caps are deterministic; agent-judged checks are `review` caps.
6. Agent `inputSchema` required-field set is minimal.
7. A doc/flow/skill count is never hardcoded in prose — it drifts.
