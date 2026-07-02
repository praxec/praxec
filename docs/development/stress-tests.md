# Stress tests

How we pressure-tested the declarative surface, what we found, and what
we changed.

This is a living document. New realistic patterns belong here when the
existing surface can't express them declaratively — and the system grows
to absorb them.

---

## Method

For each major capability — workflows, guards, output mappings,
reliability, evidence, capabilities, schema validation — we wrote a
**realistic scenario** as a golden test in
`crates/praxec-core/tests/baseline_scenarios.rs`,
`stress_guards_mapping.rs`, and `stress_lifecycle.rs`.

Each scenario has the shape:

```
Build a runtime from inline YAML.
Drive a sequence of `start` / `submit` calls.
Assert response shape, error codes, audit taxonomy, and final context.
```

Where the system *did* express the pattern, the test became a regression
guard. Where it *couldn't*, the test was the proof that a minimum-surface
declarative addition was needed; the test then drove the implementation.

The guiding principle was **"as declarative as possible."** A workaround
that requires writing a custom executor / guard in Rust was treated as a
declarative gap, not a solution.

---

## Scenarios

Each test name in the source file corresponds to a row below. **B** =
baseline (already worked); **S** = stress (revealed a gap that we
closed).

| ID    | Pattern                                                                | Outcome                                       |
|-------|------------------------------------------------------------------------|-----------------------------------------------|
| B-01  | Simple proxy call (`proxy_default` workflow + one tool)                | Passed                                        |
| B-02  | Multi-state governed flow with permission guard, happy path            | Passed                                        |
| B-03  | Schema rejection includes legal recovery links                          | Passed                                        |
| B-04  | Guard rejection: workflow stays put + audit emits `transition.rejected` | Passed                                        |
| B-05  | Stale `expectedVersion` rejected even when guards/schema pass           | Passed                                        |
| B-06  | Reliability retries exhaust → `failed`, not state advance              | Passed                                        |
| B-07  | Reliability fallback wins after primary exhausts                       | Passed                                        |
| B-08  | Named capability reused in proxy *and* in a workflow transition        | Passed                                        |
| S-01  | Bounded loop with a counter ("remediate up to 3")                       | **Drove three fixes** (see below)             |
| S-02  | Schema defaults applied to transition arguments                         | **Drove fix #2**                              |
| S-03  | Multi-approver quorum (2 of N approvers required)                       | **Drove fix #3**                              |
| S-04  | Nested schema defaults (defaults at any depth, not just top level)      | **Drove fix #2 recursion**                    |
| S-05  | `set:` operator for literal values in output mappings                   | **Drove fix #1**                              |
| S-06  | Output mappings reading from `$.arguments.*` and `$.workflow.input.*`   | **Drove fix #1 scope-aware**                  |

### Deterministic chaining scenarios

Tested in the `crates/praxec-core/tests/chain_*.rs` suites
(`chain_basic.rs`, `chain_loop.rs`, `chain_guidance.rs`,
`chain_audit.rs`, `chain_audit_criticality.rs`):

| ID    | Pattern                                                                | Outcome                                       |
|-------|------------------------------------------------------------------------|-----------------------------------------------|
| DC-01 | Linear chain stops at agent decision point                             | Passed                                        |
| DC-02 | Fully deterministic chain reaches terminal                             | Passed                                        |
| DC-03 | Mixed state (deterministic + agent) stops chain                        | Passed                                        |
| DC-04 | Deterministic transitions hidden from links                            | Passed                                        |
| DC-05 | `maxChainDepth` stops chain early                                      | Passed                                        |
| DC-06 | Chain failure returns partial steps + recovery link                     | Passed                                        |
| DC-07 | Chain auto-executes after submit (`praxec.command`)                   | Passed                                        |
| DC-08 | Phase guidance appears in response                                     | Passed                                        |
| DC-09 | Phase guidance absent when state has no goal/guidance                   | Passed                                        |
| DC-10 | Chain step versions are strictly increasing                             | Passed                                        |
| DC-11 | Chain emits `chain.step`, `chain.completed` audit events                | Passed                                        |
| DC-12 | No chain when initial state is terminal                                 | Passed                                        |
| DC-13 | No chain when state has no transitions                                  | Passed                                        |
| DC-14 | explain (`praxec.query`) includes actor type and deterministic flag    | Passed                                        |
| DC-15 | Deterministic transitions submittable manually (no actor gate — FMECA)  | Passed                                        |
| DC-16 | Chain works without executor (pure routing)                             | Passed                                        |

