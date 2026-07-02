# Pattern: Circuit Breaker

A declarative circuit breaker using three primitives you already have:
a **loop-back transition**, a **blackboard counter**, and **guard-gated exits**.

## The pattern

One state with three transitions:

```text
     ┌──────────────────────────┐
     │       retry (loop)       │ ← guard: failure AND retries < N
     ▼                          │
  ┌──────────┐                  │
  │  action   │─────────────────┘
  └──────────┘
     │                          ┌────── work (success guard)
     ├─────────────────────────→│
     │                          └──────
     │                          ┌────── escalate (retries >= N guard)
     └─────────────────────────→│
                                └──────
```

- **Success** exits when the action's output satisfies a guard.
- **Retry** loops back to the same state, incrementing a counter.
- **Escalate** trips when the counter hits the threshold.

No loops in the runtime — the state machine's own links carry every iteration.
Every retry is a transition record in the audit log.

## The primitives

| Primitive | Mechanism |
|-----------|-----------|
| State loop-back | Transition `target:` points to its own state |
| Counter | `output: retryCount: { add: ["$.context.retryCount", 1] }` |
| Guard gate | `expr: "$.context.retryCount >= 5"` on the escalate transition |

## Anti-patterns

- **Self-loop without guard** — unbounded, caught by `check` as a warning.
- **Counter without escalation** — the workflow hangs when the budget runs out.
  Always pair the counter with an exit transition gated on the counter value.

## Run it

```bash
praxec check --config examples/pattern-circuit-breaker/gateway.yaml
```
