# ADR-0008: Missions, outcomes, and resolution status

**Status:** Accepted

**Date:** 2026-06-11

## Context

[ADR-0007](0007-agents-first-class-workflow-executors.md) made an agent the
ephemeral vehicle that drives **exactly one workflow instance toward its goal,
then leaves**. But "its goal" was underspecified, and grounding in the runtime
exposes a missing layer: a workflow today is a series of **tactical, per-state
goals** with terminal states that mean only "stopped" — there is no
**strategic, measurable definition of done** for the run as a whole.

Grounding (this is designed against what the runtime already does):

- Each state carries a per-phase **`goal`** — tactical guidance, templated against
  the live instance and surfaced as `guidance.goal`
  (`runtime_response.rs:146`, `validate.rs:364`). This is "what to do in this
  phase," not "what the run must achieve."
- Transitions already carry deterministic **`guards`** — `{ kind: expr, expr:
  "$.context.verifierPassed == true" }` (`examples/swe-agent.yaml`). The guards on
  the transition **into** a terminal already *are* acceptance criteria — just
  anonymous, scattered per-transition, and invisible above the state machine.
- A state may be **`terminal: true`** (`runtime_links.rs:99`), but **every**
  terminal collapses to `result.status: "completed"` (`runtime_response.rs:99`).
  There is **no success/failure distinction**.
- `result.status` is otherwise an **untyped grab-bag** emitted ad hoc across the
  runtime — `started`, `executed`, `waiting_for_action`, `waiting_on_lock`,
  `completed`, `failed`, `cancelled`, `timed_out` — conflating two different axes
  (lifecycle vs resolution) in one string.

**Why this matters for agents.** An orchestrator (ADR-0007) handed only a chain of
per-state goals can satisfy each local step and still drift from the actual intent,
because nothing states *what winning looks like* or lets anyone *verify* it. It
needs a target focus (for interpretation) **and** a machine-checkable definition of
done (for measurement) — two different things.

## Decision

### 1. Ubiquitous language — mission / outcomes / goal

Three terms, deliberately in **different registers** so they don't blur in natural
language (the prior reach for "objective" failed because *objective* and *goal* are
synonyms — both mean "an aim"):

