# Configuration reference

The full schema lives at `schemas/gateway-config.schema.json`. Typify-
generated Rust types are in the `praxec-schema` crate. This page
is the human-shaped tour.

---

## Top level

```yaml
version: "1.0.0"
include: []         # other YAML files to merge in (deep merge; later wins)
capabilities: {}    # named, reusable capabilities
connections: {}     # named handles to MCP / CLI / REST endpoints
proxy:              # capability surface (with optional imports)
  expose: []        #   inline {name, executor, …} OR reference {capability, as?, …}
  import: []
workflows: {}       # multi-state governed workflows
                    #   each workflow may declare:
                    #     initialContext: {…}    seed instance.context at start
                    #     timeoutMs: <int>       lazy workflow-level deadline
                    #     onTimeout: {target}    where to go on timeout
                    #     linkFilter: byGuards   show only currently-passing transitions
                    #     maxChainDepth: <int>   cap for deterministic chain (default 50)
                    #   each state may declare:
                    #     goal: <string>         one-line objective for LLM guidance
                    #     guidance: <string>     detailed instructions for LLM
audit: {}           # audit sink configuration
discovery: {}       # which kinds appear in the discovery index
store: {}           # persistent WorkflowStore selection
```

---

## Connection kinds

| Kind   | Required fields                                                          |
|--------|---------------------------------------------------------------------------|
| `mcp`  | One of `command` (process) or `url` (Streamable HTTP); `args`, `env` optional |
| `cli`  | `command`; `workingDirectory`, `env` optional                            |
| `rest` | `baseUrl`; `headers` optional                                            |

See [../guides/connections.md](../guides/connections.md) for the spawn patterns and import
mechanics.

---

## Executor kinds

Executors live inside `proxy.expose[].executor`, transition `executor`,
state `onEnter.executor`, and reliability `fallback.executors[]`.

| Kind     | Notes                                                                         |
|----------|-------------------------------------------------------------------------------|
| `noop`   | Returns `{}`. Default when an exposure has no executor.                       |
| `cli`    | Spawns a process; supports `$.arguments.*` / `$.context.*` / `$.workflow.input.*` arg interpolation. |
| `mcp`    | Calls `tools/call` on a child MCP server (process or HTTP) resolved via `connection`. Lazy client cache. |
| `rest`   | HTTP request: configurable `method`, `path` (with `{var}` templating), `query`, `headers`, JSON `body`. Status codes map to `ExecutorError` so retries kick in for 408/429/5xx. |
| `human`  | Records a pending approval and emits `human.approval.requested`.              |
| `script` | Runs a declared, verb-tagged script (SPEC §22) — a named command bundle resolved through a connection, distinct from raw `cli`. |
| `llm`    | Governed in-runtime LLM call (SPEC §33); prompt + model binding resolved from config, output is a candidate subject to guards. |
| `agent`  | Spawns a sub-agent (subprocess) bound to a model; added by the binary's overlay, not the default registry. |
| `parallel` | Fan-out / fan-in inside one transition (SPEC §24): runs `branches` concurrently and joins per the declared condition. |
| `pipeline` | Sequential composition of N executor steps inside one transition (SPEC §25); each step's output threads as the next step's input. |
| `workflow` | Starts a sub-workflow by `definitionId`, waits for completion, returns its final context. Supports `input` mapping and `timeoutMs`. |
| `registry` | Authoring-time: reads/writes the workflow registry (gated; WRITE_DISABLED unless `praxec.authoring.write_enabled`). |
| `dry_run` | Authoring-time: simulates a transition without committing side effects. |
| `structural_analysis` | Authoring-time: runs structural checks over a workflow definition. |
| `ingest` | Authoring-time: imports an external definition into the registry. |
| `diff`   | Authoring-time: computes the diff between two workflow definitions. |

An executor can also reference a named capability instead of declaring
inline:

```yaml
executor: { capability: safe.create_pr }
```

