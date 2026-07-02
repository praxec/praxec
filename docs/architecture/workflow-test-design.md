# Workflow Test / Fuzz Harness — Design

**Status:** Draft for review
**Date:** 2026-06-16
**Scope:** A first-class Praxec feature that tests and fuzzes any workflow
config — used first to validate the cognitive-architectures library, and
available to every Praxec user for their own workflows.

---

## 1. Problem

Today we can prove a workflow config *loads* (`praxec check`, the V1–V23
cloud) but not that it *behaves*. The execution test doubles that exist
(`DryRunExecutor`, `walk_workflow`, `ScriptedRegistry`, `MemoryAuditSink`) live
inside Praxec's own Rust tests — they can't be aimed at an external config like
cognitive-architectures as a black box. `praxec orchestrate` can drive a
workflow end-to-end but needs a **live LLM**, so it is non-deterministic, costs
tokens, and is unfit for CI.

The gap: **no deterministic way to execute a workflow and assert it can't wedge,
crash, or skip a guard under the full surface of things its steps might return —
including failures.**

## 2. Goal

A generic, first-class harness — `praxec fuzz` / `praxec test` — that:

1. Aims at **any** resolved config (single definition or a whole repo), with **no
   edits to the workflow YAML**.
2. Overlays a **mock executor registry** that intercepts every executor kind, so
   no real model / git / cargo / network is touched.
3. **Fuzzes the workflow's nondeterminism surface** — `transition choices ×
   executor outputs × failure injection` — under a deterministic seed.
4. Checks **generic invariants for free** (zero config) and **optional declared
   properties** when you want to nail a specific behavior.
5. Emits a **report** (text + JSON) with a minimal **repro seed** per violation,
   and a non-zero exit on any failure.

The cognitive-architectures suite is then just: run this over the repo. The same
mechanism works for any workflow any user writes.

## 3. Non-goals

- Not a replacement for `check` (static validation stays the first gate).
- Not a correctness oracle for *semantic* output quality ("is the generated code
  good") — that requires a real model (Mode 2, §11) and human judgment.
- Not a load/throughput benchmark (that's docs/reference/performance.md).
- The harness does **not** prove a real model can navigate a flow — only that the
  state machine is sound across the response space. Mode 2 covers the former.

## 4. Core concept — one generic harness, two tiers of checking

```
            ┌─────────────────────── praxec fuzz / test ──────────────────────┐
            │                                                                    │
 config ──▶ │  resolve (V1–V23)  ─▶  Response-schema deriver  ─▶  Fuzz engine    │ ─▶ Report
            │                                                     │  │  │  │      │   (text+JSON,
            │      ┌────────────────────────────────────────────┘  │  │  │      │    repro seeds)
            │      ▼                ▼                  ▼             ▼             │
            │  Mock registry   Fuzzing chooser   Failure injector   Invariant     │
            │  (all kinds)     (legal moves)     (timeout/err/…)    oracle        │
            └────────────────────────────────────────────────────────────────────┘
