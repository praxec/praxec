# Program: the reusable Change Atom + DoD handoff + org migration

Goal (session): ship (1) the **handoff definition-of-done**, (2) a **reusable change-atom workflow**
in cognitive-architectures, (3) **migration of all /praxec org workflows** to use it, plus supporting
engine features. Design of record: `2026-07-28-change-building-block-design.md`; engine plan-set:
`plans/2026-07-28-00-north-star-index.md`.

## Grounding: most of the mechanism already exists (reuse, don't reinvent)
- **Part 1 (DoD handoff) = `outcomes:`** — the acceptance-criteria definition-of-done (ADR-0008:
  `statement` + deterministic `check` + live `met` flags). Already shipped and declared across ~10
  org flows (implement.deliverable, qa.promote-finding, audit-docs, review.docs-fmeca,
  harden.fmeca-converge, qa.explore/program, onboard-tool, pressure-test, audit.completeness).
- **Part 2 (reusable change atom) ≈ `flow.implement.deliverable`** — an existing reusable
  plan→execute→verify change flow (baseline→build→measure→stack-verify→mark) whose top-level
  `outcomes:` are the honest DoD (`impl_files_changed >= 1` AND `ws_verify.status == 'pass'`). It is
  the execution LEAF the cohort driver calls per deliverable. It is *specialized* (cpm-planner
  marking, stack routing, react gate) — the reusable atom is its **common core**, minus the
  consumer-specific tail.

So: consolidate the common core into a clean reusable atom, make the DoD handoff first-class, migrate
consumers, and harden with the engine L1 gates.

## Part 2 — the reusable Change Atom (`flow.change`)
The minimal, stack-agnostic, consumer-agnostic core every mutation shares:

```
inputs:
  change_spec:  { object, required }   # goal + acceptance_criteria + scope/owned_files
  repo_root:    { string, required }   # $.run.repo_root passthrough
stages (states):
  planning   → derive the change scope + the acceptance criteria (deterministic projection from
               owned_files when known; else an agent plan step producing them). Emits the DoD.
  executing  → make the change within scope (auto-drive agent + file host today; the apply-strategy
               tool later, A/B-gated). Entry-gated (L1): refuse dispatch on unresolved/empty scope.
  measuring  → deterministic file-change count from git (impl_files_changed) — no-op ⇒ failed.
  verifying  → stack-appropriate verify (cap.verify.ts|rust|dotnet) scoped to the change.
outcomes (the DoD handoff, first-class):
  - changed:  "$.context.impl_files_changed >= 1"
  - verified: "$.context.ws_verify.status == 'pass'"
  - criteria_met: every acceptance_criterion in change_spec is a checkable `met` (behavioral where
                  no deterministic oracle → label behaviorally-unverified → human, per the design's
                  no-oracle honesty).
```

`flow.change` owns *only* plan→execute→verify+DoD. Consumers wrap it:
`flow.implement.deliverable` = `flow.change` + cpm `mark_status`; `flow.refactor.god-file` =
`flow.change` (fixing) after the StructureOS move; a bugfix/feature slice = `flow.change` with its
own criteria. Extracting the core is DRY + respects "reuse existing structures, no parallel
abstraction without a migration+removal plan" (the migration IS part 3).

## Part 1 — DoD handoff, formalized
1. `outcomes:` is the handoff (exists). The change atom declares it (above); a change flow without a
   DoD outcome is the anti-pattern.
2. Engine hardening (supporting): the L1 **entry gate** (Plan A, in flight) refuses a dispatch whose
   scope/inputs didn't resolve; the **continuation gate** (Plan B) refuses a retry with no new
   information; both make the criteria trustworthy. [Plan E — one step emitting the next's criteria
   (dynamic handoff) — only if the static per-flow `outcomes:` proves insufficient; deferred.]

## Part 3 — org migration
Migrate every /praxec-org mutation flow to compose `flow.change` instead of an ad-hoc build step.
Order (each praxec-check-clean + dogfood, one PR per repo, gitflow feature→dev):
1. cognitive-architectures: refactor `flow.implement.deliverable` to wrap `flow.change`; then
   `flow.refactor.god-file`, `flow.implement.ts-slice`/`dotnet-slice` callers, `flow.cohort.compiled-
   stack`, `flow.safe-refactor`, `flow.bugfix-from-error-log`, `flow.add-feature`, `flow.debug.systematic`.
2. cognitive-architectures-max, frontrails-praxec-pack, praxec-meta, marketing-architectures: any flow
   with a mutation step → wrap `flow.change`. Non-mutating flows (triage/review/elicit) are untouched
   (L1 already governs their handoffs via `outcomes:`).
Migration invariant: each refactor is behavior-preserving at the DoD level (same outcomes met) —
verified by praxec check + a dogfood run of the migrated flow.

## Supporting engine features (workstream 1, in flight)
- **Plan A — entry gate** (fallible render + shadow-mode dispatch guard). RUNNING.
- **Plan B — continuation delta-gate** (livelock). Next.
- **Plan C — fallback-ledger/telemetry**; **Plan D — admissibility validator + external-effect rule**.
- **Plan F — apply-strategy tool** (the deterministic-execute optimization) is A/B-gated; NOT required
  for the atom or the migration (the atom uses the existing auto-drive agent executor today).

## Definition of done for the goal
1. `flow.change` authored in cognitive-architectures, praxec-check-clean, dogfooded, with the DoD
   outcomes as its handoff.
2. Every org mutation flow migrated to compose it (or explicitly noted as N/A), each praxec-check-clean.
3. The L1 entry + continuation gates shipped in the engine (Plans A, B) so the handoffs are enforced,
   not merely declared.

## Sequencing (drive order)
1. **Author `flow.change`** (part 2) grounded in `flow.implement.deliverable`; praxec check green. ← start
2. **Migrate `flow.implement.deliverable`** to wrap it (proves the atom on the highest-traffic consumer).
3. **Roll the migration** across cognitive-architectures, then the other pack repos (part 3).
4. **Engine gates** (Plans A running, B next) harden the handoff in parallel (workstream 1).
