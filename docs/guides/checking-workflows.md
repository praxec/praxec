# `praxec fuzz`

Verifies every workflow in a config with **mock executors** (no real model, git,
or network): a deterministic **graph walk** (reachability/orphans) + a
**per-transition isolation fuzz** (behavioral), plus a capped end-to-end
**integration smoke**. Exits non-zero on an orphan, an un-driveable transition, or
a smoke `EngineError` — wire it into CI. (See the [Coverage model](#coverage-model-p6)
for the full decomposition and smoke-gating rules.)

```bash
praxec fuzz --config gateway.yaml [--iterations 50] [--seed 0]
```

**What it checks (P0):**
- **Wedge** — a non-terminal state with no legal move and no human gate.
- **Livelock** — never reaches a terminal within the step budget.
- **Engine error** — the runtime errored driving the flow.

**Determinism:** a `--seed` makes every run reproducible; a flagged scenario
prints its seed for replay.

## Smart fuzzing (P1)

The mock executor is now **guard-aware**. A static pass over the resolved config
derives, per transition, the executor-output fields that downstream guards read,
and emits values that **satisfy** them — so guard-gated flows actually traverse
instead of stalling. Concretely: if a transition maps `output.field → context.slot`
and a downstream guard checks `$.context.slot == V`, the mock emits `field = V`.

It also adds:

- **Failure injection** — a seeded fraction of executor calls return an error, to
  exercise the workflow's failure path. (An injected failure with no recovery
  policy resolves the mission as `failed`, which is a valid terminal — so this
  surfaces *missing* recovery only where a flow was expected to recover.)
- **`--report json`** — machine-readable output for CI ingestion:

  ```bash
  praxec fuzz --config gateway.yaml --report json
  ```

**Remaining limitation:** guard satisfaction is best-effort and single-valued —
when a slot is read by conflicting guards across branches, the mock satisfies one;
deliberate per-branch exploration (driving each distinct branch) is future work.
Output sources other than `$.output.<field>` passthrough (operators, literals) are
not synthesized.

**Limitation (P0):** mock executors return empty output, so a flow that only
progresses when a guard reads a specific output value will under-progress.
Schema-derived mock outputs and failure injection arrive in P1.

## Declared-property scenarios (P2)

Beyond the zero-config invariant sweep, you can assert **specific properties** of
named workflows with `praxec test`:

```bash
praxec test --config gateway.yaml --scenarios tests.yaml
```

A scenario file names a workflow and what to expect; the harness fuzzes that
workflow and checks each clause. Exits non-zero on any failed assertion.

```yaml
tests:
  - name: guarded reaches done
    workflow: guarded_flow
    iterations: 30          # default 20
    expect:
      reaches: [done]                  # some run visits this state
      never_reaches: [shipped]         # no run ever visits this state
      final_state: [done]              # some run terminates in this state
      outcome_met: [approved]          # some run meets this ADR-0008 outcome
```

Semantics: positive clauses (`reaches` / `final_state` / `outcome_met`) pass when
**some** fuzzed run satisfies them; `never_reaches` passes when **no** run reaches
the state. Visited states come from the audit trace; met outcomes from the
mission's resolved outcomes.

**Limitation:** because the smart mock *satisfies* guards (P1), `never_reaches`
currently tests **structural** unreachability, not the adversarial "unreachable
*while* a predicate holds" (e.g. "ship unreachable while tests are red"). That
adversarial form needs a probe mode that deliberately emits guard-FAILING outputs
and captures per-step context — it's the next increment.

## Driving real architectures (P3)

Two improvements let the harness drive real composed workflows (e.g. the
cognitive-architectures library) instead of stalling:

- **Early-livelock detection.** The chooser gives up after a few no-progress
  choices (state version unchanged), and the per-run step budget is bounded, so a
  stuck flow is reported in a handful of steps instead of spinning to the cap. A
  full library sweep that used to time out now completes in seconds.
- **Capability-output satisfaction.** When a transition calls a capability via
  `kind: workflow`, the mock emits that capability's declared `snippet.outputs`
  (typed dummy values; an enum picks its first member), so the orchestrator's
  `use.outputs` binding propagates and the flow advances past the call.

### Sweeping a workflow library

`cognitive-architectures/scripts/fuzz.sh` runs `praxec fuzz` over every example
config and reports per-workflow verdicts. It's wired into that repo's
`validate.sh` as an **informational** step (it doesn't fail the build yet, because
some flows legitimately livelock — see limitations).

