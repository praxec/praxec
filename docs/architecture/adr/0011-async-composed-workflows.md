# ADR-0011: Composed workflows execute asynchronously; suspension is data

**Status:** Accepted

**Date:** 2026-06-15

## Context

`WorkflowExecutor::execute()` for a `kind: workflow` step started the child and
**polled it to terminal in a `sleep(200ms)` loop**, blocking the parent's
dispatch and pinning a tokio worker for the child's entire lifetime. A child
parked on `actor: human` never returned, so the nested gate was invisible.
Three components also leaned on synchronous-execution assumptions that are unsafe
under concurrent re-drive: the recursion depth guard was a `tokio::task_local!`
(`WORKFLOW_DEPTH`) that does not propagate across `spawn`, silently defeating the
guard; `run_id` uniqueness was a check-then-act (TOCTOU) with no atomic
constraint; and file-lock authority was split across two separate
`RepoLockSpace`s (runtime vs. agent-overlay `promote()`), so they did not
mutually exclude.

## Decision

**A workflow is its persisted `WorkflowInstance`; workers are stateless.** Any
worker computes the next step as a pure function of the data —
`(instance, child status, file locks) → next` — and commits via
`save_if_version` (a correct CAS: exactly one concurrent advance wins, the rest
get `STALE_WORKFLOW_VERSION` and retry). "Suspended" is not a parked thread or a
scheduler entry; it is a non-terminal instance carrying a recorded dependency
(`_subworkflow_wait { child_workflow_id, depth }`).

`WorkflowExecutor::execute()` becomes non-blocking: reuse the recorded child on
re-evaluation (never spawn twice) else spawn at `depth = parent.depth + 1`; read
the child's status once; if terminal, collect outputs and advance (fast-path: a
deterministic child that auto-chains to terminal during `start()` resolves in
the same dispatch); if non-terminal, persist the wait and return a non-advancing
`waiting` result whose HATEOAS links expose the child to any two-tool client —
**no thread is held**. When a child reaches terminal it carries
`parent_workflow_id`, so the runtime enqueues the parent for re-drive via the
existing lock-resume `redrive` plumbing (a ready-queue, not a new subsystem).

The three synchronous assumptions are fixed accordingly: depth moves to an
instance field; `run_id` uniqueness becomes an atomic constraint (delete the
check-then-act); and a single shared `RepoLockSpace` is threaded into both the
runtime and the agent overlay.

## Consequences

- No worker starvation and no re-entrancy deadlock; composed workflows scale to
  arbitrary nesting without pinning threads.
- Nested human gates resolve as an ordinary consequence of re-evaluation, not a
  special case — restart-safe, because a re-drive is just a `submit` of the
  parent's pending transition.
- The bespoke `SubworkflowScheduler` / suspend-resume machinery is deleted.

## References

- `crates/praxec-executors/src/workflow.rs` (`WorkflowExecutor`)
- `crates/praxec-core/src/runtime_submit.rs` (redrive, file locks)
- Builds on the validity-first engine roadmap (P2).
