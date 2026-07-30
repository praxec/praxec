# Mission Wall-Clock Deadline Backstop

Branch `feat/mission-deadline-backstop` (off `dev`), worktree `/home/mc/working/mcp-flowgate-deadline`.

## Why

The engine already had three defenses, each bounding a DIFFERENT thing:

| Defense | Bounds | Location |
|---|---|---|
| MCP idle timeout | one MCP call's silence | `crates/praxec-executors/src/mcp.rs:110` (`DEFAULT_IDLE_TIMEOUT_MS = 30_000`) |
| Model stall-timeout + breaker | one model call | `crates/praxec-agents/src/breaker.rs` |
| Livelock hop budget | hop COUNT, cumulative across drives | `crates/praxec-core/src/runtime/runtime_chain.rs` (`DEFAULT_LIVELOCK_HOP_BUDGET = 300`) |

None of them bound a run's TOTAL wall-clock, and none catch a single call that blocks past its own defense (a wedged browser/MCP tool with no live peer to time it out cleanly — the grounding incident: a browser flow hung ~20 minutes with no live browser). This feature adds that missing outer bound.

## Where it's enforced (and why this seam bounds the whole run, even mid-call)

The seam that owns a run's actual execution is `WorkflowRuntime::run_deterministic_chain` (`crates/praxec-core/src/runtime/runtime_chain.rs:456`) — the hop-loop that both `start()` (`runtime.rs`) and `submit()`'s `dispatch_once` (`runtime_submit.rs`) call to auto-drive deterministic transitions, guard-gated branches, and (when `auto_drive_agents` is on) agent/executor hops, including the one that can invoke a browser/MCP tool.

A new wrapper, `WorkflowRuntime::drive_chain_with_deadline` (`runtime_chain.rs:1641`), sits between the two call sites and `run_deterministic_chain`:

- Both call sites (`runtime.rs:1373` in `start()`, `runtime_submit.rs:1827` in `dispatch_once()`) now call `drive_chain_with_deadline` instead of `run_deterministic_chain` directly.
- It computes the REMAINING budget (`deadline_ms - already-spent active-drive ms`, see accounting below) and races the *entire* chain-drive call against it via `tokio::time::timeout(remaining, self.run_deterministic_chain(...))`.
- **Why this catches a call blocked mid-hop, not just at a hop boundary:** `tokio::time::timeout` races the wrapped future against a timer at the executor level — it doesn't matter what the inner future is doing (awaiting a wedged MCP tool call, a stalled model call, anything that yields to the async runtime); the timer fires and the whole `run_deterministic_chain` future — including whichever hop is currently in flight — is dropped. This is the literal answer to "fires even when blocked mid-call": the bound is on the *future*, not on a check the code has to reach.
- A second, cheaper check lives at the TOP of `run_deterministic_chain`'s loop (`runtime_chain.rs:576`, right after the existing livelock-budget check it mirrors) — this is the hop-boundary layer: it catches a mission that accumulated past its budget over MANY quick hops (each individually fast, but summing past the deadline) *before* firing another one, without needing the outer timeout to fire.

### Accounting: active-drive time only, not calendar time

The budget is **cumulative wall-clock milliseconds actually spent executing a hop** (`CHAIN_WALL_MS_TOTAL_KEY = "_chain_wall_ms_total"`, `runtime_chain.rs`), persisted in `instance.context` and bumped in the SAME read-modify-write-then-save block that already bumps `_chain_hops_total` per hop (`runtime_chain.rs:1412`) — no extra store I/O. This deliberately mirrors the existing livelock hop-counter's cross-drive persistence pattern.

This is why using `instance.started_at` (what `timeoutMs`/`check_and_apply_timeout` already uses) would have been WRONG here: that's calendar time since the mission started, and a mission legitimately parked on a human gate, a repo lock, or a sub-workflow for hours (or days, resumed later — common in this engine's HITL/elicitation flows) would false-positive-cancel on any absolute-wall-clock basis. `_chain_wall_ms_total` only grows while a hop is actually executing; a parked/waiting mission's counter is frozen, so a long-paused-then-resumed mission is unaffected — the FMECA-required backward-compatibility property.

Because the counter is per-drive-call cumulative, `drive_chain_with_deadline` recomputes `remaining = deadline_ms - wall_ms_total` fresh on every `start()`/`submit()` call, so the timeout budget correctly shrinks across resumed drives instead of resetting.

### Two failure sub-paths inside `drive_chain_with_deadline`

1. **Already exhausted** (`wall_ms_total >= deadline_ms` before even calling the chain — e.g. resumed after a prior drive spent the whole budget): builds the terminal outcome immediately, no chain call, no reload needed (the owned `instance` is still in hand).
2. **Blew it mid-call** (`tokio::time::timeout` fires): the `instance` was moved into (and dropped with) the cancelled future, so the last COMMITTED snapshot is reloaded via `self.store.load(&workflow_id)` — safe because any hop in flight when the timer fired never reached its `save_if_version`, so the reload is exactly the instance's last consistent state.

