# Part 3 — org migration work-list (grounded by full survey, 2026-07-28)

Authoritative classification of every /praxec-org mutation workflow against `flow.change`
(envelope `baselining→detecting_stack→build_gate→building[_ts]→measuring_impl→implemented_decision→
stack_gate→verify_{ts,rust,dotnet}→[react_gate]→done`; DoD `impl_files_changed>=1` AND
`ws_verify.status=='pass'`). Reference already-wrapped consumer: `flow.implement.deliverable`
(`changing→change_gate→marking_complete`).

## Classification legend
- **DIRECT** — clean OWN-ENVELOPE, single-stack, no cardinality/verify-contract mismatch → wrap now.
- **TRANSITIVE** — delegates to `flow.implement.deliverable` (or a flow that does) → inherits the
  atom for free once the reference wrapper is merged. No edit.
- **COMPONENT** — a build/verify LEAF that flow.change itself composes (e.g. ts-slice) → wrapping is
  circular. Never migrate.
- **NON-MUTATION / EXTERNAL-EFFECT** — no repo file write (design/report objects) or side-effect
  only (git push, GitHub/SaaS API) → out of scope by design; L1 governs via `outcomes:`.
- **HARD** — real mutation but flow.change's contract (single-stack build + cap.verify.ts/rust/dotnet)
  doesn't fit (bespoke verify, N:1 cardinality, external substrate, structural-move, missing verify).
  Needs a design decision — NOT a mechanical wrap. Candidate generalization: parameterize flow.change's
  verify step (inject a `verify_cap`) so regression-test / behavior-diff / `praxec check` verifies fit.

## CORRECTION (2026-07-29): #2/#3 are NOT mechanical
Reading the actual -max flows showed `flow.ui.optimal`/`flow.refactor.to-atomic-design` build via
`cap.implement.tdd-loop` (+ ui.optimal injects build-time craft skills) and verify via `hop_slot` —
flow.change's generic ts arm (ts-slice, no skills) would REGRESS them. They fold into the
**capability-injection generalization** (`build_cap`/`verify_cap`/`build_skills`), same as the Wave 3
verify-contract cases. See `2026-07-29-flowchange-capability-injection-design.md`. Only #1 (mini-vee)
was a real DIRECT migration; it shipped in #66.

## DIRECT migrations (do now — behavior-preserving, praxec-check-clean, reviewed)
1. **cognitive-architectures `flow.shared.mini-vee.yaml`** (rust) — replace `building`+`verifying`+
   `verify_gate` with one `changing` hop to `cognitive/flow.change` + `change_gate`; keep `sketching`
   prefix; re-expose `sketch/result/tests_added/verify_report` from flow.change outputs. Side effect:
   makes `flow.sebok.yaml` transitive. SAME repo+branch as flow.change (`feat/change-atom`).
2. **cognitive-architectures-max `flow.ui.optimal.yaml`** (ts) — replace `building`+`verifying_ui`+
   `ui_gate` with one `changing` hop. Keep design/FMECA prefix + react/adversarial/PR tail.
   CROSS-REPO: requires `cognitive/flow.change` resolvable from -max (see Wave 2 gate).
