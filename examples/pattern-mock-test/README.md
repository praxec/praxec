# Pattern: Mock-Test a Workflow End-to-End

Drive any workflow through its full state machine in a test, substituting
real executors with canned outputs. No LLM needed, no external services.
The same patterns used to test the TDD example in `tests/tdd_example.rs`
apply to any workflow YAML.

## The pattern

```rust
// 1. Build an in-memory runtime with scripted executors
let config = load_resolved("path/to/workflow.yaml")?;
let definitions = Arc::new(ConfigDefinitionStore::from_config(&config));
let store = Arc::new(InMemoryWorkflowStore::new());
let executors = Arc::new(ScriptedRegistry::new()
    .with("cli", vec![output1, output2])   // canned outputs per call
    .default_noop());                       // everything else → {}
let guards = Arc::new(DefaultGuardEvaluator::new());
let audit = Arc::new(MemoryAuditSink::new());
let runtime = WorkflowRuntime::new(definitions, store, executors, guards, audit);

// 2. Start
let resp = runtime.start(StartWorkflow {
    definition_id: "my_workflow".into(),
    input: json!({}),
    ..
}).await?;

// 3. Walk transitions
let resp = runtime.submit(SubmitTransition {
    workflow_id: id.clone(),
    expected_version: v,
    transition: "next_step".into(),
    ..
}).await?;
assert_eq!(resp["workflow"]["state"], "expected_state");

// 4. Walk until terminal or decision point
```

## The primitives

| Component | What it replaces |
|-----------|-----------------|
| `InMemoryWorkflowStore` | Persistent workflow storage |
| `MemoryAuditSink` | File-based audit log |
| `ScriptedRegistry` | Production executor registry |
| Queue of `canned_outputs` per executor kind | Real executors |
| `DefaultGuardEvaluator` | (same as production — guards are pure functions of context) |

Every guard, blackboard write, and state-machine transition executes against
the real runtime — only the executor outputs are mocked.

## What this catches

- **Dead states** — states with no inbound transition
- **Unreachable exits** — transitions whose guards can never pass given the blackboard shape
- **Self-loops without guard** — caught by `check`
- **Blackboard type errors** — typed slot writes that violate the schema
- **Guard logic errors** — `$.context.x` reads from wrong slot, wrong operator
- **Deterministic chain routing** — does the chain engine pick the right path?

## What this does NOT catch

- **Real executor failures** — your mock is the source of truth for outputs
- **LLM behavior** — the mock submits transitions mechanically; no model judgment
- **Timing races** — in-memory stores are synchronous

## Run it

```bash
# Check the pattern example itself
cargo run -p praxec -- check --config examples/pattern-mock-test/gateway.yaml

# Run tests against all examples
cargo test -p praxec-core --test walk_examples
```
