# Design: Evidence-Gated Workflow Boundaries (and a narrow file-mutation probe)

Status: DRAFT, **post-FMECA-vetting revision**. Phase-1 (L1 gates) recommended for implementation;
L3 interpreter recommended as an A/B-gated probe; everything above rung-0/3 is a data-gated roadmap.
Date: 2026-07-28. Vetted by three independent FMECA/poka-yoke/TRIZ reviews (see Appendix A).

> **What changed after vetting.** The first draft led with a "change building block" (a
> plan→execute→verify micro-waterfall) as *the core*, plus a large `ImplementationStrategy`
> interpreter. The vetting — grounded in the real code and the ~28 shipped orchestrator flows —
> demoted both: the **atom is L1** (evidence-gated boundaries + typed handoffs), which is universal
> and largely already shipped; the mutation waterfall is **one of five block-shapes**; the
> `kind: change` interpreter is a **narrow, A/B-gated file optimization**, not essential (the three
> incidents are all killed by L1 alone). This revision reflects that.

## 1. Context & problem

glm-5.2 reported success, burned ~$1.88/253s, wrote zero files. We shipped a runtime forcing
function (`requires_file_write`); it works. Dogfood then surfaced the same shape twice more: a
surface **name** reached a coder where **paths** were expected (→ it could only STOP), and a verify
step discarded the real build error so the retry loop fed the agent nothing to fix (→ 434s
livelock). The disease is one thing: **an agent is handed something that isn't usable truth, drives
its own execution, and the system burns silently.** We patched the instances (cognitive-
architectures#65). This design targets the class.

### Failure classes
1. Narrated success *(shipped forcing function)*.
2. Semantic-string confusion (a value with the wrong *meaning* in a slot).
3. Context starvation → livelock.
4. Silent fallback (empty/ungrounded value silently becomes a default).
5. Feedback-loop starvation (retry with no new information).

**All three incidents are killed by L1 (§2) alone — no interpreter required.** That is the central
finding of the vetting and it shapes the whole design.

## 2. The atom: L1 — evidence-gated boundaries + typed handoffs

The shipped forcing function gates the **exit** boundary (success needs evidence). L1 applies the
same principle at **entry** and **continuation**, and promotes handoffs from stringly-typed
blackboard slots to **validated typed artifacts**. This is the atom of *every* workflow, not just
code:

- **Entry gate** — no agent is dispatched on inputs that don't provably resolve to real referents.
  Mechanism: make `render_template` **fallible** (today it silently emits `(x: unset)` stubs —
  `templating.rs:90,101,117`) and add non-empty-consume, **only on `required` bindings** (`$optional`
  exempt — V29/V38 precedent, non-retroactive). Kills classes 2 & 4 pre-dispatch, zero tokens.
- **Continuation gate** — a retry is illegal unless its evidence slot changed since the last
  iteration (hash the statically-derived read-slice via `reads.rs`, minus counters; infra-transient
  retries exempt via `FailureClass::is_infrastructure`). Kills class 5 + livelock. Subsumes the
  "anti-livelock guard" candidate.
- **Exit gate** — success needs evidence *(shipped)*.
- **Fallback-ledger invariant** — every fallback/degrade path emits a typed `Anomaly` event; silence
  becomes a validation error; `cost report` gets an anomaly column. This is also the **shadow-mode
  substrate**: the entry gate ships counting would-be-refusals *before* it enforces.

L1 reuses the existing spine (`templating.rs`, `reads.rs`, evidence-guards-fail-closed, `$optional`,
snapshot-versions). It is the real MVP.

## 3. The five block-shapes (L1 is the atom; mutation is one shape)

Grounded in the real orchestrators, workflows compose from L1 into five shapes. Mutation is **one**:

| Shape | Real exemplars | Governance (all L1) | Mutating? |
|---|---|---|---|
| **gather-decide-route** | `flow.triage-issue` | closed-label-set + grounded + route resolves | No (may end in an effect) |
| **gather-judge-aggregate** | `audit-docs`, `review.docs-fmeca` | typed findings + code-computed rollup (`drift_blocking==0`) | No |
| **elicit-design-vet** | `loom` deriving, `derisk`, `sebok` | resolve-refs + **human + FMECA** | No |
| **external-effect** | `check-in`, `intent.propose_and_park` | do-effect → read-back **or** park-unverified | External |
| **mutate-verify** (was "the" block) | `implement.deliverable`, `refactor.god-file` | plan→execute→verify micro-waterfall | Filesystem |

Across ~28 flows the non-mutating + external + elicit shapes **outnumber** mutate-verify — so "THE
core building block" was over-claimed. Each mutation flow is itself a *molecule of L1 handoffs*
(e.g. `qa.promote-finding` = the TDD RED→GREEN molecule: write→prove-red→pin→fix→verify→flake-scan).

### 3.1 Handoff artifacts have two recognizers, not one
A handoff is a validated typed artifact, consumer-checked **at the consuming block's entry**
(post-predecessor apply; ground-truth = current tree ∪ pending-creates-from-earlier-ops — so a
forward-referenced file doesn't false-refuse). Two recognizer families:
- **file-plan** (mutation): paths resolve (Modify/Delete/Move) **or** match project path convention
  (Create — extension/source-root/naming, *not* just non-empty, or name-as-path survives on Create).
- **findings/decision** (triage/audit/review): closed-label-set membership + citation-grounding +
  code-computed aggregate.

Net-new content (an elicitation spec) has **no ground-truth referent** — grounding validates only
*cited-existing* entities; the operative gate is **human + FMECA** (`loom.deriving` parks;
`plan_qa` runs `fmeca-converge`). The design does not claim to certify a spec.

### 3.2 External effects are an L1 step-level rule (not an L3 op)
Legitimate external actions live at L1 as state-emitting `kind: script`/`tool_source` steps + a
downstream gate — exactly what `flow.check-in` does (`run.git.push-pr` emits `{ok, pr_url, pr_number}`;
the flow gates on `ok`; conflict uses a *local trial merge* because GitHub's async `mergeable` is
non-deterministic). The **external-effect admissibility rule**:
- **read-back postcondition** (a deterministic state predicate: PR #N exists; row present), AND
- **check-before-effect precondition (dedup)** whenever the effect isn't natively idempotent (Slack
  post, row insert) — *this precondition was missing in the draft and is required*;
- **default arm = park-labeled-unverified** (the real packs park far more than they auto-verify:
  `frontrails-campaign` defers PR-open to a human/CI; `intent.propose_and_park` parks every proposal).

The **`RunCommand` rejection is an L3-op-vocabulary rejection, not a ban on external actions** —
praxec posts to Slack / opens PRs today via L1 state-emitting steps. (One sentence the draft omitted.)

## 4. L2 — the mutate-verify shape (one of five)

A filesystem-mutating step is the plan→execute→verify micro-waterfall. **Deterministic-by-default
stages**: when the file-set is known (CPM `owned_files`), the plan stage is a deterministic
projection, not a model call (prevents the historical ceremony-failure). TDD is a molecule (RED
block: verify = "test exists and fails for the intended reason"; GREEN block: verify = "that test
flips red→green, nothing regresses"; the inter-block handoff `a proven-failing test` makes
red-before-green a contract). **This is a shape, not the atom.**

## 5. L3 — the `kind: change` interpreter (a narrow, A/B-gated probe)

A separate bet: *if the system does the writing, narrated-write becomes structurally impossible.*
Plausible, but **unproven against the shipped `kind: agent` + forcing function**, and §6 concedes
constrained may be *worse* for some changes. So it is built as a **probe**, not a co-equal
deliverable. A `ChangeExecutor` (`kind: change`, peer to `kind: agent`) is a deterministic
interpreter of `Vec<ImplementationStrategy>`, plugged into the existing `promotion.rs::run_trusted_
agent` `edit: FnOnce(PathBuf)->Fut` seam. The model is invoked at exactly one site per generative
slot, returns a typed value only (no fs tools, no path, no "done"), validated before the system
applies it.

**Phase-1 probe scope (build only this of L3):**
- `DeleteFile`, `MoveFile` — **zero model**, pure-upside deterministic conformance.
- `CreateFile` — one CONTENT slot; path is a deterministic field checked against project convention.
- `ModifyFile` — **lift `edit_file`'s existing `0/1/n` unique-match** to a *pre-generation*
  precondition (fail-closed on `≠1`; feed the miss into the block re-plan); model returns the new
  span text only.

**Everything above is HARD-GATED on the Phase-1 A/B** (§6): `UpdateReferences` (index/LSP — the
single largest, least-proven build), `StructuredEdit`, `Split`/`Merge`, gated `Codemod`/`Migration`,
and the full recognizer type-system. Until the A/B shows constrained beats free-form on
success-rate × cost, ripples use `kind: agent` + slice-compile.

**Reuse / relabel:** reuse promotion/locks/chain-walk/cost-gate/effort (the `SlotGenerator` runs
single-shot completions through them; escalation is per-slot with a **structured diagnostic** bound
into the next fill and delta-gated, so the #65 disease can't recur per-slot). The forcing function
is **not removed** — `kind: agent` keeps it as the escape hatch *and* the A/B baseline. "Moot"
means "never fires for interpreter-driven ops," not "deleted." No cleanup is claimed.

**Prerequisite (grounded spine gap):** `promotion.rs` on `Conflict` leaves the live tree with
`git apply --3way` markers and exits non-zero with no reset. **Fix this (apply-to-staging-ref or
reset-on-conflict) BEFORE wiring `kind: change` on top** — the interpreter inherits the rollback gap.

## 6. Constrained vs unconstrained — TRIZ, with a runtime fall-through

Physical contradiction: the model should be *free* (agentic loop it's trained on → effectiveness)
AND *constrained* (deterministic frame → correctness). Resolved by separation:
- **In TIME:** unconstrained plan, constrained execute (the micro-waterfall).
- **On the slot ladder:** constrain placement/orchestration (the model's weakness); free generation
  (its strength). IFR: the model does only the irreducibly-generative part; the system does the
  chores. Constraint removes failure-prone responsibilities, not creative ones.
- **By CONDITION — a runtime ladder, not a one-shot verdict:** keep both modes; on repeated block
  no-progress (continuation gate: re-plan yields no new strategy shape), **auto-fall-through to
  `kind: agent` free-form**. §6's "constrained may lose" becomes a runtime safety net, not only an
  offline A/B.

**The A/B is a STOP-GATE, not a footnote.** If free-form matches constrained on success-rate × cost,
most of L3 is over-engineering: keep the cheap L1 gates + `kind: agent`, drop the rest.

## 7. The irreducible residuals (no oracle — be honest)

Three risk classes cannot be driven below Medium/High because **there is no deterministic oracle for
intent-correctness or data-safety**; TRIZ/poka-yoke cannot manufacture a missing oracle:
- **conforming-but-wrong edit / vacuous-green** (structural conformance + "tests pass" while the
  change is wrong, or the slice has no test exercising it);
- **semantic rename** over/under-match (esp. dynamic languages without an LSP);
- **migration data-safety** (a schema-delta postcondition ≠ row preservation).

The only honest mitigation is the design's own posture, made mandatory:
- **Label `behaviorally-unverified` and route to human — never let structural conformance
  masquerade as correctness.** Labels must be *real gates*, not advisory strings.
- **Diff-coverage instrumentation** (was the changed span exercised?) *triggers the label
  automatically* — without it, conforming-but-wrong is byte-indistinguishable from correct in the
  audit (the observability hole).
- **`Migration`/`Codemod` route to human park-approval unconditionally** (postcondition gates
  schema; human gates data) — cannot reach `Applied` without an approval-evidence artifact.
- **A calibration harness** (a corpus of known-good/known-bad edits → false-pass confusion matrix)
  must exist *before* the two-tier-verification claim is treated as proven.

## 8. Observability (detectable from the audit alone)

Uniform **gate telemetry** across all boundaries (entry/continuation/exit/structural/intent +
fallback-ledger): every gate emits pass/fail/refusal with a reason. Then: premature-completion is
structurally observable for `kind: change` (the system writes — a patch exists or doesn't);
silent-scope-escape is machine-checkable post-hoc via `observed_files ⊆ ∪effect_scope` **iff
`effect_scope` is in the audit**; policy-regression = fleet-level refusal/fallback rate; bad-edit
(conforming-but-wrong) is *only* visible via diff-coverage (§7).

## 9. Revised architecture & phasing (the vetted outcome)

**SHIP (Phase 1 — L1, cheap, universal, incident-justified, non-retroactive):**
- Entry gate (fallible render + non-empty-consume, **required-only, shadow-mode first via the
  fallback-ledger**, flag-rollback — widest blast radius, do not enforce day one).
- Continuation delta gate (infra-transient exempt).
- Fallback-ledger invariant + uniform gate telemetry.
- Admissibility rule + `RunCommand`-rejection as an authoring validator (mirrors V38/V39); the
  external-effect L1 rule (read-back + dedup precondition + park-default).
- Per-slice intent-verify with **real-error surfacing (#65) + diff-coverage + behaviorally-unverified
  label**.
- Prerequisite fix: `promotion.rs` Conflict-leaves-dirty-tree.

**PROBE (Phase 1, behind the A/B):** `kind: change` with `Delete`/`Move`/`Create`/`Modify` only;
dogfood `flow.refactor.god-file` against the free-form baseline; auto-fall-through on no-progress.

**DEMOTE / RELABEL:** "THE core building block" → "the mutate-verify shape (one of five)"; the
handoff type-system → consumer-validation on the consumed slot (defer named-type/recognizer/hash
machinery until a second consumer needs it); the slot ladder / op-families → design lenses.

**HARD-GATE on the A/B (roadmap, not Phase 1):** `UpdateReferences`, `StructuredEdit`, `Split`/`Merge`,
gated `Codemod`/`Migration`, the full recognizer type-system, `ExternalEffect` op-vocabulary.

**NON-GOALS (do not build):** fractal-recursion enforcement; full provenance value-envelopes;
auto-mining guards; forward-only state ratchet; fleet-canary infra (just `check --against-corpus`);
the untrusted-subprocess NoChanges hole (no cap uses it); parametric brands.

## 10. Final outcome
No High or Medium residual risk remains **that is mitigable** — the surviving Med/High
(conforming-but-wrong, semantic rename, migration data-safety) are *oracle-absent* and are handled
by the only honest means (label-and-route-to-human + diff-coverage + mandatory migration gate),
with severity intrinsic and probability driven low by real (not advisory) gates. The design's
complexity is brought in line with its evidence base: L1 ships (cheap, proven-adjacent), L3 is a
narrow probe, and the speculative apparatus is gated on data.

---

## Appendix A — FMECA vetting record

Three independent reviews (architecture-validity; failure-mode + calibration; generality across
workflows), grounded in `promotion.rs`, `file_tools.rs`, `executor.rs`, `rig_runner.rs`,
`templating.rs`, and the shipped orchestrator packs.

### A.1 Phase-1 component classification (consolidated)
| Component | Classification | Evidence | Disposition |
|---|---|---|---|
| Entry / Continuation / Exit gates | **Essential** | Adapted / Proven | Ship (Phase 1) |
| Fallback-ledger + gate telemetry | Essential | Adapted | Ship (Phase 1) |
| Admissibility rule + RunCommand rejection | Essential | Adapted (V38/V39) | Ship as validator |
| External-effect L1 rule (+ dedup precondition) | Essential | Adapted | Ship (Phase 1) |
| Per-slice intent-verify + diff-coverage + label | Essential | Proven (#65) + new | Ship (Phase 1) |
| Deterministic-by-default stage rule | Essential (constraint) | Proven | Keep as rule |
| Findings/decision artifact recognizer | Useful | Adapted | Ship w/ L1 |
| Micro-waterfall as "THE core block" | **Unjustified (supremacy)** | Adapted | **Demote** → one of five shapes |
| Handoff type-system (named-type/recognizer/hash) | Speculative | Speculative | **Defer** (collapse load-bearing half into entry gate) |
| `kind: change` interpreter | **Useful, not Essential** | Adapted (seam Proven) | **Probe**, A/B-gated |
| File ops Delete/Move/Create/Modify | Useful | Proven mechanism | Build (probe) |
| `UpdateReferences` (index/LSP) | **Speculative — largest build** | Speculative | **Defer hardest** |
| StructuredEdit / Split / Merge / Codemod / Migration | Speculative | Adapted | Hard-gate on A/B |
| ExternalEffect as an L3 op-family | **Mis-placed** | Speculative | **Relocate to L1 rule** |
| Slot ladder / fractal recursion | Speculative (lens) | Adapted / Speculative | Keep as lens / do not enforce |
| Dual-mode + A/B | Essential (stance) | Adapted | Make A/B a **stop-gate** |

### A.2 Top risks (residual after mitigation)
| # | Failure mode | Sev | Prob | Mitigation (poka-yoke) + observability | R-Sev | R-Prob |
|---|---|---|---|---|---|---|
| 2/14 | conforming-but-wrong / vacuous-green | H | M | label behaviorally-unverified + diff-coverage triggers it; calibration harness | M | L |
| 3 | semantic rename over/under-match | H | M | admissibility ejects grep-rename; compile+test; label for dynamic langs | M | L |
| 10 | migration data-safety slips gate | H | L | unconditional human park-approval; approval-token invariant in audit | H | L |
| 7 | name-as-path survives on `CreateFile` | M | M | Create-path convention recognizer (not just non-empty) | L | L |
| 5b | `promote` Conflict leaves dirty tree | H | L | fix before wiring kind:change; post-promote tree-clean assertion | L | L |
| 12 | fallible-render mass false-refusal | M | M | required-only + shadow-mode ledger warn→enforce + flag rollback | L | L |
| 9 | mis-routing to constrained mode | M | M | runtime auto-fall-through to kind:agent on no-progress | L | L |
| 4 | per-slot chain exhaustion | M | M | structured diagnostic bound into next fill + per-slot delta gate | L | L |
| 6 | handoff false-refuses forward-ref | M | M | validate at block entry vs tree ∪ pending-creates | L | L |

### A.3 Phase-3 systemic review
- **Calibration:** structural conformance is trustworthy for *"the op executed as specified"* only;
  over-reading it as correctness is the danger. Needs the calibration harness (A.2 #2/14) before the
  two-tier claim is proven. Intent-verify is untrustworthy without diff-coverage.
- **Observability:** premature-completion structurally observable for `kind: change` (win);
  scope-escape observable iff `effect_scope` in audit; conforming-but-wrong is the hole (needs
  diff-coverage); regressions need uniform gate telemetry.
- **Over-engineering:** confirmed for L3-as-specified; simplest viable = L1 gates + Delete/Move/
  Create/Modify; hard-gate the rest.
- **Incremental delivery / rollback:** L1 entry-gate widest blast radius → shadow-mode + required-
  only + flag. `kind: change` additive → clean, rollback = route to `kind: agent`. Fix #5b first.
  Migration/Codemod deferred + human-gated (correct).

## Appendix B — grounding
`promotion.rs` (edit-closure seam :161-184; observed_files lock :244; Conflict path :73-76 — the
rollback gap); `file_tools.rs` (`edit_file` `0/1/n` :239-248); `executor.rs` (chain-walk/breaker
:772-1019); `rig_runner.rs` (forcing-function stack); `templating.rs` (`(unset)` stub :90,101,117);
`ports.rs` (`Executor` one-method trait). Packs: `flow.triage-issue`, `flow.audit-docs`,
`flow.check-in`, `flow.intent.propose_and_park`, `flow.loom`, `flow.implement.deliverable`,
`flow.refactor.god-file`, `qa.promote-finding`.