A representative sweep result over the cognitive library (28 definitions): most
capabilities that the mock can satisfy reach a clean resolution; **human-gated**
capabilities report `Livelock` (they structurally require a person); and some
flows `Wedge` (make progress, then stall where the composition expects an output
the mock can't yet produce) — that `Wedge` set is actionable signal about
capability-composition gaps.

### Limitations / next increments

- **Evidence-gated HITL.** A human-approval transition guarded by `evidence`/`role`
  can't be satisfied by the mock, so human-gated flows report `Livelock`. Getting
  past them needs an evidence-satisfaction mode (or a "treat human gates as
  passable" fuzz flag).
- **Required workflow inputs.** The fuzzer starts workflows with empty input, so a
  workflow whose `inputSchema` marks fields required fails to start
  (`ENGINE_ERROR`). A future increment generates a dummy input from the schema.
- **Shallow dummies.** `snippet.outputs` dummies cover enum/primitive types;
  nested object/array shapes are emitted empty.

## Live mode (P4)

The mock path proves the state machine is sound; **live mode** checks whether a
*real model* can actually navigate it:

```bash
praxec fuzz --config gateway.yaml --live --model anthropic:claude-haiku-4-5-20251001
```

`--live` swaps the mock executor registry + seeded chooser for the **real
executor registry and a real-model `AgentChooser`** (the same machinery as
`praxec orchestrate`), driving each workflow to a verdict under the same
invariant oracle. It needs provider credentials (e.g. `ANTHROPIC_API_KEY`) and
runs real executors, so it is **local-only — never wire it into CI** (it's
non-deterministic and costs tokens). Use it as the reality check after the
deterministic mock sweep is green.

Without credentials (or against a workflow with no agent-actionable step) the
real chooser has nothing to decide and the run is reported as a violation rather
than panicking — so `--live` is safe to invoke, it just needs a real model and an
agentic workflow to be meaningful.

The same `--live --model` pattern extends to `praxec test` (declared-property
scenarios against a real model) — a small follow-up.

## Coverage model (P6)

Coverage uses the correct decomposition — **a deterministic graph walk** (structural)
plus **per-transition isolation fuzz** (behavioral) — not random end-to-end traversal.

- **Graph walk.** Enumerates every `state → transition → target` edge, computes
  reachability from `initialState`, and flags **orphaned states** (unreachable —
  the same class `praxec check` warns on). Deterministic and exhaustive.
- **Per-transition isolation fuzz.** Each transition is tested *alone*: the harness
  seeds a workflow instance at the transition's source state with a context fuzzed
  over the transition's read-set — every `$.context.*` read seeded as a
  **blackboard-typed** dummy (guard slots, including `branches[].when` predicates,
  at satisfying values) and `$.workflow.input.*` reads seeded from the workflow
  `inputSchema` — then submits just that one transition and checks
  it behaves — fires on a satisfying context and advances to its declared target,
  **rejects** on a guard-violating context, handles executor failure, resolves its
  output mapping, and never errors unexpectedly. `actor: human` transitions are
  driven with a human principal, so each human branch (approve/reject) is tested
  in isolation.

Because each transition is verified independently, **full coverage is linear in
the number of transitions** — no path-combination explosion. If every step is
correct for all the values it can branch on and the graph is structurally sound,
the chained whole is sound. Random graph traversal is deliberately NOT the coverage
mechanism.

```bash
praxec fuzz --config gateway.yaml          # walk + per-transition + a 1-run smoke
```

The end-to-end drive is kept only as a **capped integration smoke** (a single run
that proves the whole workflow can execute start-to-finish); `--iterations` caps
only that smoke. The smoke seeds the mission's start input from the workflow's
`inputSchema`, so flows with required inputs actually start.

**Smoke gating.** The smoke gates the exit code **only on an `EngineError`** — the
flow could not execute at all (e.g. an unresolved script subject, a missing
required input). A mock chooser cannot produce valid agent outputs, so it cannot
drive an *agent-heavy* flow to a terminal; the resulting `Wedge`/`Livelock` is
reported as a **`⚠` advisory**, not a failure (use `--live --model` for a
real-model end-to-end check). The exit code is non-zero if the walk finds an
orphan, any transition can't be driven correctly, or the smoke hits an
`EngineError`.

### What it catches
- **Orphaned / unreachable states** (graph walk).
- **Un-driveable transitions** — e.g. a guard the harness can't satisfy (an
  `evidence`/`role` gate with no backing store), a transition whose output mapping
  can't resolve, or one that fires a guard it shouldn't.
- **Wrong target, type-mismatched output, unhandled failure** (per-transition).

### Limitations
- Evidence/role/permission-gated transitions report as *un-driveable* (the harness
  can't synthesize evidence yet) rather than being exercised — honest signal, not a
  false pass.
- Output dummies cover enum/primitive + blackboard-typed slots; deeply nested
  object/array shapes are shallow.
