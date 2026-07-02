# Pattern: Parallel Fan-Out / Fan-In

SPEC §24. Concurrent execution of N independent branches inside a single
transition. The state machine stays sequential — exactly one version bump,
one transition record. The executor fans out internally.

## What this shows

Five variants of the `parallel` executor, each a separate workflow:

| Workflow | Join condition | Failure mode | What it demonstrates |
|----------|---------------|-------------|---------------------|
| `parallel_all` | `all` (default) | `bail` | Every branch must succeed |
| `parallel_any` | `any` | `continue` | First success wins, siblings cancelled |
| `parallel_at_least` | `{ at_least: 2 }` | `continue` | K-out-of-M quorum |
| `parallel_percent` | `{ percent: 70 }` | `continue` | Percentage-based quorum |
| `parallel_all_continue` | `all` | `continue` | Run all, aggregate, decide after |

## The primitives

| Config key | Values | Behavior |
|------------|--------|----------|
| `join` | `all`, `any`, `{ at_least: K }`, `{ percent: P }`, `{ expression: "..." }` | When does the aggregate succeed? |
| `on_branch_failure` | `bail` (default), `continue` | First failure cancels siblings, or let them run |
| `max_concurrency` | integer | Cap in-flight branches (required when ≥ 10) |
| `total_timeout_ms` | integer | Aggregate wall-clock cap |
| `max_recursion_depth` | 3 (default) | Cap nested `parallel` depth |

## Branch output shape

Every branch produces `{ ok, index, output? | error? }`. The executor
aggregates these into `summary: { n, ok_count, failed_count, verdict }`.

Use `$.output.branches[*].ok` path projection in the transition's
`output:` block to pluck per-branch results.

## Audit events

Every branch emits `parallel.branch.started` / `.completed` / `.failed` /
`.cancelled`. The aggregate emits `parallel.fanout.completed`. All carry
the parent correlation_id + branch_index.

## Run it

```bash
praxec check --config examples/pattern-parallel/gateway.yaml
```
