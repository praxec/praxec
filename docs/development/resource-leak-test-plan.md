# Resource-Lifecycle & Leak Test Plan — praxec

**Status:** Active · **Date:** 2026-06-12 · Companion to [`testing-strategy.md`](testing-strategy.md)

Praxec runs work **in-process** and **spawns OS processes** (child MCP servers,
sandboxed agent runs). The question this plan answers: *when a workflow/mission is
done — or is cancelled, or times out — does everything get cleaned up, with no
orphaned processes, leaked tasks, or unbounded in-memory growth?*

## 1. Reframe: what "leak" means here

Rust's ownership makes C-style heap leaks rare. The realistic leak classes, in
priority order:

1. **Orphaned OS subprocesses + file descriptors** — the biggest risk, because we
   spawn real children (`TokioChildProcess` for MCP, bwrap/`systemd-run` for the
   sandbox).
2. **Detached tokio tasks** — `tokio::spawn`ed work that outlives its mission (the
   headless consumer, per-turn agent work).
3. **Unbounded in-memory collection growth** — maps/caches/stores that only ever
   grow (the bus pending-reply map, the MCP connection cache, in-memory stores).
4. **True heap leaks (Arc cycles / `mem::forget`)** — a distant fourth; needs
   tooling, not unit tests.

## 2. Resource inventory (the real spine)

| Resource | Spawned/held by | Teardown path | Status |
|---|---|---|---|
| Child **MCP server** process | `RmcpToolCaller` cache (`Arc<RunningService>`), `StdioGateway` | rmcp `DropGuard` cancels loop → transport drop → `ChildWithCleanup` kills child (async, fire-and-forget). Now also `RmcpToolCaller::close()` for sync drain. | ✅ reaped on drop; cache pools for caller lifetime (bounded by #connections) — `close()` added for graceful shutdown |
| **Sandbox** child (bwrap / OCI / systemd-run) | `sandbox.rs` providers | `cmd.output()` under `tokio::time::timeout`; **fixed** with `kill_on_drop(true)` so a timeout-dropped future kills the child | ✅ fixed (was a confirmed orphan-on-timeout bug) |
| Bus **pending-reply** entry (`oneshot::Sender`) | `Bus.pending` map | **fixed** with `PendingGuard` (RAII) — removed on drop even when the park is abandoned | ✅ fixed (was a confirmed leak in a long-lived bus) |
| Headless **consumer task** | `tokio::spawn(run_headless_consumer)` in `orchestrate` | the drive returns, the spawn handle is dropped | ⚠ verify it's aborted/joined, not detached |
| Per-turn **agent work** | `RigSessionRunner` loop | the run future completes/drops | ✅ awaited; covered by timeout → drop |
| In-memory **stores** (`InMemoryWorkflowStore`, evidence, audit) | runtime | grow by design (history retention) | ⚠ unbounded by design — a retention/eviction policy is a separate product decision, not a leak |

## 3. Confirmed bugs found by this audit (2026-06-12)

The investigation that seeded this plan confirmed **two real bugs + one hardening**
(all fixed — see the lifecycle fix commit):

1. **Bus abandoned parks** — a cancelled/timed-out `request_interaction` stranded
   its `oneshot::Sender` in `pending` forever. → `PendingGuard` RAII cleanup.
2. **Sandbox timeout orphans** — bwrap/OCI children had no `kill_on_drop`, so a
   wall-clock timeout left the sandbox process running. → `kill_on_drop(true)`.
3. **MCP cache hardening** — pooled connections reaped only on async drop. →
   `RmcpToolCaller::close()` for guaranteed synchronous shutdown.

## 4. Two-tier test strategy

### Tier A — deterministic lifecycle tests (unit/integration speed; in CI)

The core technique is **RAII Drop-guard counters**: wrap each spawned resource in a
guard that `+1`s a shared `Arc<AtomicUsize>` on create and `-1`s on drop. After a
workflow lifecycle the counter must return to **baseline (0)**. Run the assertion
across the **three teardown paths**, which is where bugs hide:

- **complete** — the normal resolution,
- **cancel** — an operator aborts mid-flight,
- **timeout** — an executor/wall-clock deadline fires (drops an in-flight future).

Concrete deterministic checks:

- **Bus invariant** — `pending_count()` returns to 0 after a mission resolves *and*
  after a parked orchestrator is aborted. (✅ first case + the abandonment test
  shipped.)
- **Process reaping** — spawn a controlled child via the provider, drive
  complete/cancel/timeout, then assert the **child PID is gone** (Linux: read
  `/proc/<pid>` / enumerate `/proc/<self>/task/*/children`). The harness owns the
  spawn so it knows the PID (avoids the racy "any praxec process" check).
- **MCP `close()`** — after `close()`, the cache is empty and connections are
  cancelled; a follow-up call re-establishes (no stale handles).
- **Task accounting** — the headless consumer task is joined/aborted when the drive
  ends (assert via a Drop-guarded task wrapper or a completion flag).

### Tier B — soak / true-leak detection (periodic CI job, NOT per-commit)

Catches slow growth and Arc cycles that Tier A can't:

- **RSS soak** — drive N hundred workflows (incl. cancel/timeout paths) in a loop;
  assert resident memory **plateaus** (slope ≈ 0 after warmup), and that the
  process/FD count returns to baseline between batches.
- **valgrind / heaptrack** (optional, heaviest) — run a representative scenario
  under a leak detector for genuine heap leaks.

These are slow/heavy — a nightly or on-demand job, gated out of the unit tier.

## 5. The harness (to build)

A small `resource_lifecycle` test-support module providing:

- `CountedGuard` — `Arc<AtomicUsize>` increment/decrement on new/drop, with
  `assert_settled()` (== 0).
- `procutil::child_alive(pid) -> bool` (Linux `/proc`), `living_children(parent)`.
- A `drive_then(path, teardown)` helper that runs a workflow to a chosen teardown
  (complete | cancel | timeout) against the real binary and returns the spawned
  PIDs for assertion.
- Wiring so the bus / MCP caller / sandbox provider can be observed under the guard.

## 6. Progress

- [x] Investigate the 3 suspects (bus / MCP cache / cancel-timeout) — 2 bugs + 1 hardening confirmed
- [x] Fix: bus `PendingGuard`, sandbox `kill_on_drop`, MCP `close()`
- [x] Bus abandonment test (`pending_count → 0`)
- [ ] Tier A harness (`CountedGuard` + `/proc` child enumeration)
- [ ] Process-reaping matrix (complete/cancel/timeout) for MCP + sandbox children
- [ ] Consumer-task accounting test
- [ ] Tier B soak job (RSS plateau) — separate periodic CI
