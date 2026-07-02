# Poka-yoke: `orchestrate` strands fully-deterministic missions ("stuck" must be unrepresentable)

**Severity:** medium (silent strand — no data loss, but the mission wedges in `running` and no
driver or human can advance it without out-of-band knowledge).
**Component:** `praxec orchestrate` (ADR-0009 agentic driver) + `get` HATEOAS link projection.
**Found:** 2026-06-29, driving `cognitive/flow.greenfield-mcp`.

## Symptom

```
praxec orchestrate --definition cognitive/flow.greenfield-mcp --input <...> \
  --model openrouter:anthropic/claude-sonnet-4-6 --policy auto-approve
# →
orchestrate: started cognitive/flow.greenfield-mcp → wf_261ab4b0...
[mission wf_261ab4b0...] status: running
Error: the agentic driver found no actionable move and gave up
       (stalled at status `running`; legal actions: []).
```

The instance is left **`running`** at state `spec_vetting`. But the engine disagrees that there is
no move:

```
query {workflowId, transition: "vet_spec"} →
  { "actor":"deterministic", "deterministic":true,
    "allowedFromCurrentState":true, "legalTransitionsNow":["vet_spec"] }
query {workflowId} → "links": []      # the move is invisible to a human poller too
approvals list      → (empty)         # no HITL request was ever enqueued
```

So a **legal, fireable transition exists**, yet the driver claims "no actionable move," exits
non-recoverably, and strands the workflow. `get.links` is `[]`, so a human inspecting the instance
also sees a dead end.

## Root cause

`orchestrate` is an **agentic** driver: it selects among `actor: agent` transitions toward the
mission's declared `outcomes`. `flow.greenfield-mcp` is a **deterministic pipeline**:

| actor          | count |
|----------------|-------|
| `deterministic`| 37    |
| `human`        | 1 (the `awaiting_signoff` signoff gate) |
| `agent`        | 0     |

With zero `actor: agent` transitions there is nothing for the agentic driver to *choose*, so it
concludes "no actionable move" — **even though the engine itself can fire `vet_spec`** (it is
deterministic and legal-now). Deterministic pipelines are meant to be advanced by the gateway's
server-side `auto_drive` (`agents.auto_drive: true`), which is a *different* driver than the
agentic `orchestrate`. Pointing `orchestrate` at a deterministic flow is the trigger.

Three failures compound:

1. **False "no move."** The driver gives up while `legalTransitionsNow` is non-empty with a
   deterministic move it is fully capable of firing.
2. **Mutate-then-strand.** It had already created the instance and auto-advanced one step
   (`eliciting --elicit/noop--> spec_vetting`) before giving up, so it leaves a `running` orphan
   rather than failing cleanly without side effects.
3. **Invisible move.** `get.links` omits deterministic legal-now transitions, so even manual rescue
   looks impossible (`links: []`).

**This is NOT a human-gate-delivery bug.** The `awaiting_signoff` (`actor: human`) gate is correctly
authored (a direct blocking transition with an `inputSchema`); it is simply never reached, and the
approvals queue is empty. The failure is upstream, in driver/definition-class matching.

## Invariant being violated

> A workflow in status `running` with a non-empty `legalTransitionsNow` MUST be advanceable by the
> driver acting on it. No (driver, definition) pair may leave a `running` instance holding a
> fireable transition that nothing will fire. **"Stuck" must be unrepresentable.**

## Poka-yoke (layered: prevent → fail-loud → surface)

1. **PREVENT — core fix: `orchestrate` fires deterministic transitions.**
   Driver loop: when no `actor: agent` move is available but `legalTransitionsNow` holds a
   transition whose actor is `deterministic` and `allowedFromCurrentState`, **fire it** (run its
   executor, capture outputs, advance), then re-evaluate. Loop until reaching (a) an `actor: agent`
   choice — its real decision point, (b) an `actor: human` gate — answer per `--policy` or
   park+surface, or (c) a terminal. **Reuse the serve-mode `auto_drive` advance path; do not fork
   the logic.** This alone makes any fully-deterministic / human-gated pipeline drivable by
   `orchestrate` and removes this stuck class.

2. **PREVENT — start guard.** When `--definition` has zero `actor: agent` transitions, either drive
   it as a deterministic pipeline (per #1) or **refuse before creating the instance** with a clear
   diagnostic ("engine-driven / human-gated pipeline — drive via serve `auto_drive` or `command`
   stepping"). Never half-run and strand.

3. **FAIL-LOUD — backstop.** A driver may never exit "gave up" while `legalTransitionsNow` is
   non-empty. If it would, that is an engine invariant violation: fire the legal move, or hard-error
   explicitly (non-zero), but never silently leave the instance `running`.

4. **SURFACE — HATEOAS.** `get.links` should include deterministic legal-now transitions (flagged
   as auto/advance), so a human polling a parked or stranded instance always sees an actionable link
   instead of `[]`. Closes the "invisible move" gap for human rescue and is adjacent to the original
   misread ("the gate isn't reaching the human").

**Minimum viable fix = #1 + #3.** #2 and #4 harden it.

## Acceptance / regression

- `orchestrate --definition <fully-deterministic flow>` drives end-to-end to `done`, or parks at the
  lone `actor: human` gate (e.g. surfacing it under `--policy decline`) — and **never** exits "no
  actionable move" while `legalTransitionsNow` is non-empty.
- Add to the `praxec fuzz` wedge/livelock invariant set: for every reachable `running` state
  with non-empty legal transitions, the active driver must advance, or return a typed gate/terminal
  — never a silent give-up.

## Workarounds until fixed

- Drive deterministic pipelines via the **serve-path `auto_drive`** (MCP `praxec.command {definitionId,…}`
  start), which fires deterministic transitions and parks at the `actor: human` gate, OR
- Step manually with `praxec command '{"workflowId":…,"expectedVersion":N,"transition":…}'`.