Both funnel through the shared `mission_deadline_exceeded_outcome` helper (`runtime_chain.rs:1596`), which records the `chain.deadline_exceeded` audit event and returns the new `ChainOutcome::DeadlineExceeded { partial, reason, elapsed_ms, deadline_ms }` variant.

### No-Tokio-runtime guard (found via the workspace test run, fixed before green)

`tokio::time::timeout` requires a live Tokio reactor. `crates/praxec-executors/src/dry_run.rs`'s `DryRunExecutor` (SPEC §17.3) deliberately builds and drives an isolated sub-`WorkflowRuntime` via `futures::executor::block_on` in its tests (no Tokio runtime at all) — my first pass panicked there ("there is no reactor running"). Fix: `drive_chain_with_deadline` checks `tokio::runtime::Handle::try_current().is_ok()` and runs the chain unwrapped (identical to `deadline_secs == 0`) when there is none. Every production entry point (`serve`, the CLI, `#[tokio::test]`-based tests) runs under Tokio, so this only affects the deliberately-synchronous dry-run test harness — exactly as if the feature didn't exist there.

## Typed failure path (mirrors the existing livelock-cancel path byte-for-byte in shape)

Both call sites (`runtime.rs` `start()`, `runtime_submit.rs` `dispatch_once()`) got a new `ChainOutcome::DeadlineExceeded` match arm, placed right next to the existing `ChainOutcome::Quarantined` arm it mirrors:

```rust
self.cancel(&partial.instance.id, &reason).await?;   // durable, idempotent, wakes any suspended parent
let cancelled = self.store.load(&partial.instance.id).await?;
let response = self.response(&definition, &cancelled, StatusHint::Cancelled,
    Some(json!({
        "code": "MISSION_DEADLINE_EXCEEDED",
        "message": reason,
        "elapsedMs": elapsed_ms,
        "deadlineMs": deadline_ms,
        "cancelled_reason": cancelled.cancelled_reason,
    })), &request.principal).await;
```

