# Dogfood findings — `flow.cohort.compiled-stack` on praxec spec #2 cohort 1

**Date:** 2026-07-17 · **Run:** praxec building praxec (spec #2 requirement-driven
resolution), cohort 1 = `resolution-config` + `named-accounts`, plan
`plan_159d10f1…`, worktree `/home/mc/wt-praxec-spec2` (branch `feat/spec2-impl`).
**Outcome:** agents wrote correct core edits; the flow ended `failed_attributed`
on the verify milestone; the frontier orchestrator hand-completed the integration
and it went green (commit `b90b918`). These are findings to fix in the next praxec
release.

## Actionable tooling findings

### F1 — Verify-milestone attribution is blind to compile errors (HIGHEST VALUE)
The flow ended `failed_attributed` with *"zero file:line findings; all three
criteria are tree-level workspace failures with no per-file granularity"* — yet
`cargo` emitted precise locations (`walk.rs:176`, `preflight.rs:283`, …). The
`verify_rust` milestone's failure parser did **not** extract compile-error
`file:line`, so attribution had nothing to pin, both deliverables scored `clear`,
and the flow dead-ended with **no remediation path**.
**Fix:** parse `error[E….]` + `--> path:line:col` (and test-failure locations)
into findings. That turns the dead-end into a recoverable loop: attribution can
point at the offending consumer files and trigger a scoped fix pass. Without this,
any cohort whose failure is a compile error is unrecoverable by the flow.

### F2 — Scope-bounded edits can't cover a public-type change's blast radius
`resolution-config` changed a public type (`ModelsFile.overrides/activity:
Vec<Binding> → Pool<Binding>`). The scope-bounded agent correctly edited only its
owned file (`config.rs`) and honestly flagged the out-of-scope parse-grammar — but
the type change breaks **consumers in other files and other crates**
(`walk.rs`, `preflight.rs`, `gateway.rs`, `affinity_resolver.rs` across
`praxec-core` + `praxec`). `test --workspace` then fails to compile.
**Fix (one of):** (a) cohort planning computes a public-type change's blast radius
and includes consumers in the deliverable's scope; (b) a "type-change" deliverable
pattern that owns the type *and* its readers; (c) on a cross-file compile failure
(needs F1), auto-widen scope / spawn a fix pass over the implicated files.
**Design lesson baked into the fix here:** a `Vec→NewType` change is
"backward-compatible for readers" only if the newtype `Deref`s to the slice +
`impl IntoIterator for &NewType` — then read-consumers need zero edits (only
genuine `&Vec`/`&NewType` unifications, e.g. one `unwrap_or`, need touching).

### F3 — Build-loop agents emit edition-2024-incompatible Rust
The agent wrote `std::env::set_var` / `remove_var` in tests **without `unsafe {}`**
— a hard error under edition 2024 (this workspace), fine in the model's
training-era edition. Also `{{members, strategy}}` (format-escaping) inside a
**plain string literal** passed to `Error::custom`, producing doubled braces in
user-facing text (its own test caught it once the suite compiled).
**Fix:** put the edition-2024 rules (unsafe env, etc.) in the coding agent's
system context for Rust stacks, and/or run `cargo fix --edition-idioms` +
`clippy --fix` as a cheap post-edit pass before the verify milestone.

## Positive findings (keep / validate)

### P1 — compiled-stack sidesteps the build-loop ceremony failure
`flow.cohort.compiled-stack` uses **edit-only** scope-bounded agents + **one**
shared verify milestone — not per-deliverable build-loop sign-off — so it did
**not** hit the `AGENT_NO_RESULT` ceremony failure
(`project_buildloop_ceremony_failure`). The failure mode shifted from "ceremony"
to "verify-attribution dead-end" (F1). Prefer compiled-stack over
`flow.execute-cohorts` for real compiled deliverables.

### P2 — the generated code was faithful and high-quality
`Effort`/`Strategy` enums, dual-form `Pool` serde (R3), the named-accounts
registry with load-time fail-fast — all matched the spec, with real TDD tests. The
agent respected scope boundaries and flagged out-of-scope work honestly. The
issues were **integration/edition**, not logic — which is exactly what a better
verify-attribution + blast-radius model (F1/F2) would let the flow self-heal.

## Related plan/scope gap (spec #2, not tooling)
The `resolution-config` deliverable I scoped owned only `config.rs`, but the
`ModelRef` **parse-grammar extension** (`affinity|tier|effort`, unknown 3rd
segment = error, R9) lives in `walk.rs` — so the `Effort` enum exists but is
**not yet wired into `ModelRef` parsing**. Fold this into the `pool-resolver`
deliverable (it already touches resolution) or add a small follow-up deliverable.