3. **cognitive-architectures-max `flow.refactor.to-atomic-design.yaml`** (ts) — replace `refactoring`+
   `verifying_ui`+`ui_verified` with one `changing` hop. Keep decomposition prefix + review tail.
   CROSS-REPO (same gate as #2).

## Waves
- **Wave 1 (now):** #1 mini-vee — same repo/branch, highest confidence.
- **Wave 2 (gated on part-2 merge):** #2 + #3. RESOLVED — cross-pack is NOT a blocker: -max composes
  against the base pack (`path: ../../cognitive-architectures`, namespace `cognitive`) and already
  references `cognitive/*` ids throughout, so `cognitive/flow.change` is referenceable from -max the
  moment flow.change exists in the base pack's loaded branch. The only gate is: flow.change (part 2)
  must be merged to cognitive-architectures dev first. Then #2/#3 are the same mini-vee-shape wrap on
  the ts stack (changing→flow.change, backstop_cwd=repo_root, stack ts, verify_scope passthrough),
  keeping each flow's design/FMECA prefix + review/PR tail; behavior-preservation review each.
- **Wave 3 (design follow-on):** the HARD cases via verify-contract generalization (below).

## TRANSITIVE (free — verify routing only, no edit)
- cognitive-architectures: `flow.execute-cohorts` (→deliverable), `flow.drive-program` (→deliverable),
  `flow.loom` (→execute-cohorts→deliverable), `flow.sebok` (→mini-vee; inherits once #1 lands).

## COMPONENT (never wrap — they are flow.change's leaves)
- cognitive-architectures: `flow.implement.ts-slice`, `flow.implement.dotnet-slice`.
- cognitive-architectures-max: all 11 live `cap.*` capabilities.

## NON-MUTATION / EXTERNAL-EFFECT (out of scope by design)
- NON-MUTATION: `flow.pressure-test.use-cases`, `flow.derisk`, `flow.harden.fmeca-converge`,
  `flow.audit-docs`, `flow.audit-codebase`, `flow.audit.completeness`, `flow.review.docs-fmeca`,
  `flow.qa.explore` (cog-arch); `flow.ux.discovery` (-max); `flow.vet-page`, `flow.review-page`
  (marketing).
- EXTERNAL-EFFECT: `flow.triage-issue`, `flow.onboard-tool`, `flow.check-in` (cog-arch);
  `flow.review-site` (marketing).

## HARD cases (Wave 3 — design decision, not mechanical wrap)
Grouped by why the contract doesn't fit:
- **Cardinality (do-not-migrate):** `flow.cohort.compiled-stack` (N:1 build:verify is the point),
  `flow.add-ui-feature` (dual-stack rust+ts from one build).
- **Verify ≠ cap.verify.ts/rust/dotnet:** `flow.safe-refactor` (behavior-diff), `flow.bugfix-from-
  error-log`, `flow.debug.systematic` (regression-test), `flow.fix.react-antipatterns` (runtime-
  oracle), `flow.qa.promote-finding`/`flow.qa.program` (TDD-tamper), all 6 praxec-meta flows
  (`praxec check` / config round-trip / smoke-test).
- **External substrate:** marketing `flow.optimize-page`, `flow.suggest-new-page` (page = Allumata
  SaaS Workspace; nothing local to baseline/measure).
- **Structural-move:** `flow.refactor.god-file` (StructureOS `move`, not an agentic build-loop;
  explicitly named in the program spec but needs its own design pass).
- **Missing verify today:** `flow.greenfield-mcp` (no post-build verify), `flow.ux.optimize`
  (chunk-loop, no verify gate — a wrap would ADD correctness but needs per-chunk DoD accumulation).
- **hop_slot vs explicit routing:** `flow.add-feature` (verify injected via `hop_slot:verify`).

## N/A repos
- `frontrails-praxec-pack` — no `orchestrators/`; FrontRails static-analysis config packs only.
- cognitive-architectures `workflows/*.yaml` (10 legacy/demo files) — README-labeled demo, snake_case,
  no `cognitive/` refs; not wired to anything calling flow.change.
- `marketing-architectures` on `master` w/ zero commits (uncommitted scaffold).

## Pre-existing defect flagged (independent of migration, worth a separate fix)
- praxec-meta `flow.optimize-flow` / `flow.optimize-capability`: likely missing a write step
  (`cap.install.emitted-definition`) between `emitting` and `checking` → verify may run against an
  unchanged repo.

## Part-3 DoD (honest)
"Every org mutation flow migrated OR explicitly noted N/A" (program spec) is satisfied by: DIRECT
wrapped + TRANSITIVE verified + COMPONENT/NON-MUTATION/EXTERNAL noted N/A-by-design + HARD noted
N/A-pending-verify-generalization (Wave 3 design). Blanket wrapping the HARD cases would break
behavior — that is a defect, not the goal.