```

- **Tier 1 — generic invariants (free, zero config).** Aim it at anything; it
  hunts for structural failures (§7). This is what runs over all of
  cognitive-architectures and any user config out of the box.
- **Tier 2 — declared properties (optional).** A scenario file asserts
  workflow-specific behavior (§8). Praxec's **flagship fixtures** (§12) are the
  canonical users of this tier and double as engine conformance tests.

## 5. Architecture

A new crate `crates/praxec-test` (library) plus `fuzz` / `test`
subcommands wired into the `praxec` binary. The crate depends on
`-core`, `-executors`, and (for `kind: agent`) `-agents`, and reuses the
existing in-memory store / audit sink / runtime.

| Component | Responsibility |
|-----------|----------------|
| **MockRegistry** | An `ExecutorRegistry` impl returning a generated/scripted response for any `(kind, subject/connection/tool)` invocation point. Never spawns a process or calls a model. |
| **Response-schema deriver** | Static pass over the resolved definition computing each invocation point's **read-set** (§6). |
| **FuzzChooser** | A `TransitionChooser` that, at agent decision points, enumerates the legal transitions; coverage-guided. Deterministic transitions auto-run in the engine; human transitions are resolved by the failure injector (approve/decline). |
| **FailureInjector** | Decides, per step, whether to return a normal response or inject a failure variant (timeout, executor error, malformed/empty output, boundary value, declined gate). |
| **Invariant oracle** | After each step and at terminus, evaluates the Tier-1 invariants (§7) and any Tier-2 assertions (§8). |
| **CoverageTracker** | Records visited `(state, transition)` edges and which failure variants fired; drives exploration to saturation and reports coverage. |
| **Reporter** | Aggregates per-definition results into text + JSON, including the seed + response trace to replay any violation. |
| **Driver** | Wires MockRegistry + FuzzChooser + runtime; runs N seeded scenarios per definition until coverage saturates or a budget is hit. |

**Determinism:** a single u64 seed drives the RNG for choices, response samples,
and failure injection. The report prints the seed of every failing scenario;
`--seed <n>` replays it exactly.

## 6. Response-schema derivation (the hard part)

An `agent`/`llm`/`cli`/`script` step returns free-form data; we can't generate
arbitrary blobs. Insight: **the machine can only branch on the parts of a
response it actually reads.** So derive, per invocation point, the **read-set**:

- RHS of the transition's `output:` mapping (`$.output.<path>` references).
- Fields referenced by **guard expressions** (`expr`, `evidence`) on transitions
  out of the target state.
- The downstream consumer's `inputSchema` / capability `snippet.inputs`.
- For `kind: workflow`, the sub-workflow's declared `snippet.outputs` (already
  typed — easy).
- For `kind: human`, the finite outcome set (approve / decline / request-changes).

The fuzzer then samples values **only for read-set fields**:
- **Valid** samples — type-correct, within enum/range/format.
- **Out-of-contract** samples — wrong type, missing field, empty, boundary
  (0, "", null, huge, negative), to probe failure handling.
- Plus **executor-level** failures (the step itself fails before producing
  output): timeout, error, non-zero exit.

This keeps the surface finite and targeted instead of generating noise, and ties
naturally to contracts the engine already declares.

## 7. The invariant oracle (Tier 1 — free on any config)

A scenario **fails** if any of these is violated:

1. **No panic / engine error** — the runtime never returns an internal error or
   panics for a contract-valid input.
2. **No stuck state** — no non-terminal state where, after guard evaluation, zero
   legal moves remain (a silent wedge).
3. **Failures are handled** — an injected executor failure must resolve to a
   declared `reliability` path (retry/fallback), a recovery link, or a terminal —
   never a wedge or an unhandled propagation.
4. **Version monotonicity / optimistic lock** — `version` strictly increases on
   each mutating step; stale-version submits are rejected, not applied.
5. **Locks released** — any acquired plan/file lock (planner cohorts) or repo
   file-lock is released on terminal/failure; none leak past completion.
6. **Liveness** — every run reaches a terminal state **or** a legal HITL park
   within the step budget; no livelock.
7. **Actor integrity** — no `actor: human` / `actor: deterministic` transition is
   ever fired by the agent chooser; `ACTOR_MISMATCH` is enforced.
8. **Audit completeness** — every executed transition emits a transition record;
   the audit trace replays to the observed final state.

## 8. Declared-property scenario format (Tier 2 — optional)

```yaml
version: "1.0.0"
tests:
  - name: ship-unreachable-while-red
    workflow: ship_guard           # definitionId (namespace-prefixed ok)
    input: { repo: "demo" }
    # Optional fixed mocks; omit to let the fuzzer generate them:
    responses:
      checked.run_check: [ { exitCode: 1, stdout: "FAIL" } ]
    fuzz: { iterations: 500, inject_failures: true }   # omit → invariants only
    expect:
      never_reachable:
        - { state: shipped, while: "$.context.lastCheck == 'red'" }
      # other assertion kinds:
      # final_state: published
      # outcomes: [ { id: approved, met: true } ]
      # audit_events: [ "human.approval.requested" ]
```

- No `expect` block → Tier-1 invariants only (pure fuzz).
- `never_reachable` is the headline property kind: across the entire fuzzed
  space, the named state is unreachable while the predicate holds. This is how a
  TDD/guard fixture proves its one aspect.

## 9. CLI surface

```bash
# Zero-config invariant fuzz over one definition or every definition in a config:
praxec fuzz --config gateway.yaml [--definition D] [--seed N] \
              [--iterations N] [--report text|json]

# Declared-property scenarios:
praxec test --config gateway.yaml --scenarios tests.yaml [--report text|json]

# Mode 2 — real local models, off-CI (see §11):
praxec test --config gateway.yaml --scenarios tests.yaml --live --model anthropic:claude-...
```

Both share one engine; `test` is `fuzz` plus assertions. Non-zero exit on any
violation. `--report json` for CI ingestion.

## 10. Report format

Per definition: `pass|fail`, scenarios run, **coverage** (states hit / total,
transitions hit / total, failure-variants exercised), and per violation:

```
✗ flow.safe-refactor — STUCK_STATE
    seed: 0x9f3c…   (replay: praxec fuzz --config … --definition flow.safe-refactor --seed 0x9f3c…)
    path: idle → planning → editing → (verify failed) → ???   no legal move
    response trace:
      planning.draft_plan  → { plan: {…} }            (valid)
      editing.apply        → TIMEOUT                   (injected)
      verifying.run_verify → { success: false }        (out-of-contract: no recovery link)
