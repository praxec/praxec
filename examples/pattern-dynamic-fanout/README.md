# Pattern: Dynamic Fan-Out (for_each)

SPEC §24.2. Fan out over a blackboard array — one branch per element.
The `for_each` path resolves at runtime; the `do:` template expands
per element with `$.branch.value` and `$.branch.index`.

## The pattern

```text
    queries: [q1, q2, q3]   ← blackboard slot (array)
         │
         ▼
    ┌─────────────────────────────────┐
    │  parallel executor              │
    │    for_each: $.context.queries   │
    │    do:                           │
    │      kind: noop                  │
    │      args: { q: $.branch.value } │
    │    join: all                     │
    └─────────────────────────────────┘
         │
         ▼
    branches[]: [ {ok, index:0, output}, {ok, index:1, output}, ... ]
```

## Template substitution markers

| Marker | Resolves to |
|--------|------------|
| `$.branch.value` | The current array element |
| `$.branch.index` | Zero-based position in the array |

Both are string-replaced in the `do:` template before execution.

## Edge cases

- **Empty array** (`for_each` resolves to `[]`) → emits `parallel.fanout.empty`,
  succeeds vacuously.
- **Non-array** → runtime error, transition fails.
- **Heterogeneous branches** → use static `branches: [...]` instead.
  `for_each` is for homogeneous work over a dynamic set.

## Run it

```bash
praxec check --config examples/pattern-dynamic-fanout/gateway.yaml
```