---

## Gaps found and fixes shipped

### 1. Output mappings could only read `$.output.*` and only carry strings

Realistic patterns broke immediately:

- **Counters.** "Increment a remediation count each time you self-loop"
  needs `attempts + 1`. There was no way to express arithmetic in YAML.
- **Pass-through.** "Stash the user's note from `$.arguments.note` into
  context for later steps" needs to read from arguments, not just
  executor output.
- **Literal flags.** "Mark the workflow as reviewed" needs a way to
  write a constant value into context.

**Fix.** `mapping::resolve_value` now accepts:

| Form                                          | Meaning                                                         |
|-----------------------------------------------|-----------------------------------------------------------------|
| `"$.output.x"` / `"$.context.x"` / `"$.arguments.x"` / `"$.workflow.input.x"` | Read from any of the four scopes. |
| `{ add: [a, b] }`, `subtract`, `multiply`, `divide` | Arithmetic; operands may be paths or literal numbers. Missing/null operands default to 0 so a counter can start unset. |
| `{ set: <value> }`                            | Literal pass-through.                                           |

Backward-compatible: every existing mapping (string-only, `$.output.*`)
works as before.

### 2. JSON Schema `default` values weren't applied

Schema in:

```yaml
inputSchema:
  type: object
  properties:
    priority: { type: string, default: "normal" }
```

…parses fine, but with no default-application step, the field never
arrived at the executor. Worse, if it was also `required`, validation
failed for callers who reasonably omitted it.

**Fix.** `runtime::apply_schema_defaults` walks the schema's
`properties`, fills in any `default` for missing keys, and recurses into
nested object schemas. Applied to both workflow `input` (in `start`) and
transition `arguments` (in `submit`).

### 3. Evidence guard couldn't express quorums

The original `requires: [tests_passed, security_scanned]` checked "at
least one of each kind." Multi-approver patterns ("2 of N reviewers must
approve") would need a custom guard — hello procedural escape hatch.

**Fix.** `requires` accepts both forms in the same list:

```yaml
guards:
  - kind: evidence
    requires:
      - tests_passed                       # string form, count >= 1
      - { kind: approval, count: 2 }       # object form, count >= N
```

Backward-compatible: bare strings still mean "at least one record."

### 4. Workflow-instance state had no declarative seed

The bounded-loop test failed *before any of the above fixes mattered*:
on the first remediate, the guard `$.context.attempts < 3` evaluated
against missing `attempts`, returned false, and blocked the loop from
even starting.

The natural workaround — an `onEnter` action that copies workflow input
into context — fired *every time the state was entered*, including on
self-loops, which reset the counter every iteration.

**Fix.** `initialContext: { … }` on a workflow definition. Seeded into
`instance.context` at start, untouched by self-loop transitions.

```yaml
workflows:
  demo:
    initialState: open
    initialContext:
      attempts: 0
    states:
      open:
        transitions:
          remediate:
            target: open
            guards: [{ kind: expr, expr: "$.context.attempts < 3" }]
            executor: { kind: noop }
            output:
              attempts: { add: ["$.context.attempts", 1] }
```

This is the **single declarative knob** for "what does instance state
look like at workflow start." No ceremony, no onEnter, no input
gymnastics.

---

## Patterns we're confident about now

After fixes 1–4, the system declaratively expresses every realistic
pattern we tested:

- Bounded loops with counters
- Multi-step workflows with branching via guarded transitions
- Multi-approver quorums (2-of-N, M-of-N)
- Schema defaults at any depth
- Pass-through of arguments / inputs into context
- Literal flags / status markers
- Reliability with retry + fallback wired through audit

The corresponding scenarios are the regression contract.

---

## Subsequent stress passes

A second round of pressure-testing surfaced and fixed three deferred
patterns, plus the LLM-guidance gap.

### LLM guidance: transition prefill (S-07)

Realistic pattern: a `create_pull_request` link that an LLM has to
synthesize from scratch wastes most of the model's effort on values
that are already deterministic (repo, base, head). Without a way to
ship pre-shaped arguments, every workflow author either repeats
themselves in `executor.map` or hopes the LLM gets it right.

**Fix.** A transition can declare a `prefill: {…}` block that resolves
at link-generation time against `$.workflow.input.*` and `$.context.*`,
and the resolved values land in `link.args.arguments`. The model takes
that block as the starting point and only generates the genuinely-LLM
fields. Full guide: [../guides/llm-guidance.md](../guides/llm-guidance.md).