This gives a caller a **normal typed failure**, never a hang, never a panic:
- `response.result.status == "failed"`, `response.result.reason == "cancelled"` — same `FailReason::Cancelled` shape the existing livelock-quarantine path already uses (a later `get()` on this instance derives the SAME `StatusHint::Cancelled` from `instance.cancelled_at.is_some()` regardless of what the original response said, so this is the only self-consistent choice — the codebase's own `get()` logic forced this, not a compromise I invented).
- `response.error.code == "MISSION_DEADLINE_EXCEEDED"`, plus explicit `elapsedMs`/`deadlineMs` fields (machine-readable, not just embedded in the message string).
- The instance is durably `cancel()`-ed (frees pool leases, wakes any suspended parent, idempotent) — it can never be silently re-driven back into the burn.
- One `chain.deadline_exceeded` audit event is recorded, carrying `state`, `elapsedMs`, `deadlineMs`.

I also mirrored the existing boot-time livelock-reap logic: `reap_orphaned_runs` (`runtime.rs:954`) now ALSO cancels an instance whose `_chain_wall_ms_total` already exceeds its deadline, checked in the same spot as (and right after) the livelock reap check, before the "legitimately waiting → skip" branch — so a process crash that orphans a run already over its wall-clock budget doesn't get silently resumed into the burn by the boot-time reaper.

## Config surface

- **Per-definition override**: `missionDeadlineSecs` on the workflow definition (same style as the existing `livelockHopBudget` / `maxChainDepth`). `0` disables the backstop for that definition.
- **Runtime/gateway default**: `WorkflowRuntime::mission_deadline_secs` field, settable via the new builder `WorkflowRuntime::with_mission_deadline_secs(secs)` (mirrors `with_max_chained_llm_turns`). Defaults to `DEFAULT_MISSION_DEADLINE_SECS`.
- **Default**: `DEFAULT_MISSION_DEADLINE_SECS = 1800` (30 minutes), `runtime_chain.rs`. Rationale (documented in the doc comment on the constant): generous enough that a legitimate multi-step auto-driven agentic mission (tens of minutes of real tool/model work) is never cut short, while still bounding an indefinite hang; the budget only counts ACTIVE hop time (see accounting above), so a mission parked on a human/lock/sub-workflow wait for hours does not accrue against it.
- Resolution order: `missionDeadlineSecs` on the definition wins; else the runtime's configured default; else `1800`. `0` at either level disables it (free function `mission_deadline_secs_for`, `runtime_chain.rs`).
- **Backward-compatible**: every existing test and definition is silent on `missionDeadlineSecs`, so they all get the 1800s default — far beyond any existing test's real duration — and the full workspace suite is unaffected (see Results).

## Tests (assert-first, red→green)

New module `crates/praxec-core/src/runtime/runtime_chain.rs::mission_deadline_tests` (bottom of file), reusing the existing `runtime_with_audit`/`start`/`events_of_type` test-harness conventions from the sibling `cancellation_and_heartbeat_tests` module in the same file:

1. **`a_hop_blocked_mid_call_is_cancelled_at_the_mission_deadline`** — a `missionDeadlineSecs: 1` definition whose one transition runs a custom `SlowExecutor` that sleeps 10s (no `reliability.timeoutMs`, so nothing else could cut it off). Asserts `start()` returns in well under 5s (not the wedged 10s), with `MISSION_DEADLINE_EXCEEDED`, `result.status == "failed"`, the instance durably cancelled, and exactly one `chain.deadline_exceeded` audit event.
   - **Verified genuinely red pre-fix**: I temporarily hardcoded `deadline_secs = 0` inside `drive_chain_with_deadline` (bypassing the whole feature) and reran this test in isolation — it ran the full 10s and failed the `elapsed < 5s` assertion (`took 10.003360147s`), then I restored the real code (diffed byte-identical to before) and reran to confirm green. This is the direct evidence the test exercises the NEW mid-call cutoff, not something already true.
2. **`a_mission_already_over_budget_from_prior_drives_is_cancelled_before_the_next_hop`** — no sleeping; seeds `_chain_wall_ms_total = 999_999` directly (simulating a mission resumed after a prior drive spent the whole budget) then submits a transition. Proves the hop-boundary/cumulative-across-drives path fast and deterministically, and proves the REQUESTED transition still commits (state advances to `b`) while only the FOLLOWING auto-drive hop is refused.
3. **`a_normal_run_under_the_deadline_completes_unchanged`** — a plain 2-hop line under `missionDeadlineSecs: 1`, completes to its real terminal state, `result.status == "succeeded"`, no cancellation, no `chain.deadline_exceeded` event. Behavior-preserving regression proof.
4. **`mission_deadline_secs_zero_disables_the_backstop`** — same seeded-exhausted-counter setup as test 2, but `missionDeadlineSecs: 0`; the mission drives to completion normally, proving the opt-out.

All four pass (`cargo test -p praxec-core --lib mission_deadline_tests`, 1.01s).

## Results

- `cargo test --workspace` — **green** (exit 0). One real regression was found and fixed mid-pass: `crates/praxec-executors/tests/dry_run.rs` (3 tests) panicked on the first pass because it drives an isolated runtime via `futures::executor::block_on` with no Tokio reactor; fixed with the `Handle::try_current()` guard described above, then reconfirmed green.
- `cargo fmt` — clean (`cargo fmt -p praxec-core -- --check` produced no diff before the explicit `cargo fmt` run).
- `cargo clippy --workspace --all-targets -- -D warnings` — clean (files `touch`ed first per the incremental-cache caveat). Only an unrelated upstream `future-incompat` notice for `proc-macro-error2` (a transitive dependency, not this crate's code).

## Known scoped gaps (not built, documented rather than silently left)

- The ONE-TIME `run_on_enter` call made for the very first state of a `start()`/`submit()` cycle happens BEFORE `drive_chain_with_deadline` is invoked (it's outside the wrapped region); a hang there is bounded only by its own executor-level timeout, same as before this feature — not additionally covered by the new backstop. Every `run_on_enter` call INSIDE the chain loop (i.e. after the first hop) IS covered, since that call happens inside `run_deterministic_chain`.
- The `gateway-config.schema.json` JSON Schema does not list `missionDeadlineSecs` (or the pre-existing `livelockHopBudget`, which has the same gap) — confirmed this schema is not the enforced gate on arbitrary workflow-level config keys at load time (no existing test breaks), so left alone, consistent with the established precedent.

## Files touched

- `crates/praxec-core/src/runtime/runtime_chain.rs` — constants, hop-loop instrumentation + hop-boundary check, `mission_deadline_exceeded_outcome`, `drive_chain_with_deadline`, `mission_deadline_secs_for`, new `mission_deadline_tests` module.
- `crates/praxec-core/src/runtime/runtime.rs` — `ChainOutcome::DeadlineExceeded` variant, `WorkflowRuntime.mission_deadline_secs` field + `with_mission_deadline_secs` builder, `start()` call-site swap + new match arm, `reap_orphaned_runs` mission-deadline check.
- `crates/praxec-core/src/runtime/runtime_submit.rs` — `dispatch_once()` call-site swap + new match arm.
- `Cargo.lock` — regenerated to match the already-committed `0.0.41` `Cargo.toml` versions (pre-existing drift on this worktree's base, not a version bump by this change).
