# Durable await/resume: one primitive, three signal sources

**Status:** settled design (v0.0.16). Consolidates HITL, the async/poll executor (P7),
and elicitation under a single runtime primitive. Related: ADR-0009 (execution vs.
interaction layering, mediator/bus), P12 (auto_drive resumability), `docs/p12-autodrive-requirements.md`.

## Problem

A human decision (or any out-of-band signal) can gate execution at **arbitrary nesting
depth** — inside a `kind: agent` hop, inside a `kind: workflow` sub-flow, several layers
down from the top. When that happens the whole stack must **suspend durably** and later
**resume from the exact frame**, and **no intermediate layer** (sub-workflow, driving
agent, orchestrator, or a relaying LLM) may satisfy the gate itself. It must also survive
a power cycle, and it must not block the rest of the DAG.

Three features on the v0.0.16 list are the *same shape*:

| Feature | Suspend until… | Signal producer |
|---|---|---|
| **HITL approval** | a human decides | a human-authenticated actor |
| **P7 poll / async executor** | external state (CI, build) reaches a condition | a poll of external state |
| **elicitation** | a human finishes structured input | a human-finished interview (elicitation-mcp) |

All three are: *suspend the execution stack durably until an out-of-band party supplies a
required signal, then resume from the exact suspension point.* They differ only in **who
produces the signal** and **how rich it is**.

## Decision

Build **one** durable await/resume primitive with **pluggable signal sources**. HITL, P7,
and elicitation become thin adapters over it — not three separate suspend/resume
implementations that will drift.

```
await(source, correlation_id) -> Suspend        // parks the stack durably
                              -> Resume(signal)  // wakes the exact frame with the signal
```

- **source** — one of: `human_decision` (HITL) · `poll(condition, interval, timeout, backoff)` (P7) · `interview(session)` (elicitation). Extensible.
- **correlation_id** — routes a later signal back to the exact suspended frame.

The **HITL "queue" is a view**, not a subsystem: a projection over outstanding awaits whose
`source` is `human_decision`, rendered in order. No bespoke queue is added.

## The primitive — required semantics

1. **First-class suspend, not error/timeout/give-up.** `await` returns a `Suspend(awaiting, correlation_id)` that is a *normal* control-flow outcome. The orchestrator must **park + notify**, never conclude `GaveUp`.
2. **Uniform propagation across boundaries.** `Suspend` bubbles identically through the `kind: agent` executor AND the `kind: workflow` executor, so a gate five layers deep parks the whole stack the same way a top-level gate does.
3. **Durable park.** The suspended frame — including a driving agent's **conversation/history** — persists to the **sqlite** governance store (not a file; serve fail-fasts on a file store). Survives a power cycle.
4. **Correlated resume.** A signal carrying `correlation_id` wakes the exact frame: resume sub-workflow → resume parent → **resume the agent's tool-loop from the parked turn** → resume the orchestrator. (This agent-loop suspend/resume is P12 R1.4 — the load-bearing 80%, shared by all three sources.)
5. **Async, non-blocking.** Parking one await does not block the DAG. File-disjoint branches keep running; only the awaiting branch waits. Await latency is decoupled from execution throughput.

## Authenticity — a property of the signal source

The signal source defines **who may produce the signal**:

- `human_decision`: only a **human-authenticated** producer may resolve it. **No LLM in the
  chain — including the top, human-facing one when running headless — may produce it.** The
  top LLM *relays* the request to the human and the human *decides*; the resolution is bound
  to the human's identity (via the authenticated approvals channel / a signed action), not
  to whatever holds the MCP socket.
- `poll`: the producer is the deterministic condition check (no human).
- `interview`: the producer is the human via elicitation-mcp's out-of-band surface.

This puts origin-enforcement where it belongs (the source), instead of bolting a role check
onto a queue. Guard rule: **reject a `human_decision` resolution whose principal is not a
proven human**, regardless of the connection's role.

## Surfacing — two modes, one store

- **Interactive** (a human is at the top): the **mediator** (ADR-0009) drains the pending
  `human_decision` awaits and surfaces each — **with full context** (artifact + prompt +
  originating workflow + correlation) — to the human-facing top. Human decides → resume now.
- **Headless** (no human present): the awaits **accumulate durably**; a human drains them
  later via `px approvals` / a UI, bound to their identity → resume then.

Same store, same primitive, both modes — precisely because no LLM may self-resolve a
`human_decision`.

## What already exists vs. what to build

**Exists (foundation):**
- Durable sub-workflow suspend/wait across the workflow boundary.
- Auto-drive won't auto-advance `actor: human` (AgentChooser only picks `agent_actions`).
- `px approvals list/resolve/tail` — the authenticated operator channel (the `human_decision` drain).
- CMP-001 principals (`_meta`, `trust_meta_principal`) — the identity plumbing.
- **elicitation-mcp** — the proven rich instance: human-finished, append-only, session-keyed,
  resumable, out-of-band. **HITL approval is the degenerate elicitation** (a single yes/no).
- The mediator/bus with oneshot HITL park/resume (ADR-0009).

**To build (shared, load-bearing):**
- **Agent-loop suspend/resume (P12 R1.4)** — `rig_runner` must suspend-durably-and-stop on a
  `Suspend` tool result (persisting its conversation) and resume that same session later.
  Today it loops/stalls/ends. *This is the 80% and it is identical for all three sources.*
- **`Suspend(awaiting, correlation)` propagation** through both executor boundaries.
- **`drive_mission` park+notify** at a human-only state (not `GaveUp`).
- **Origin-enforced `human_decision` resolution** + full-context surfacing.

## Plan fold

- **P12** absorbs the primitive (its R1.4 resumability *is* the durable suspend/resume core).
- **HITL** is the first source: `human_decision` + the mediator surfacing view + origin
  enforcement. (Supersedes the standalone "HITL-isolation" item.)
- **P7** is re-cast as the `poll` source on the primitive — not a standalone executor kind.
  Its interval/timeout/backoff/typed-timeout land as the `poll` source's config.
- **elicitation** is the `interview` source — elicitation-mcp integrated via the same await,
  rather than a parallel park/resume path.

**Sequence:** build the primitive (P12 R1.4 suspend/resume + `Suspend` propagation) → wire
the `human_decision` source (HITL, the active need) → re-cast P7 as the `poll` source →
integrate elicitation as the `interview` source.

## Acceptance (EARS)

- The runtime SHALL provide `await(source, correlation_id)` returning a first-class `Suspend`.
- When a gate is hit at any nesting depth, the system SHALL park the entire stack durably to
  the sqlite store, including any driving agent's conversation, and SHALL resume from the
  exact frame on a correlated signal.
- While a branch is parked, the system SHALL continue executing file-disjoint branches.
- If a `human_decision` resolution's principal is not a proven human, then the system SHALL
  reject it — no LLM in the chain may resolve it.
- The system SHALL surface pending `human_decision` awaits (with full context, in order) to
  the human-facing top when interactive, and SHALL retain them durably for later human drain
  when headless.
- The `poll` and `interview` sources SHALL reuse the same suspend/resume primitive.
