# ADR-0009: The interaction bus, the orchestrator, and the mediator

**Status:** Accepted

**Date:** 2026-06-11

## Context

[ADR-0007](0007-agents-first-class-workflow-executors.md) gave us the **agent** —
an ephemeral, harness-bound, 1:1 execution vehicle for a workflow.
[ADR-0008](0008-missions-outcomes-and-resolution-status.md) gave a **mission**
measurable **outcomes** and a typed resolution status including **`waiting`**. Two
things were still unsettled, and surfaced in design:

1. **Who drives a mission to its outcomes, and where does that run?** ADR-0007's
   `orchestrator:` field names a driver, but nothing acts on it (the
   orchestrator-not-runtime-wired gap). The driver **cannot live in the cockpit** —
   missions must run **headless** (CI, cron, a server, an MCP client). And the
   cockpit's LLM is the *cockpit's* conductor (navigation, authoring, audit), **not
   a workflow executor** — conflating the two is wrong.
2. **How does a running mission interact with the human** — kickoff, HITL
   approvals, clarifying conversations — **without yanking the human between
   missions?** (SPEC §29.7's "live multi-turn human↔agent dialogue" is deferred.)

Grounding (designed against what already exists):

- An orchestrator is **not fire-and-forget**: it **parks** when it needs the human
  and resumes — which *is* an ADR-0008 **`waiting`** mission.
- The cockpit already streams model output over a tokio **`mpsc`** channel
  (`cockpit/main.rs`) — the bus pattern is established, just not generalized.
- The ADR-0007 runner goes through the llm-executor **`ProviderFactory`**
  (mockable) — so a headless driver built on it is **testable without a live LLM**.
  The cockpit's `agent.rs` is *not* mockable — another reason execution doesn't
  belong there.

**Framing principle.** **Separate execution from interaction.** Execution is
headless and lives in the harness; interaction is a swappable front-end (a human
via the cockpit, or a policy in headless). **One bus** connects them.

## Decision

### 1. Ubiquitous language

Named by what each does — two distinct verbs, no blur:

| Term | What it is | Verb |
|---|---|---|
| **Workflow** | the program — states + transitions + **outcomes** | — |
| **Mission** | a running instance of a workflow | — |
| **Outcomes** | the measurable definition of done | — |
| **Goal** | the tactical aim of the current step | — |
| **Orchestrator** | the actor that drives one mission to its outcomes | **orchestrates** |
| **Mediator** | the actor that bridges the human and all running orchestrators | **mediates** |
| **Bus** | the channel fabric they communicate over (tokio) | — |
| **Agent** | the execution substrate both run on (ADR-0007) | — |

Out-loud test: *"You talk to the **mediator**. When you start a **mission**, it
spins up an **orchestrator** to run it — one per mission. The orchestrator
**orchestrates** the **workflow**: drives the state machine toward the mission's
**outcomes**, working the **goal** at each step. When it needs you it parks on the
**bus** and waits. The mediator is listening across every mission; it **mediates**:
gathers those requests, groups them into themes, and brings them to you together —
so you decide related things in one context instead of bouncing between missions.
Your answers flow back over the bus and the orchestrators resume."*

### 2. The orchestrator — the execution actor

An **agent** (ADR-0007) driving **exactly one mission** toward its outcomes (1:1;
composition spawns a **tree**, each child mission its own orchestrator — never one
actor multiplexing many). It is:

- **Headless.** It lives in the harness, acts only through the §32 surface
  (`praxec.query`/`command` — a governed principal), and reuses the ADR-0007
  runner. No UI dependency.
- **Long-lived.** It **parks on the bus** when it needs the human (→ `waiting`) and
  resumes on reply. Its stop condition is the **outcomes** (resolve when met), plus
  step/budget bounds.

### 3. The mediator — the interaction actor

Bridges the human and **all** running orchestrators. **Executes nothing.** This is
the cockpit's conductor LLM. Its job is **attention management**: collect the
interaction requests coming off the bus across every mission, **theme** them, and
present them so the human stays in **one cognitive context** (no per-mission
context-switching), then route replies back. It is **one consumer** of the bus; a
headless run swaps a different consumer (an auto-answer policy, or park-and-fail).

### 4. The bus — tokio channels (no framework)

The actor model, on `tokio::sync`:

- **`broadcast` / `mpsc`** — stream events + model chunks from an orchestrator to
  subscribers (the cockpit is one).
- **`oneshot`** — the **HITL park/resume**: an orchestrator sends `(request,
  reply_tx)` and `await`s `reply_rx` — *parked* until answered.
- **`watch`** — the latest mission status, for fleet observers.

A **hub** in the harness is where orchestrators publish and consumers subscribe.
tokio *is* the bus — no actor framework.

### 5. HITL park/resume contract — un-defers §29.7

At a human-gated point an orchestrator emits a typed **Interaction** request
(`approve | answer | form | discuss` — the existing Hitl kinds) with a `oneshot`
reply onto the bus, the mission goes **`waiting`**, and the orchestrator awaits. A
consumer answers (the **mediator** for a human; a **policy** for headless); the
mission **resumes**. The human↔agent dialogue *is* bus traffic — SPEC §29.7
realized.

### 6. Naming cleanup

`orchestrator` now means the **actor**. The flow-tier **program** (today's
`orchestrator` in V8/V9/V11 and the `ORCHESTRATOR_HAS_*` codes — *"orchestrators
are not externally invokable"*, *"an orchestrator does not invoke an
orchestrator"*) becomes a **flow** (it already carries the `flow.` prefix). Rename
those rules/codes; clean cutover (no alias).

## Consequences

- **Positive.** Execution and interaction are cleanly split; **headless and
  interactive share one path** (swap the bus consumer); §29.7 dialogue becomes bus
  traffic; `waiting` gets a precise mechanism (parked on a `oneshot`); the
  orchestrator seam from ADR-0007/0008 is finally closed; the cockpit stays a pure
  observer + mediator. No new dependencies — tokio is the bus.
- **Costs.** A hub + the bus contract; the headless orchestrator driver (on the
  ADR-0007 runner); the mediator's theming logic; the typed Interaction
  park/resume; the `orchestrator`→`flow` rename. Running an orchestrator
  end-to-end needs real LLM keys (the loop is testable with a scripted
  `ProviderFactory`).
- **Sequencing.** (a) naming cleanup (`orchestrator`→`flow`) + surface the
  **orchestrator binding** in the gateway response (so the mediator can show
  "driven by X"); (b) the **bus + HITL park/resume** contract (the foundation);
  (c) the **headless orchestrator driver** on the ADR-0007 runner + a headless
  `orchestrate` command (fully testable); (d) the **mediator** as a bus consumer in
  the cockpit (theming the Needs-You queue); (e) optional config-gated **auto-drive
  on start**.

## Alternatives considered

- **Cockpit-hosted execution.** Rejected — not headless; conflates the cockpit's UX
  LLM with workflow execution.
- **One orchestrator multiplexing many missions.** Rejected — breaks ADR-0007's
  1:1; composition (a tree of orchestrators) gives "many" without multiplexing.
- **"Meta-orchestrator" for the human bridge.** Rejected — it doesn't orchestrate
  (execute); it mediates. **Mediator** is the honest verb.
- **An actor framework (actix / ractor / kameo).** Rejected — `tokio`
  `mpsc`/`oneshot`/`broadcast`/`watch` are the bus; no framework needed.
- **Keeping `orchestrator` for the flow-tier program.** Rejected — the executing
  actor is the central, future-facing concept and should own the name.

## Status update (2026-07)

Decision §4 enumerated four tokio channel types (`broadcast`, `mpsc`, `oneshot`,
`watch`). As built, the bus (`crates/praxec-core/src/bus.rs`) uses **only
`broadcast` + `oneshot`**: `broadcast` fans every `MissionEvent` out to all
subscribers (it subsumed the streaming role §4 sketched for `mpsc`), and
`oneshot` is the HITL park/resume reply channel. `mpsc` and `watch` were **not
needed** — `mpsc` was superseded by the cloning `broadcast` fan-out, and no
`watch`-backed latest-status channel was required (fleet status is derived
elsewhere). The two-channel shape is the frozen contract.

## References

- [ADR-0007](0007-agents-first-class-workflow-executors.md) — the orchestrator is
  an agent; the mockable runner the headless driver reuses.
- [ADR-0008](0008-missions-outcomes-and-resolution-status.md) — `waiting` = a
  parked orchestrator; outcomes = the stop condition.
- SPEC §29 / §29.7 — HITL kinds; the deferred human↔agent dialogue the bus
  un-defers.
- `crates/praxec-cockpit/src/main.rs` — the `mpsc` streaming pattern the bus
  generalizes.
