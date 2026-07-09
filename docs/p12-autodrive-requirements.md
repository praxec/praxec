# P12 — Auto-drive hardening: requirements

**Deliverable:** P12 (SRC/code) — `crates/praxec-agents/src/orchestrator.rs`, `crates/praxec-core/src/bus.rs`.
**Traceability:** feedback #2 (de-solutioned) + user directive 2026-07-09 (conversational-refinement).
**Why:** the thesis enabler. "Deterministic control + commodity execution ≈ frontier-model result at a
fraction of the cost" is won or lost on how reliably — and how *cheaply* — praxec drives an `actor: agent`
hop to a contract-satisfying result. Requirements are EARS-form; each has an ID for verification traceability.

## Scope

Two problems, one seam:
1. **Reliability** — an agent hop must converge or fail cleanly (evidence-fed, bounded, HITL-clean, resumable),
   never hang or loop unboundedly.
2. **Efficiency (minimal cost)** — a hop that produces *close-but-incomplete* output must not throw the work
   away and re-derive from scratch. It must climb the **cheapest rung that closes the gap**.

## R1 — Reliability envelope (the existing P12 requirement)

- **R1.1** (ubiquitous) The auto-drive system shall inject the prior step's structured failure evidence
  (e.g. `verifyOut.findings`, `build_issues`, `qa_findings`) into the agent hop's context before each attempt.
- **R1.2** (event-driven) When an agent hop is retried, the system shall enforce a hard attempt bound
  (`max_attempts`, default configurable) and shall not exceed it.
- **R1.3** (unwanted) If an agent hop reaches its attempt/time bound without satisfying its contract, then the
  system shall transition to an explicit human-in-the-loop (HITL) state carrying the accumulated evidence —
  never silently proceed and never loop.
- **R1.4** (state-driven) While an agent hop is stalled (no progress within a watchdog interval), the system
  shall be able to reclassify, kill, and resume it from the last durable checkpoint (resumability).

## R2 — Conversational-refinement executor mode (NEW)

The core efficiency requirement: **retain the model conversation across a contract-miss; obtain only the
missing/corrected piece; stop when the contract is met.**

- **R2.1** (ubiquitous) The agent executor shall support a *conversational-refinement* mode in which a hop's
  underlying model conversation is retained across attempts within that hop (as opposed to a stateless
  re-invocation that re-derives the whole output).
- **R2.2** (event-driven) When a hop's output fails its snippet/output contract in a way that is *repairable by
  the model* (a missing or malformed field, not a wholesale failure), the system shall send a **targeted
  follow-up** naming exactly the unmet contract obligation (field, type, constraint) into the retained
  conversation, and shall accept the model's delta.
- **R2.3** (ubiquitous) The follow-up prompt shall carry only the delta request plus the specific contract
  violation — not a re-statement of the full task — so refinement cost scales with the gap, not the task.
- **R2.4** (unwanted) If the retained conversation exceeds a token/turn bound, then the system shall stop
  refining and escalate per the ladder (R3) — a growing conversation must not become an unbounded cost sink.

## R3 — Contract-miss ladder (cheapest rung first)

On any agent-hop output that fails its contract, the system shall attempt recovery in this fixed order,
advancing to the next rung only when the current one cannot close the gap:

- **R3.1 — Deterministic repair (no model call).** (event-driven) When a contract miss is mechanically
  repairable (e.g. `null → []`, whitespace/shape normalization, defaulting an absent optional), the system
  shall repair it deterministically and re-validate, with **zero** model invocation.
- **R3.2 — Conversational gap-fill (retained context).** (event-driven) When R3.1 cannot repair it but the
  output is close-but-incomplete, the system shall apply R2 (targeted follow-up in the retained conversation).
- **R3.3 — Decompose the step (micro-waterfall).** (event-driven) When R3.2 fails to converge within its
  bound — i.e. the model cannot do the whole step — the system shall surface the step for decomposition into
  smaller sub-steps rather than continue conversing.
- **R3.4 — Escalate tier / HITL (last resort).** (unwanted) If R3.1–R3.3 all fail to close the gap within
  their bounds, then the system shall escalate model tier and/or hand off to HITL with the full accumulated
  evidence.

**Invariant** (ubiquitous): The system shall always prefer the cheapest rung that closes the gap, and shall
record which rung resolved (or failed) each contract miss, so the cost profile is observable (feeds P14/P8).

## Acceptance criteria

1. A hop whose model emits a repairable-by-default violation (e.g. `artifacts: null`) is resolved by R3.1 with
   no additional model call. *(This is exactly dogfood finding #2.)*
2. A hop whose model omits one required field, given a targeted follow-up, returns the field via a delta on the
   **retained** conversation (verified: the follow-up token count << the original prompt token count).
3. Refinement halts at the configured turn/token bound and advances to R3.3/R3.4 rather than looping.
4. Reaching the outer attempt bound lands in an explicit HITL state carrying accumulated evidence — never a
   silent proceed, never an unbounded loop.
5. Each contract miss records the resolving rung (deterministic | gap-fill | decompose | escalate), queryable
   as cost/telemetry evidence.
6. A stalled hop can be killed and resumed from its last durable checkpoint.
