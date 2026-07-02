# Pattern: Output Mapping Operators

All six operators in the `output:` block, plus path resolution
and array projection (`[*]`). Defined in `crates/praxec-core/src/mapping.rs`.

## The operators

| Operator | YAML | Effect |
|----------|------|--------|
| **set** | `slot: { set: "literal" }` | Write a literal value |
| **add** | `slot: { add: [a, b] }` | Arithmetic sum |
| **subtract** | `slot: { subtract: [a, b] }` | Arithmetic difference |
| **multiply** | `slot: { multiply: [a, b] }` | Arithmetic product |
| **divide** | `slot: { divide: [a, b] }` | Arithmetic quotient (null on div-by-zero) |
| **concat** | `slot: { concat: [a, b, c] }` | String concatenation |
| **path** | `slot: "$.output.field"` | Resolve against executor output |
| **context path** | `slot: "$.context.existing"` | Read existing blackboard slot |
| **arguments path** | `slot: "$.arguments.mode"` | Read LLM-provided argument |
| **workflow input** | `slot: "$.workflow.input.goal"` | Read initial workflow input |
| **array projection** | `slot: "$.output.branches[*].ok"` | Pluck field from every element |

## Numeric handling

- `null` / missing operands → treated as `0` — counters work on first increment.
- Division by zero → `null`.
- NaN / infinity → `null`.
- Results prefer integers when round.

## String handling

- Path resolution returns `null` when the path doesn't resolve.
- String literals (not starting with `$.`) stay as-is.
- `concat` coerces booleans and numbers to their string form.

## Run it

```bash
praxec check --config examples/pattern-output-mapping/gateway.yaml
```