```

A machine-readable JSON mirror carries the same fields for CI dashboards. The
aggregate footer is the "report back": N workflows, P passed, F failed, coverage
summary.

## 11. Sub-workflow handling & Mode 2

- **Sub-workflows (`kind: workflow`).** Default: **mock at the snippet boundary**
  — return values typed by the capability's `snippet.outputs` (unit isolation,
  fast). Flag `--recurse-subworkflows` runs them for real under the same mock
  registry (integration depth). Both are valid; default is isolation.
- **Mode 2 (live, local).** `--live --model <m>` swaps the MockRegistry for the
  real executor registry and a real-model chooser, running the *same scenarios*
  against actual models. Never runs in CI / PRs — it's the local reality check
  after Mode 1 is green. Tier-2 `expect` assertions still apply; Tier-1
  invariants still apply.

## 12. Flagship conformance fixtures

A set of **minimal, single-aspect** workflows (Praxec's own, under
`crates/praxec-test/fixtures/` with paired scenario files), one feature
each — they are the engine's conformance suite *and* the harness's dogfood:

guard-rejection · deterministic-chaining · output-mapping · parallel
join/fan-out · HITL park & resume · reliability retry/fallback · recovery link ·
`script_acknowledged` · actor-mismatch · optimistic-lock · planner cohort &
file-lock release. Each carries a Tier-2 property nailing its aspect.

## 13. cognitive-architectures integration

- Extend `scripts/validate.sh`: after `check`, run `praxec fuzz` over every
  orchestrator/capability/workflow in the repo (Tier-1 invariants).
- Add `cognitive-architectures/tests/*.yaml` with Tier-2 properties for the
  flagship orchestrators (e.g. `flow.add-feature` reaches `pr_open` with the
  review outcome met; `flow.safe-refactor` never reaches `done` if the baseline
  comparison fails).
- Wire both into cog-arch CI. Public release gate = green fuzz over the library.

## 14. Where it lives

- `crates/praxec-test` — the engine (MockRegistry, deriver, FuzzChooser,
  injector, oracle, coverage, reporter, driver). Library + unit tests.
- `praxec` binary — `fuzz` and `test` subcommands (clap), default-on; gate
  behind a `test-harness` feature only if binary size demands it.

## 15. Testing the harness itself

- The flagship fixtures (§12) are *known-good* and *known-bad* pairs: a
  deliberately-wedging fixture must make the oracle report `STUCK_STATE`; a sound
  one must pass. This tests the oracle both ways.
- Deterministic replay: a fixed seed reproduces the identical scenario + verdict.
- Snapshot the report JSON for the fixtures.

## 16. Phasing

1. **P0 — core (MVP):** MockRegistry + FuzzChooser + Driver + Tier-1 oracle +
   `praxec fuzz` + text report. Enough to sweep cog-arch for wedges/crashes.
2. **P1 — smart fuzzing:** response-schema deriver + valid/out-of-contract
   sampling + failure injector + coverage tracker + JSON report + seeds.
3. **P2 — properties:** scenario format + `praxec test` + `never_reachable` /
   `final_state` / `outcomes` / `audit_events`.
4. **P3 — fixtures + cog-arch CI:** flagship conformance fixtures; wire
   `validate.sh` + cog-arch CI.
5. **P4 — Mode 2 live:** `--live --model`, local-only.

The first implementation plan targets **P0 + P1 + a thin slice of P3** (a couple
of fixtures to prove the oracle both ways), since P0 alone (no failure injection,
no schema derivation) under-delivers on "hit the entire surface."

## 17. Open questions / risks

- **Read-set completeness.** If a guard reads a context slot written several
  states earlier, the deriver must trace provenance across states, not just the
  immediate transition. Mitigation: build the read-set over the whole reachable
  sub-graph from each write point; start conservative (over-fuzz) and narrow.
- **State-space explosion.** Coverage-guided search + a per-definition scenario
  budget bound it; report coverage so silent under-exploration is visible (never
  claim "passed" when coverage was low — surface the coverage number).
- **`kind: agent` interception.** The mock must satisfy the sub-agent session
  contract without a model; reuse the existing `MockSessionRunner`/scripted
  provider test doubles rather than inventing a new seam.
- **Free-form agent outputs with no downstream read.** If nothing reads a step's
  output, there's nothing to fuzz there — correct, and the coverage report should
  make that explicit rather than implying coverage.
