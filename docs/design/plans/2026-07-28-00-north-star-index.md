# North-Star Plan Index — Evidence-Gated Boundaries + the file-mutation probe

**Design of record:** `docs/design/2026-07-28-change-building-block-design.md` (vetted, aligned).
**Method:** design big, build small. This index is the *big* — the complete, ordered plan-set we
drive toward. Each plan is *small* — a standalone, testable, subagent-buildable unit. No code lands
until the plan-set below is nailed; then subagent-driven execution proceeds plan-by-plan,
task-by-task, with review gates.

## The north star (one paragraph)
The atom is **L1: evidence-gated boundaries + typed, checkable handoffs**, applied to *every*
workflow. A step is refused if its inputs don't resolve to real truth (entry), a loop is refused if
it re-runs on unchanged information (continuation), and a success needs evidence (exit — shipped).
Handoffs are **acceptance criteria** (a checkable definition-of-done, extending ADR-0008
`outcomes`), which unify the handoff with verification. The mutation micro-waterfall is **one of
five block-shapes**, and the deterministic file-apply is a **separate MCP/CLI tool** (a probe,
A/B-gated), not an engine `kind`. The engine stays lean: it gains only the generic gates + validators.

## Definition of done for the north star
1. **L1 shipped** in the engine (entry + continuation + exit gates, fallback-ledger/telemetry,
   admissibility validator, acceptance-criteria handoff) — all shadow-mode-capable, non-retroactive.
2. **The three incidents structurally can't recur** (entry kills name-as-path/silent-fallback;
   continuation kills livelock; exit kills narrated success).
3. **The apply-strategy probe measured** against the free-form `kind: agent` baseline (the A/B
   stop-gate). If it wins → extend the op-vocabulary; if not → keep L1 + `kind: agent`, drop L3.

## The plans (order = build order)

| # | Plan | Home | Ships / Probe | Depends on | Kills / Delivers |
|---|---|---|---|---|---|
| **A** | Entry gate (fallible render + shadow dispatch guard) | engine `praxec-core`/`praxec-agents` | Ship | — | name-as-path, silent-fallback (incidents 2 + partial) |
| **B** | Continuation delta-gate (no-retry-without-new-info) | engine `praxec-core` (+ promote `reads.rs` from `praxec-test`) | Ship | A (shares anomaly/telemetry shape) | livelock (incident 3); subsumes the anti-livelock candidate |
| **C** | Fallback-ledger + `cost report` anomaly column + uniform gate telemetry | engine `praxec-core` | Ship | A, B (they emit into it) | observability; the shadow-mode substrate; silent-fallback surfacing |
| **D** | Admissibility validator + `RunCommand`-rejection (load-time) + external-effect L1 rule (read-back + dedup precondition, park-default) | engine `praxec-core` `validate.rs` + packs | Ship | — | authoring poka-yoke; the classifier that keeps op-vocabularies honest |
| **E** | Acceptance-criteria handoff — extend `outcomes` so a step emits the criteria the next must meet; entry gate validates criteria are *checkable*; continuation measures progress vs unmet criteria | engine `praxec-core` `runtime_response.rs`/`guards.rs` | Ship | A, B (the gates consume criteria) | the general, declarative handoff (all 5 shapes); unifies handoff + verify; findings/decision recognizer |
| **F** | Apply-strategy **tool** (`Delete`/`Move`/`Create`/`Modify` + per-op conformance + effect-scope) **+** a change micro-workflow that uses it, dogfooded vs the free-form baseline; prerequisite: fix `promotion.rs` Conflict-leaves-dirty-tree | a **separate MCP/CLI tool** + **cognitive-architectures** pack | **Probe** (A/B-gated) | A–E (the gates + criteria govern it); the promotion fix | tests "system-does-the-writing beats free-form"; narrated-write structurally impossible for these ops |

## Sequencing logic
- **A → B → C** are the L1 gate trio + their observability. A and B are the direct incident fixes; C
  makes both safe to roll out (shadow-mode counting) and answerable in production. Build first.
- **D** is independent (a load-time validator) and can be built in parallel; it is the cheap
  authoring poka-yoke that keeps any future op-vocabulary (E's criteria, F's ops) honest.
- **E** generalizes the handoff to acceptance criteria; it *consumes* A + B (the gates now validate
  and measure against criteria) and is the piece that makes the paradigm work beyond code (triage/
  review/elicit). Build after the gates exist.
- **F** is the A/B-gated bet, in its own tool + pack, gated on the `promotion.rs` fix and on A–E
  being in place. Build last; its go/no-go on further op-vocabulary is the stop-gate.

## Guardrails carried into every plan (from the vetting)
- **Shadow-mode first** for anything with wide blast radius (esp. A's render gate); enforcement is a
  flag; rollback is the flag.
- **No deterministic oracle for intent/data-safety** → conforming-but-wrong, semantic-rename, and
  migration-data-safety are handled by **label `behaviorally-unverified` → human** + diff-coverage
  instrumentation, never by a false green. `Migration`/`Codemod` are unconditionally human-gated.
- **Engine stays lean:** generic gates/validators only. Workflow shapes → packs; deterministic apply
  → a tool. Nothing change-specific enters the spine.
- **Non-breaking + reuse:** additive functions, reuse `AuditSink`/`permanent`/`FailureClass`/
  `outcomes`; no parallel abstractions without a migration+removal plan.

## Status
- [x] Plan A — entry gate (`2026-07-28-entry-gate-fallible-render.md`)
- [ ] Plan B — continuation delta-gate
- [ ] Plan C — fallback-ledger + telemetry
- [ ] Plan D — admissibility validator + external-effect rule
- [ ] Plan E — acceptance-criteria handoff (outcomes extension)
- [ ] Plan F — apply-strategy tool + change workflow (A/B probe)

Execution mode chosen: **subagent-driven** (fresh subagent per task, review between tasks). Begins
once the plan-set above is complete.
