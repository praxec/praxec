# `smoke-ete` — End-to-End Smoke Workflow

A single workflow that composes every v0.4 praxec primitive. Used
as the ETE-readiness gate: if this walks clean, the wiring is correct.

## What's exercised

| Primitive | Where |
|---|---|
| `kind: parallel` with aggregator | `scan.fan_out` — fans over `$.workflow.input.queries`, filters via `where:`, joins via `aggregator: {kind: expression}` |
| `kind: pipeline` with `on_step_failure: continue` | `verify.verify_all` — sequential two-step pipeline |
| `enable_human_ask: true` (auto-injected `ask_human`) | workflow-level; ask_human appears on every non-terminal state |
| `human_ask_cap: 2` | bounds the auto-injected ask_human fire count |
| `path_allowlist` slot constraint | `validated_paths` slot — rejects writes outside `allowed/**` or `tests/**`, blocks `allowed/legacy/**` even within allowed scope |
| State-local slot (`scope: state`) | `scan.scan_attempts` — scrubbed on exit |
| Deterministic chaining | `scan → verify → validate_paths → ship` auto-chains |
| Lightweight transition audit | injected `ask_human` is `lightweight: true` — emits `workflow.interaction`, not `.transition` |

## Composition smoke (no API key)

In-process runtime + real executor registry. Verifies the primitives
compose without needing a live model.

```bash
./examples/smoke-ete/walk.sh
```

Runs the `cargo test -p praxec-executors --test ete_smoke` battery (3 tests):

1. **`smoke_ete_walks_to_ship_via_v04_primitives`** — drives the workflow
   to terminal `ship`, asserts `parallel.fanout.completed` and
   `pipeline.completed` events fired.
2. **`smoke_ete_path_allowlist_rejects_disallowed_path`** — probes the
   constraint evaluator with a disallowed path; asserts a precise
   `SLOT_CONSTRAINT_VIOLATED` naming the offending element.
3. **`smoke_ete_enable_human_ask_injected_into_states`** — asserts the
   `ask_human` transition appears on every non-terminal state AND
   nowhere else (terminal `ship` is correctly excluded).

CI-runnable. ~30 seconds.

## Smoke = sanity, not coverage

The smoke workflow is intentionally small. It proves the v0.4 primitives
compose. It does NOT exercise the full feature matrix (e.g. `while:`
loops, `subset_of` constraints, `kind: script` executors, `git+https`
URIs). Those are covered by their dedicated test files:

| Feature | Dedicated test |
|---|---|
| `while:` loop | `crates/praxec-core/tests/state_while_loop.rs` (covered by SPEC §26 tests) |
| `subset_of` constraint | `crates/praxec-core/tests/slot_constraints.rs` |
| `script` executor | `crates/praxec-executors/tests/script.rs` |
| `git+https://` URIs | `crates/praxec-core/tests/script_validation.rs` |
| Parallel join conditions | `crates/praxec-executors/tests/parallel_executor.rs` (29 tests) |
| Pipeline | `crates/praxec-executors/tests/pipeline_executor.rs` (7 tests) |
| HITL auto-injection | `crates/praxec-core/tests/hitl_interaction.rs` |
| Fire-cap enforcement | `crates/praxec-core/tests/fire_cap.rs` |
