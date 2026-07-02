# Pattern: Recovery & Escalation Topology

A recovery topology where a state can fail, auto-retry, escalate to
a human, and resume from where it left off. Composes circuit-breaker,
deterministic failure-detection, and human-in-the-loop.

## The topology

```text
    ┌─────────────────────────────────────────┐
    │              retry (≤ budget)            │
    ▼                                         │
  ┌──────────┐  ┌──────────┐  ┌──────────┐   │
  │ attempt  │──│  check   │──│ escalate  │───┘
  └──────────┘  └──────────┘  └──────────┘
                   │                │
                   │ pass           │ retries exhausted
                   ▼                ▼
                 done          human_triage
                                   │
                                   ▼
                               continue
                                   │
                                   ▼
                                 done
```

## Key concepts

- **Attempt** — the operation that might fail. Runs any executor.
- **Check** — deterministic state that reads the attempt's output and routes.
  No LLM involvement — the guard expressions do the work.
- **Escalate** — state reached when retries are exhausted. `actor: human`
  hands off to a person for triage.
- **Resume** — after human triage, transitions back to `attempt` — the
  blackboard carries the accumulated context forward.

## Audit visibility

Every iteration produces:
- `attempt` → one transition record with executor outcome
- `check` → one transition record with guard evaluations
- `retry` / `escalate` → named transition showing the path taken

A downstream consumer can count `transition: retry` per `workflow_id` to
compute a retry rate without parsing executor output.

## Run it

```bash
praxec check --config examples/pattern-recovery/gateway.yaml
```