- **Mission** — the *undertaking*: one running workflow instance (the cockpit's
  `MissionView`; the runtime's workflow instance). The noun on the map.
- **Outcomes** — the *measurable definition of done*. **Plural** — a mission may
  have several; it is done when they are all met. *(new first-class concept.)*
- **Goal** — the *immediate aim of the current step* (`state.goal`, unchanged). Now
  unambiguous: always small, local, per-step — never the whole mission.

Out-loud test: *"The **mission** is to migrate the store; it's done when its three
**outcomes** are met — data migrated, tests green, old store removed; right now in
the validate step the **goal** is to dry-run and diff."* `objective` is **not** in
the language.

### 2. Outcomes — mission-level, named, deterministic

A workflow declares a top-level **`outcomes`** list. Each outcome pairs a
human-readable **`statement`** (the orchestrator's target focus / the cockpit's
checklist label) with a deterministic **`check`** that reuses the existing guard
`expr` evaluator over `$.context`:

```yaml
swe_agent:
  outcomes:
    - id: verified
      statement: The patch passes deterministic verification.
      check: "$.context.verifierPassed == true"
    - id: low-risk-or-signed-off
      statement: The change is low-risk, or a human has signed off.
      check: "$.context.risk != 'critical'"
  states: ...
```

- **Outcomes assert; guards route.** Transition `guards` decide *which path* the
  machine takes; outcomes decide *whether the mission actually succeeded*. They may
  reference the same context fields; they answer different questions.
- The response **surfaces outcomes live** — `outcomes: [{ id, statement, met:
  bool }]`, evaluated against current context every turn — so the orchestrator sees
  target focus + progress and the cockpit renders a checklist.
- **Measurement is deterministic, never LLM-judged.** "Done" is the machine
  reaching a success terminal because every outcome `check` passed — not the model
  declaring victory.

### 3. Terminals carry an outcome; status is a typed enum

- A terminal state declares **`outcome: success | failure`**:
  ```yaml
  completed: { terminal: true, outcome: success }
  aborted:   { terminal: true, outcome: failure }
  ```
- **`result.status` becomes a typed enum** (poka-yoke — exhaustive match, not a
  string grab-bag), replacing the eight ad-hoc strings:

  ```
  running | waiting | succeeded | failed
  └──── in process ────┘   └──── resolved ────┘
  ```

  - **running** — advancing on its own (an executor/agent is working).
  - **waiting** — alive but stalled on input: a human gate, a lock, or an external
    answer. This is the cockpit's "Needs You" at the mission level — the temporal,
    not-progressing state (subsumes `waiting_for_action`, `waiting_on_lock`).
  - **succeeded** — reached a `success` terminal, all outcomes met.
  - **failed** — reached a failure resolution, with a typed **`reason`**:
    **`cancelled | timed_out | guard_unmet | error`** (kept as a reason, not a peer
    status, so the enum stays at four and the cockpit badges four colors).
    `guard_unmet` = reached a `failure` terminal, or a `success` terminal whose
    outcomes did not all hold (the poka-yoke), or ran out of legal moves with
    outcomes unmet. `error` = a deterministic chain or an executor **errored** (the
    `error` slot carries the specific code, e.g. `CHAIN_FAILED` / `EXECUTOR_FAILED`)
    — distinct from a *deliberate* failure terminal. *(Amendment: `error` was added
    during implementation — a step error must resolve the last action as `failed`
    so a parent `kind: workflow` detects it instead of polling forever.)*

- Clean cutover from the flat `completed` (no deprecation shim, per project
  convention).

### 4. Poka-yoke — a success terminal must earn it

Reaching a **`success`** terminal while any outcome `check` is unmet is a
**definition error**, surfaced (not silently reported as `succeeded`). A workflow
whose success terminal is reachable without its outcomes holding is rejected at
validation. Failure terminals carry no such obligation.

## Consequences

- **Positive.** A mission has a measurable, deterministic definition of done that an
  orchestrator can target and a human can verify. The status enum stops lying
  (`completed` no longer hides failure) and collapses eight ad-hoc strings into four
  honest ones. Outcomes reuse the guard `expr` evaluator — no new expression
  language. The cockpit gets a live outcome checklist and a four-color status badge
  for free.
- **Costs.** A schema addition (`outcomes`, terminal `outcome`), a runtime-response
  change (surface outcomes, emit the typed status), new validation (success-terminal
  reachability vs outcomes; typed status/reason), and call-site churn replacing the
  eight status strings. The cockpit view-model adopts the four statuses + reason.
- **Sequencing.** This lands **before** the cockpit launch UI — you cannot launch a
  mission toward outcomes that do not yet exist in the schema. Order: (a) schema +
  validation for `outcomes` and terminal `outcome`; (b) the typed status/reason +
  live outcome surfacing in the runtime response; (c) cockpit view-model + badge +
  checklist; (d) then ADR-0007's goal-directed launch.

## Alternatives considered

- **"Objective" as the done-concept.** Rejected — synonymous with `goal`; the pair
  cannot be separated in natural language. Different registers (undertaking /
  done-check / immediate-aim) is what makes the trio sayable.
- **Binary succeeded/failed status.** Rejected — a mission spends most of its life
  unresolved; the temporal axis (`running`/`waiting`) is essential, and `waiting`
  is what the cockpit's attention queue is built on.
- **`cancelled` / `timed_out` as top-level statuses.** Rejected — they are *kinds of
  failure*, not successes; modelling them as a `reason` on `failed` keeps the enum
  small and the cockpit's color story simple.
- **Outcomes as inline transition-guard annotations.** Rejected — outcomes are
  mission-level by nature (one target block the orchestrator and cockpit read);
  scattering them across success transitions loses that and muddles routing vs
  acceptance.
- **LLM-judged definition of done.** Rejected — not measurable. Done bottoms out in
  the guard `expr` evaluator over context, reaching a designated success terminal.

## References

- [ADR-0007](0007-agents-first-class-workflow-executors.md) — the agent that drives
  a mission toward these outcomes.
- `examples/swe-agent.yaml` — the real guards/terminals this generalizes.
- `crates/praxec-core/src/runtime/runtime_response.rs`,
  `runtime_links.rs` (`is_terminal`), `validate.rs` — the touch points.