### Idempotency keys for retries (S-08, S-09)

Realistic pattern: a `create_pr` REST call times out at 30s but
actually succeeded; the retry creates a second PR. Without a stable
key carried across retries, side-effecting executors aren't safe to
retry.

**Fix.** `executor.idempotencyKey: true` (auto-derived as
`workflowId.transition.correlationId`) or a string template with
`{workflowId}` / `{transition}` / `{correlationId}` tokens. The runtime
computes the key once per `submit` and passes it via:

- `Idempotency-Key` HTTP header (REST executor)
- `IDEMPOTENCY_KEY` env var (CLI executor)
- `_idempotencyKey` argument (MCP executor)

The same key is used across retries and across fallback executors so a
downstream service that dedupes on the key sees one logical attempt.
Audit events for `executor.*` include the key under
`payload.idempotencyKey`.

### Workflow-level lazy timeouts (S-10)

Realistic pattern: an approval workflow that hasn't finished in 24
hours should auto-escalate. We had per-executor `timeoutMs` but no
workflow-level deadline.

**Fix.** `workflows.<id>.timeoutMs` + `onTimeout.target`. Lazy
semantics: the timeout is checked on the next `submit` or `get`. If
the workflow has been alive longer than the deadline, the runtime
auto-transitions to `onTimeout.target`, emits `workflow.timed_out`,
and short-circuits the caller's submit. No sweeper / cron required —
which is the *right* tradeoff for our model: workflows only matter
when interacted with.

### Link filtering by guards (S-11, S-12)

Realistic pattern: a state with mutually exclusive guarded transitions
(`if risk > 50 → manual_review, else → auto_approve`). Default
behavior returns both links, the model picks one, possibly gets
GUARD_REJECTED, recovers from the rejection's `links` array. Wasted
round trip.

**Fix.** `linkFilter: byGuards` at workflow or state level. When set,
each link is generated by silently evaluating the transition's guards
against the current context + caller's principal; only passing
transitions are returned. State-level setting overrides workflow-level
so authors can opt one tricky state in without committing the whole
workflow.

The guard evaluation in link-gen is silent (no audit event) so it
doesn't pollute the trail. Argument-dependent guards typically filter
out — at link-gen time we don't know what arguments the next call
will carry.

---

## Remaining patterns we deliberately defer

### Cross-workflow choreography (S-13)

A workflow that wants to spawn a sub-workflow and wait for its result
is now expressible declaratively via the `workflow` executor kind:

```yaml
executor:
  kind: workflow
  definitionId: with_artifact_lock
  input:
    artifact: "$.context.artifact_name"
    owner: "$.workflow.input.user"
  timeoutMs: 60000
```

The executor:
1. Starts the sub-workflow internally (the start operation, via
   `praxec.command`) with the given `definitionId` and `input` (path
   expressions resolved against the parent's context and arguments).
2. Polls the get operation (via `praxec.query`) until the sub-workflow
   reaches a terminal state.
3. Returns the sub-workflow's final `context` as `ExecuteResult.output`.
4. Emits `sub_workflow.started`, `sub_workflow.completed` (or
   `sub_workflow.failed`) audit events.
5. If the sub-workflow times out, returns `ExecutorError::Timeout`.

Integration tests live in
`crates/praxec-executors/tests/workflow_executor.rs`.

---

## How to add a new stress test

If you find a realistic pattern that the docs claim should be
expressible, but you can't write the YAML for it without resorting to a
custom executor or guard:

1. Write the test in
   `crates/praxec-core/tests/baseline_scenarios.rs` (or the
   matching `stress_*.rs` suite) as **the declarative form you wish
   worked**, even if it doesn't yet.
2. Run it. Watch it fail. The failure is the pressure-test.
3. Decide: is this a runtime gap, a doc-and-pattern gap, or genuinely
   not a goal?
4. If it's a runtime gap, add the minimum surface to close it. Update
   this document with the gap-and-fix narrative.
5. If it's a doc-and-pattern gap, add the recipe to
   [../architecture/mcp-control-architecture.md](../architecture/mcp-control-architecture.md) instead of
   growing the runtime.
6. If it's not a goal, document the explicit non-goal here so the next
   person doesn't repeat the analysis.

The test that pressured the runtime to grow is the regression guard
once the gap closes.
