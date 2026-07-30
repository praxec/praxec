# flow.change capability-injection generalization (unlocks Wave 2 + Wave 3)

**Status:** design, for framing. Grounded in reading the actual org flows (not the survey's
optimistic classification). Supersedes the "Wave 2 = mechanical" assumption.

## The finding that forced this
Reading `cognitive-architectures-max/flow.ui.optimal` and `flow.refactor.to-atomic-design` (the
"clean Wave 2" pair) showed they are NOT mechanically wrappable:
- They **build** via `cognitive/cap.implement.tdd-loop` (`plan` + `scope_paths` → `result`), NOT
  flow.change's ts arm (`cognitive/flow.implement.ts-slice`). Different builder, different discipline.
- `flow.ui.optimal` injects **build-time craft skills** on its build state (`implement.tdd.behavioral-
  discipline`, `implement.atomic.component-design`, `implement.storybook.author`,
  `review.react.best-practices`) via `scope.skills`. flow.change carries none — a `kind: workflow`
  sub-mission does not inherit the parent state's `scope.skills`.
- They verify via `hop_slot: verify` (strict-blackboard mode), not flow.change's internal stack_gate.

Wrapping them in today's flow.change would **swap the builder and drop the craft skills** = a
regression. The behavior-preservation gate would reject it. So the survey's "mechanical" tag was wrong
for these; they belong with the Wave 3 hard cases.

## Root cause (one, shared with Wave 3)
flow.change hard-codes BOTH ends of its envelope:
- **build** = stack-routed (`ts → flow.implement.ts-slice`, else `cap.implement.build-loop`), no
  consumer choice, no skill pass-through.
- **verify** = stack-routed (`cap.verify.{ts,rust,dotnet}`), no consumer choice.

The Wave 3 hard cases fail the VERIFY assumption (bugfix/debug want `cap.verify.regression-tests`;
praxec-meta wants `verify.praxec.check`; safe-refactor wants behavior-diff). The -max flows fail the
BUILD assumption (they want `cap.implement.tdd-loop` + craft skills). Same shape: **the atom should be
parameterized on the build and verify capabilities, defaulting to today's stack-routed behavior.**

## SPIKE OUTCOME (2026-07-29) — mechanism REVISED (see spike-findings doc)
The illustrative "templated `definitionId` + dynamic `scope.skills`" mechanism is NOT viable and, more
importantly, we don't want it:
- `definitionId` is a literal registry key, load-time-validated to reference a real definition — a
  poka-yoke worth KEEPING. So capability *selection* = **static `build_mode`/`verify_mode` enum routing**
  (a `build_gate` with guarded arms, each a real validated id), NOT a templated id.
- Skills = instructions to the model; the frontier-aligned mechanism already works: thread the skill
  instruction text through `use.inputs` → child `$.workflow.input.*` → `{{ }}` in the builder's `goal:`
  (lands in the model's USER turn). No `scope.skills` propagation, no engine change.
- One real gap found: an unresolvable `$.`-path in `scope.skills` is SILENTLY dropped (not even an
  error). That's assertion A5 (red) → a small engine hardening (fail loud).
So the generalization is a **pack-level change to flow.change** (enum-routed build/verify arms + skills
threaded via use.inputs), buildable on today's primitives, plus the one A5 engine fix. The desired
contracts are pinned as `crates/praxec-core/tests/primitive_contracts.rs` (A1-A6), per
[[feedback-assert-dont-derive]] — the greens confirm buildability, A5 red is the only engine work.

## Proposed generalization — capability injection (additive, backward-compatible)
NOTE: read the mechanism through the SPIKE OUTCOME above — selection is static enum routing, skills go
via use.inputs→goal. The `build_cap`/`verify_cap` framing below is kept for intent; the IMPLEMENTATION
is enum-routed arms + input-threaded skills, not templated ids.
Add two OPTIONAL inputs to flow.change; when omitted, behavior is byte-identical to today.

```
inputs:
  # existing: deliverable, backstop_cwd, verify_scope, react_review, cargo_scope
  build_cap:   { type: string, default: "" }   # override the builder definitionId
  verify_cap:  { type: string, default: "" }   # override the verifier definitionId
  build_skills:{ type: array,  default: [] }    # scope.skills injected into the build state
```
- `build_gate`: if `build_cap != ""` → a `building_injected` state runs `kind: workflow`
  `definitionId: $.workflow.input.build_cap` with `scope.skills: $.workflow.input.build_skills` and the
  deliverable/scope threaded in; else the existing ts/default routing (UNCHANGED).
- `stack_gate`: if `verify_cap != ""` → a `verify_injected` state runs it, mapping its verdict onto
  `ws_verify` (must emit a `verifyOut`-shaped `{status}`); else existing ts/rust/dotnet routing
  (UNCHANGED).
- The honest DoD (`impl_files_changed >= 1` AND `ws_verify.status == 'pass'`) is unchanged — it keys on
  the evidence regardless of which build/verify cap produced it. That is the whole point: the DoD is
  capability-agnostic.

Open design question (needs care, not yet decided): can `scope.skills` be threaded from a flow input
into a sub-state, and do injected build/verify caps reliably emit the `result`/`verifyOut` shapes the
DoD reads? These are the two things a spike must prove before adopting.

## What this unlocks (once designed + built + dogfooded)
- **Wave 2 (-max):** `flow.ui.optimal` / `flow.refactor.to-atomic-design` wrap flow.change with
  `build_cap: cognitive/cap.implement.tdd-loop`, `build_skills: [...the craft skills...]`,
  `verify_cap: cognitive-max/cap.verify.ts` — no builder swap, no skill loss. Behavior-preserving.
- **Wave 3 verify-contract cases:** bugfix/debug/qa-promote/praxec-meta wrap with their own `verify_cap`
  (regression-tests / praxec-check / runtime-oracle). Build stays their existing cap via `build_cap`.
- Leaves genuinely-structural cases still out (cardinality: cohort N:1, add-ui-feature dual-stack;
  external substrate: marketing Allumata; structural-move: god-file). Those remain honest N/A — the
  generalization does not force them.

## Recommended path
1. **Spike** the two open questions (skill threading across the sub-mission boundary; injected-cap
   verdict shape) on a throwaway branch — cheap, decisive.
2. If the spike holds → build capability-injection into flow.change (additive; existing consumers and
   the Wave-1 mini-vee/deliverable wrappers pass their same inputs, defaults keep them byte-identical).
3. Then Wave 2 + the verify-contract Wave 3 cases become real behavior-preserving migrations, each
   praxec-check-clean + behavior-preservation reviewed (the established pattern).

## Why not just force Wave 2 now
It would regress `flow.ui.optimal`'s craft-skill build and swap both flows' builder. "Migrated" would
be a false green — the exact anti-pattern the whole change-atom program exists to prevent (a verifier
passing while the real work was lost). Honest N/A-pending-generalization is the correct state until the
atom can carry the consumer's build+verify.
```