See the [Capabilities](#capabilities) section below.

Any executor can declare an idempotency key:

```yaml
executor:
  kind: rest
  connection: github_api
  idempotencyKey: true                       # auto: workflowId.transition.correlationId
  # idempotencyKey: "praxec:{transition}:{workflowId}"   # custom template
```

Surfaces per executor: `Idempotency-Key` HTTP header (REST),
`IDEMPOTENCY_KEY` env var (CLI), `_idempotencyKey` argument (MCP).

### REST executor shape

```yaml
executor:
  kind: rest
  connection: github_api
  method: POST                                  # default GET
  path: "/repos/{owner}/{repo}/pulls"           # {var} pulls from arguments → context → input
  query: { state: open }                        # values may use $.arguments / $.context / $.workflow.input
  headers: { X-Foo: bar }                       # per-call overrides
  body:
    title: "$.arguments.title"
    head:  "$.arguments.head"
    base:  main
```

### CLI executor shape

```yaml
executor:
  kind: cli
  connection: dotnet
  args:
    - test
    - "$.arguments.project"                     # interpolated; otherwise passed verbatim
```

---

## Guard kinds

Guards live on transitions and on capability definitions. They run in
declaration order, before the executor.

| Kind         | Configuration                                                     |
|--------------|--------------------------------------------------------------------|
| `permission` | `permission: foo.bar`                                              |
| `role`       | `role: approver`                                                   |
| `expr`       | `expr: "$.context.x <= 80"`                                        |
| `evidence`   | `requires: [tests_passed, …]` — every listed kind must have a record |

See [governance.md](governance.md#guards-preconditions) for semantics.

---

## Transition branches

Declare auto-branching for transitions whose destination depends on the
executor's result:

```yaml
transitions:
  run_tests:
    target: red                                 # default fallback
    executor:
      kind: cli
      connection: shell
      args: ["-c", "cargo test"]
      treatNonZeroAsFailure: false
    output:
      passed: "$.output.success"
    branches:
      - when:   { kind: expr, expr: "$.context.passed == true" }
        target: green
      - when:   { kind: expr, expr: "$.context.passed == false" }
        target: red
```

Evaluated after the executor succeeds and after `output` mappings have
been applied, so branches can depend on values just produced. First
match wins; falls back to the declared `target` if none match. Emits
`transition.branched` audit events.

The CLI executor's `treatNonZeroAsFailure: false` flag (default `true`
keeps existing behavior) routes a non-zero exit to `output.success:
false` instead of erroring the transition — useful for "exit code is
data" patterns.

---

## Transition prefill

Pre-shaped argument values that the runtime injects into each link for a
transition. Reduces what an LLM caller has to generate.

```yaml
transitions:
  create_pr:
    target: review
    inputSchema:
      type: object
      required: [repo, base, head, title, body]
      properties: { … }
    prefill:
      repo: "$.workflow.input.repo"
      base: "main"
      head: "$.context.branch_name"
    executor: { kind: mcp, connection: github, tool: create_pull_request }
```

Resolution uses the same expression syntax as output mappings — path
strings, operator objects, or bare literals. See
[../guides/llm-guidance.md](../guides/llm-guidance.md) for the design patterns.

---

## Deterministic chaining

Transitions tagged `actor: "deterministic"` auto-execute without LLM
involvement. When a workflow enters a state where **all** transitions
are deterministic, the runtime chains through them automatically until
it hits a decision point (any non-deterministic transition), a terminal
state, the depth limit, or a failure.

```yaml
workflows:
  deploy_pipeline:
    initialState: lint
    maxChainDepth: 10              # default 50; safety cap per chain run

    states:
      lint:
        transitions:
          run_lint:
            target: test
            actor: deterministic
            executor: { kind: cli, command: lint-check }

      test:
        transitions:
          run_tests:
            target: build
            actor: deterministic
            executor: { kind: cli, command: test-runner }

      build:
        transitions:
          build_artifact:
            target: ready_to_deploy
            actor: deterministic
            executor: { kind: cli, command: build-artifact }

      ready_to_deploy:
        transitions:
          deploy:
            target: deployed
            actor: agent                # chain stops here — LLM decides
            executor: { kind: cli, command: deploy }

      deployed: { terminal: true }
```

Starting this workflow auto-executes lint → test → build and returns
the response at `ready_to_deploy` with the full chain trace. The LLM
only sees the deploy decision.

The response includes a `chain` array recording each auto-executed step:

```json
{
  "chain": [
    { "fromState": "lint",  "transition": "run_lint",       "toState": "test",  "version": 2 },
    { "fromState": "test",  "transition": "run_tests",      "toState": "build", "version": 4 },
    { "fromState": "build", "transition": "build_artifact", "toState": "ready_to_deploy", "version": 6 }
  ]
}
```

**Mixed states stop the chain.** If a state has both deterministic and
non-deterministic transitions, the chain stops — it's a decision point
for the LLM or human.

**Deterministic transitions are hidden from links.** The LLM never
sees them in the `links` array during normal operation. On chain
failure, the failed transition is surfaced as a recovery link so the
LLM can retry it.

**No actor gate on submit.** Deterministic transitions can still be
submitted manually via `praxec.command` — this is the recovery path
when a chain fails mid-execution.

`maxChainDepth` (default 50) caps how many deterministic steps run in
a single chain invocation. Set it lower for workflows where runaway
loops are a concern.

See `examples/deploy-pipeline/` for a full worked example.

---

## Phase guidance

States can declare `goal` and `guidance` strings that surface in every
workflow response as contextual instructions for the LLM:

```yaml
states:
  ready_to_deploy:
    goal: Confirm deployment
    guidance: >
      All automated checks passed. Review the lint report, test
      results, and build artifact before deciding to deploy.
    transitions: { … }
```

The response includes a `guidance` object:

```json
{
  "guidance": {
    "goal": "Confirm deployment",
    "instructions": "All automated checks passed. Review the lint report..."
  }
}
```

Phase guidance complements `prefill` (which pre-shapes *arguments*)
by pre-shaping the LLM's *reasoning* about what to do next. Use
`goal` for the one-line objective and `guidance` for detailed
instructions. Both are optional and independent.

`goal` and `guidance` text is indexed by the discovery system, so
`praxec.query` search queries can match against it.

---

## Workflow timeouts

```yaml
workflows:
  approval:
    timeoutMs: 86400000          # 24h, lazy: checked on next submit/get
    onTimeout:
      target: timed_out
    initialState: pending
    states:
      pending: { … }
      timed_out: { terminal: true }
```

When the next operation occurs past the deadline, the runtime
auto-transitions to `onTimeout.target`, emits `workflow.timed_out`, and
short-circuits the caller's submit. No background scheduler.

---

## Link filtering

```yaml
workflows:
  demo:
    linkFilter: byGuards         # workflow-wide
    states:
      triaged:
        linkFilter: byGuards     # per-state override (state wins)
```

`byGuards` runs each transition's guards silently and returns only
links that would currently pass. Default is `all`.

---

## Reliability policy

Lives on `proxy.expose[].reliability`, transition `reliability`, and
state `onEnter.reliability`.

```yaml
reliability:
  timeoutMs: 30000
  retry:
    maxAttempts: 3
    backoff: exponential                # none | fixed | exponential
    initialDelayMs: 500
    maxDelayMs: 5000
    retryOn: [timeout, transient_error, rate_limited]
    # available classes: timeout, transient_error, rate_limited, connection_error
  fallback:
    strategy: first_success             # only strategy implemented today
    executors:
      - { kind: rest, connection: github_rest, method: POST, path: "/repos/{o}/{r}/pulls" }
```

Semantics in [governance.md](governance.md#reliability-timeout--retry--fallback).

---

## Capabilities

Named, reusable capability definitions:

```yaml
capabilities:
  raw.create_pr:
    title: Create GitHub PR
    description: Open a pull request on GitHub.
    tags: [github, write]
    inputSchema:
      type: object
      required: [title]
      properties: { title: { type: string } }
    executor:
      kind: mcp
      connection: github
      tool: create_pull_request

  safe.create_pr:
    wraps: raw.create_pr                  # inherits executor + base guards
    guards: [{ kind: evidence, requires: [tests_passed] }]
    reliability: { retry: { maxAttempts: 3 } }
```

Reference from `proxy.expose[]`:

```yaml
proxy:
  expose:
    - capability: safe.create_pr
      as: github.create_pr                # alias; defaults to capability name
      tags: [primary]                     # extra tags for discovery
      aliases: [pr, pull-request]         # search synonyms for discovery
```

Reference from a workflow transition:

```yaml
transitions:
  create_pr:
    target: review
    executor: { capability: safe.create_pr }
```

`wraps:` stacks: parent's guards run first, then the wrapper's, then
the calling transition's. All must pass. Reliability is "more specific
wins" — transition > wrapper > base.

For design patterns around composing capabilities into larger systems,
see [../architecture/mcp-control-architecture.md](../architecture/mcp-control-architecture.md).

---

## Persistent stores

Workflow instance state across restarts:

```yaml
store:
  kind: memory                          # memory | file | sqlite
  path: /var/lib/praxec.sqlite        # required when kind is file or sqlite
```

| Kind     | Notes                                                                                                |
|----------|-------------------------------------------------------------------------------------------------------|
| `memory` | Default. Fast, no durability — workflow state is lost on restart.                                     |
| `file`   | One JSON file per workflow under `path`. Atomic-rename writes; optimistic locking on `version`.       |
| `sqlite` | Single SQLite file (WAL mode). Bundled — no system libsqlite required. Transactional version-checked upsert. The only backend with durable evidence + acknowledgment stores. |

The `WorkflowStore` is a trait, so a custom backend (Redis, a SQL database)
plugs in without changing the runtime.

---

## Discovery

```yaml
discovery:
  index: memory                         # only "memory" is implemented today
  include: [proxy, workflows, connections]
```

`praxec.query` lexically scores items against the query (title 6× /
id 5× / tags 3× / description 2× / freeform indexed text 1×). The trait
is `DiscoveryIndex` so a Tantivy or vector backend can replace the
in-memory default.

---

## Audit

```yaml
audit:
  sink: stderr                          # stderr | memory | file | none
  path: /var/log/audit.jsonl            # required when sink: file
```

Event taxonomy in [governance.md](governance.md#audit).

---

## include — multi-file composition

```yaml
include:
  - base.connections.yaml
  - team.policy.yaml
  - workflows/governed-change.yaml
```

Includes deep-merge in declaration order, then the main file's body
overrides on top.

- **Maps** merge recursively. Later wins on key collisions.
- **Arrays** concatenate. (`proxy.expose`, `proxy.import`,
  `workflows.X.states.Y.transitions.Z.guards`, etc.)
- **Scalars**: later wins.

Cycles raise an error. Includes don't work with compile-time embedding
(`include_str!`) — see [../guides/embeddings.md](../guides/embeddings.md) for the pre-merge
pattern.

### Remote / verified includes

An `include:` entry may be an object `{ uri, hash }` instead of a path string:

```yaml
include:
  - base.yaml                                   # local path (relative); hash optional
  - uri: "https://raw.githubusercontent.com/<org>/frontrails-praxec-pack/<ref>/frontrails.yaml"
    hash: "sha256:<64-hex>"                     # REQUIRED for non-file:// URIs
```

Schemes: `file://` (relative to the including file), `https://` (load-time GET),
and `git+https://<host>/<repo>@<ref>#<path>` (load-time `git archive` — note many
forges disable this; prefer `https://` raw URLs for GitHub/GitLab). Non-`file://`
URIs **require** a `sha256:` hash; the merged config is rejected
(`INCLUDE_HASH_MISMATCH`) if the fetched bytes don't match. Unsupported schemes
are rejected (`UNSUPPORTED_INCLUDE_URI_SCHEME`); malformed `git+https` URIs are
rejected (`INVALID_GIT_HTTPS_URI`). Merge, deep-merge, array concatenation, and
cycle detection are identical to path includes.

---

## Backing up the store

The `WorkflowStore` holds all active workflow instances. Backup strategy
depends on the store kind:

| Store kind | Backup approach |
|------------|-----------------|
| `memory` | No on-disk state to back up. Workflows are ephemeral — lost on restart. Use only for development or stateless proxy-only deployments. |
| `file` | Each workflow is one JSON file under the configured `path` directory. Use your filesystem's standard backup tool (rsync, restic, borg) to snapshot the directory. Atomic-rename writes mean a snapshot taken mid-write is either the old or new version — never a partial file. |
| `sqlite` | The SQLite database is a single file (WAL mode). Use `.backup` for online backups: `sqlite3 /path/to/store.sqlite ".backup /backup/dir/praxec-$(date +%Y%m%d).sqlite"`. For filesystem-level snapshots, ensure WAL checkpoint completes first: `sqlite3 /path/to/store.sqlite "PRAGMA wal_checkpoint(TRUNCATE);"`. |

> **Restoring:** Stop the gateway, replace the store file/directory with
> the backup, and restart. The gateway re-reads all workflow instances
> from the store on first access. Workflows that completed between the
> backup and the restore will be re-created in their pre-completion
> state — the gateway handles this gracefully (stale versions are
> rejected, and the caller can re-submit).

---

## Reloading configuration

Send **SIGHUP** to reload config without restarting. The gateway
re-reads the YAML file, rebuilds definitions, executors, connections,
and the discovery index, then swaps them in atomically. In-flight
workflows continue uninterrupted. A `config.reloaded` audit event is
emitted on success.

```bash
# Reload after editing the config
kill -HUP $(pidof praxec)
```

If the config file fails to parse or resolve, the error is logged and
the gateway keeps running with the previous config.

**What changes on reload:** workflow definitions, executor connections
(MCP/CLI/REST), proxy imports, and the discovery index.

**What survives unchanged:** the workflow store (in-flight instances),
the evidence store, the audit sink, and the drain state.

> **Note:** SIGHUP is Unix-only. On Windows, use the supervisor restart
> approach below.

### Zero-downtime restart (alternative to SIGHUP)

1. Update the gateway config file on disk.
2. Validate: `praxec check --config gateway.yaml`.
3. Send SIGTERM (or Ctrl+C) to the running process.
4. The gateway refuses new start calls (`praxec.command` with a
   `definitionId`) with a clean error. In-flight `praxec.command` /
   `praxec.query` calls continue until the drain deadline (default
   30 s; override with `PRAXEC_DRAIN_DEADLINE_SECS`).
5. After the deadline, the gateway closes the MCP transport and exits.
6. Your supervisor starts a new process with the updated config.
7. With a persistent store, active workflows resume from disk on the
   next `praxec.query` (get) or `praxec.command` (submit) call.

### Validating configs

Always validate a config before deploying it:

```bash
praxec check --config /etc/praxec/gateway.yaml
```

Add this step to your CI/CD pipeline to catch errors before they reach
production. The `check` subcommand verifies:
- The `version` field is present
- All YAML parses correctly
- All `include:` references resolve
- All capability references resolve
- All workflow definitions compile
- Workflow graph integrity: unreachable states, dangling transition
  targets, dead-end non-terminal states

---

## Health checking

The gateway exposes health information through its MCP tool surface.
There is no separate HTTP health endpoint — the two MCP tools
(`praxec.query` + `praxec.command`) are the health surface.

### Liveness

If the gateway process is running and responding to MCP `tools/list`,
it's alive. Any MCP host's built-in health monitoring (reconnection,
timeout detection) is sufficient for liveness detection.

### Readiness

The gateway is ready when it has:
1. Loaded and resolved the config
2. Established all MCP connections (imports may fail — the gateway
   starts with whatever succeeded)
3. Initialized the `WorkflowStore`
4. Started listening on its transport

All of this happens during startup. If the process exits with a
non-zero status, readiness failed. Check the startup logs for details:

```bash
RUST_LOG=praxec=debug praxec serve --config gateway.yaml
```

### Deep health check

For a more thorough check, call the home operation
(`praxec.query` with empty args) and verify the response:

```bash
# Using an MCP client or a tool like `mcp-cli`:
mcp-cli call praxec.query '{}'

# Expected response:
# { "links": [{ "rel": "search", "method": "praxec.query", ... }] }
```

A failed home call indicates the runtime is not functioning
correctly — the process should be restarted.

### Prometheus-style health

For Prometheus-based monitoring, see
[examples/audit-to-prometheus/](../examples/audit-to-prometheus/) for
converting audit events into metrics that can drive alerting rules.

---

## Where to next

- Mental model: [../architecture/concepts.md](../architecture/concepts.md)
- Governance knobs in depth: [governance.md](governance.md)
- Connection patterns and importing: [../guides/connections.md](../guides/connections.md)
- Composing for larger systems: [../architecture/mcp-control-architecture.md](../architecture/mcp-control-architecture.md)
- Embedding the library: [../guides/embeddings.md](../guides/embeddings.md)
