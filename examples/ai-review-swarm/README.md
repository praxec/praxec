# AI Review Swarm — Parallel Code Review Workflow

Seven specialized AI reviewers review the same diff in parallel, findings are
merged and deduplicated, and a human arbitrates. Built entirely on Praxec
primitives — no custom flow, no agent loop, no scheduler.

## Architecture

```text
extract_diff ──→ review_swarm (parallel, 7 branches)
                        │
                  ┌─────┼─────┬─────┬─────┬─────┬─────┐
                  ▼     ▼     ▼     ▼     ▼     ▼     ▼
               arch   bug   sec   perf  test   api   maint
                  │     │     │     │     │     │     │
                  └─────┼─────┴─────┴─────┴─────┴─────┘
                        ▼
                   merge_findings (delegate: sub-agent)
                        │
                        ▼
                 human_arbitration (actor: human)
                     │         │
                apply_fixes  done (reject all)
                     │
                   done
```

## How it works

### Step 1: Extract diff
`kind: script` runs `git diff` and produces `{ diff, files[], stats }` as
structured JSON. Supports `--uncommitted` and `--base <branch>` modes.

### Step 2: Parallel review swarm
The `parallel` executor fans out 7 branches concurrently. Each branch is a
`kind: mcp` call to an LLM with a specialized system prompt:

| Branch | Focus |
|--------|-------|
| Architecture | Correctness, coupling, abstraction boundaries |
| Bug Risk | Edge cases, null handling, error propagation, state machines |
| Security | Injection, auth bypass, secret exposure, sandbox escape |
| Performance | Complexity, allocation, locking, caching, N+1 queries |
| Testing | Missing tests, weak assertions, coverage gaps |
| API Contract | Breaking changes, schema compat, versioning |
| Maintainability | Naming, dead code, magic numbers, readability |

**Join condition:** `all` — every reviewer must complete.
**Failure mode:** `continue` — if one reviewer fails, the other 6 still
produce results. The merger accounts for null outputs.

### Step 3: Merge + dedupe
A `delegate:` sub-agent receives all 7 arrays, identifies duplicates (same
file + symbol + substantively similar problem), keeps the highest-severity
version, and ranks by priority. Produces `{ merged: [...], stats: {...} }`.

### Step 4: Human arbitration
`actor: human` — a human reviews the ranked findings and decides:
- **accept** → queued for autofix
- **reject** → dismissed
- **ignore permanently** → never surfaces again (rule-backed)
- **convert to rule** → becomes a permanent guard in the workflow

### Step 5: Optional autofix
A `delegate:` sub-agent applies minimal, surgical fixes to accepted findings.
Produces `{ changes: [...], unfixable: [...] }`.

## Praxec primitives in play

| Primitive | Where |
|-----------|-------|
| `kind: parallel` + `on_branch_failure: continue` | Step 2 — fan-out that tolerates individual failures |
| `max_concurrency: 7` | Step 2 — all reviewers run concurrently |
| `delegate:` | Step 3 (merge) + Step 5 (autofix) — sub-agent delegation |
| `actor: human` | Step 4 — human-in-the-loop arbitration |
| `kind: script` | Step 1 — deterministic git diff extraction |
| `evidence: { kind, id, summary }` | Step 2 — per-reviewer evidence records for audit |
| `skills:` (guidance fragments) | 9 skill fragments (7 reviewers + merge + autofix) |
| `inputSchema` | Steps 3, 4, 5 — typed contracts for LLM arguments |
| `output: { ... }` | Every step — blackboard state propagation |
| `template {{ $.context.X }}` | Step 4 — live context in human-facing guidance |

## Every finding is an audit event

Each reviewer's branch emits `review` evidence with its findings.
The merge step emits its deduplication stats. The human's verdicts are
recorded as transition arguments. The autofix step emits `changes` and
`unfixable`. Every step is replayable from the audit log.

## The token economics

**Without parallel fan-out:**
7 reviewers × 3 round trips each (get input → generate → submit) = 21 calls,
each re-reading the diff. ~21× input token burn.

**With parallel fan-out:**
1 `workflow.start` → diff extracted once, sent to 7 reviewers in one
transition. 7× output tokens, 1× input token for the diff. After merge,
the human sees the merged findings — one payload, not seven.

## Wiring it up

1. Wire an LLM MCP server behind the `review_llm` connection.
2. Register sub-agents for `merge-agent` and `fix-agent` in your TUI config.
3. Set `base_branch` in `initialContext` or override via `workflow.start` input.

```bash
# Validate
praxec check --config examples/ai-review-swarm/gateway.yaml

# Run (gateway + TUI)
praxec serve --config examples/ai-review-swarm/gateway.yaml
```

## Audit-driven metrics

From the audit log alone, a downstream consumer can compute:

| Metric | Derivation |
|--------|-----------|
| `review_coverage` | count(reviewers with findings) / 7 |
| `duplicate_rate` | 1 - (merged_count / total_raw) |
| `severity_distribution` | `merge_stats.high / medium / low` |
| `accept_rate` | accepted / total verdicts |
| `autofix_success_rate` | count(changes) / accepted |
| `per-reviewer_signal_ratio` | findings per reviewer (normalized) |

No new metrics service — just `tail -f audit.log | jq …`.