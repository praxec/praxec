# praxec

[![CI](https://github.com/praxec/praxec/actions/workflows/ci.yml/badge.svg)](https://github.com/praxec/praxec/actions/workflows/ci.yml)
[![crates.io](https://img.shields.io/crates/v/praxec.svg)](https://crates.io/crates/praxec)
[![docs.rs](https://docs.rs/praxec/badge.svg)](https://docs.rs/praxec)
[![License: BSD-3-Clause](https://img.shields.io/badge/license-BSD--3--Clause-blue.svg)](LICENSE)

**The AI execution kernel for deterministic, policy-gated workflows.**

praxec runs deterministic workflows over pluggable capabilities — MCP tools, CLIs,
HTTP services, scripts, and native modules — while holding the LLM inside a bounded
execution context. You describe what's allowed as small YAML state machines; at
each step the model is offered only the transitions that are legal right now. An
illegal move isn't rejected after the fact — it's never presented.

It inverts the usual arrangement. Instead of an agent choosing tools from an
ever-growing list while a prompt hopes to keep it in line, the **workflow** chooses
the capability, the **kernel** grants bounded access, the capability executes, and
the kernel **validates the output before the state advances**:

| Most agent systems | praxec |
|---|---|
| LLM chooses tools | Workflow chooses capabilities |
| Prompt governs behavior | Kernel enforces transitions |
| Tool list grows with complexity | Capability surface stays controlled |
| Agent is the orchestrator | Workflow engine is primary |
| Best-effort guardrails | Validated state transitions |

The same YAML gives you schema validation, guards, human-approval gates,
deterministic step-chaining, and an audit log — with your capabilities staying in
whatever language they already use. MCP is one transport into the kernel, not the
whole story: any MCP client (Claude Code, Cursor, or your own) drives it through a
fixed two-tool surface.

Conceptual guides and deeper narrative live at [praxec.dev](https://praxec.dev);
this README covers what it is, how to install it, and how to use it.

## Install

`cargo install` (from crates.io):

```bash
cargo install praxec
```

Pre-built bundle — verify the `praxec` binary against the release's `checksums.sha256`:

| Platform | Download |
|----------|----------|
| Linux x86_64 | [`.tar.gz`](https://github.com/praxec/praxec/releases/latest/download/praxec-x86_64-unknown-linux-gnu.tar.gz) |
| Linux ARM64 | [`.tar.gz`](https://github.com/praxec/praxec/releases/latest/download/praxec-aarch64-unknown-linux-gnu.tar.gz) |
| macOS x86_64 | [`.tar.gz`](https://github.com/praxec/praxec/releases/latest/download/praxec-x86_64-apple-darwin.tar.gz) |
| macOS Apple Silicon | [`.tar.gz`](https://github.com/praxec/praxec/releases/latest/download/praxec-aarch64-apple-darwin.tar.gz) |
| Windows x86_64 | [`.zip`](https://github.com/praxec/praxec/releases/latest/download/praxec-x86_64-pc-windows-msvc.zip) |

All releases + checksums: [github.com/praxec/praxec/releases](https://github.com/praxec/praxec/releases/latest).

Docker:

```bash
docker run -v $(pwd)/gateway.yaml:/config/gateway.yaml ghcr.io/praxec/praxec
```

Full matrix (binary verification, Docker, editor wiring): [Installation](https://praxec.dev/installation/).

The `praxec` binary above is the gateway — the MCP server you wire into an agent
host. The optional interactive control-plane TUI ships separately as the `px`
binary (`cargo install praxec-tui`); you don't need it to run the gateway.

## Quick start

```bash
cat > hello.yaml <<'EOF'
version: "1.0.0"
proxy:
  expose:
    - name: hello.echo
      description: Returns the message you sent.
      executor: { kind: noop }
EOF

praxec serve --config hello.yaml
```

Wire it into your editor as an MCP server (`command: praxec`,
`args: ["serve", "--config", "/abs/path/hello.yaml"]`) and two tools appear —
`praxec.query` and `praxec.command`. The model can find and call `hello.echo`
through them, with discovery, schema validation, and audit built in. Copy-paste
config for Zed, Cursor, Claude Desktop, Claude Code, and VS Code:
[Wire praxec into your editor](https://praxec.dev/guides/editors/).

## Run a full pack — one command

Beyond a single tool, get a complete **workflow pack** and every MCP tool it needs
provisioned and wired in one step:

```bash
curl -fsSL https://raw.githubusercontent.com/praxec/packs/main/setup.sh | bash
```

That pulls the `cognitive-architectures` pack (a SWE-lifecycle library) plus its tools
(cpm-planner, fmeca, elicitation, scientific-process), sets up your provider keys, writes a
validated gateway config, and prints the `serve` command. Browse the catalog at
[praxec.dev/packs](https://praxec.dev/packs) or the [pack registry](https://github.com/praxec/packs).

## Two tools

praxec exposes exactly two MCP tools, no matter how many capabilities you wire
in behind it:

| Tool | Purpose |
|------|---------|
| `praxec.query` | Read-only: discover, inspect, and look up anything (search, describe, get workflow, explain transition). |
| `praxec.command` | State-mutating: start a workflow, submit a transition, define a term. |

Capabilities surface through search results and response `links[]` — loaded one at
a time, only when relevant — so the model's tool list stays at two and the token
cost of tool definitions stays flat as you scale. The model searches instead of
scanning, and follows links instead of guessing. That pattern is
[HATEOAS](https://en.wikipedia.org/wiki/HATEOAS): the server tells the client
what's legal next.

## Example

A 9-line `ship_guard` workflow: `unchecked → run_check → checked → ship → shipped`.
`ship` is declared only inside `checked`, so from `unchecked` it is not a move the
agent can make:

```jsonc
// Agent reaches straight for ship, before any check has run:
→ praxec.command { "workflowId": "wf_01H…", "expectedVersion": 1, "transition": "ship" }
← { "result": { "status": "rejected" },
    "error": { "code": "INVALID_TRANSITION",
               "message": "Transition 'ship' is not valid from state 'unchecked'." },
    "links": [ { "rel": "run_check", … } ] }   // refusal still hands back the legal move

// Agent follows the only legal move — and now `ship` exists:
→ praxec.command { "workflowId": "wf_01H…", "expectedVersion": 1, "transition": "run_check" }
← { "workflow": { "state": "checked", "version": 2 },
    "result": { "status": "executed" },
    "links": [ { "rel": "ship", … } ] }
```

Point `run_check` at your real `npm test` (or `cargo`, `pytest`, …) and the gate
has teeth: a red suite leaves you in `unchecked`, so `ship` never becomes
reachable. The same machine expresses TDD, deploy-gating, "CI before merge," and
"human approves the PR."

## Governance

Every capability passes through a state machine. The simplest is one state that
loops to itself — a flat tool call. Add states and rules only when you need
control. One line turns an action into a human-gated approval:

```yaml
proxy:
  expose:
    - name: deploy.prod
      executor: { kind: human, queue: prod-deployments }   # LLM can't fire it; a human resolves the queue
```

The same declarative surface gives you the whole control plane — no glue code, no
per-tool wrapper, your tools in whatever language they already live in:

| You know…                       | Declare…                                                | Reference |
|---------------------------------|---------------------------------------------------------|-----------|
| What input is valid             | `inputSchema` — bad input never reaches the executor    | [Guards](https://praxec.dev/reference/guards/) |
| Who should run this             | Guards: `permission`, `role`, `expr`, `evidence`        | [Guards](https://praxec.dev/reference/guards/) |
| What shouldn't run autonomously | `actor: "human"` — enforced at submit time, not a hint  | [Governance](https://praxec.dev/guides/governance/) |
| How calls fail                  | `reliability:` timeout, retry/backoff, fallback executors | [Executors](https://praxec.dev/reference/executors/) |
| What gets logged                | Audit: every step emits structured JSON automatically   | [Audit](https://praxec.dev/reference/audit/) |
| What order steps come in        | Workflows: states, transitions, output mapping          | [Workflows](https://praxec.dev/guides/workflows/) |
| What the LLM doesn't decide     | `actor: deterministic` — runtime chains it, zero round trips | [Chaining](https://praxec.dev/guides/chaining/) |
| How to reason at a decision     | `goal` / `guidance` per state — pre-shaped context      | [Phase guidance](https://praxec.dev/guides/phase-guidance/) |
| Who is allowed to run anything  | Provenance tiers — the model can never introduce a command | [Trust model](docs/reference/governance.md#execution-trust-model-who-can-run-what) |

## Two surfaces, one set of rules

praxec is two surfaces over the same YAML — same workflows, same guards, same
audit log; operators choose per step:

- The MCP server your external agent drives. Point any MCP client at the gateway
  and it governs your existing coding agent — fixed two-tool surface, everything
  behind it.
- The agentic runtime (`praxec` TUI, default-on `agents` feature on the single
  binary) that runs workflows end-to-end on the platform. Its graph-walking
  interpreter spawns an isolated sub-agent per state, so each model sees only its
  scoped guidance and blackboard — a Qwen-7B editor directed by Sonnet-grade
  planning and reviewed by an Opus-grade critic, each doing only what it's best
  at. → [docs/guides/tui-agent.md](docs/guides/tui-agent.md) · [docs/architecture/research.md](docs/architecture/research.md)

Capabilities (`cap.<verb>.<name>`, typed `snippet: { inputs, outputs }` leaves)
compose into orchestrators (`flow.<name>`) via `kind: workflow` executors. Ship
them as Git repos with a `praxec.repo.yaml` manifest; operators load any number
with a top-level `repos:` block, namespace-prefixed and collision-checked at load.
Two sibling libraries show the shape:
[cognitive-architectures](https://github.com/praxec/cognitive-architectures)
and [praxec-meta](https://github.com/praxec/praxec-meta).
→ [Capabilities & orchestrators](https://praxec.dev/guides/capabilities-and-orchestrators/)
· [Multi-repo loading](https://praxec.dev/guides/multi-repo-loading/)

## Companion MCP tools

Standalone MCP servers that praxec consumes purely over the protocol — no crate
dependency, no coupling. Install one, then wire it into a workflow as a connection
and the model calls its tools like any other:

```yaml
connections:
  planner:
    kind: mcp
    command: cpm-planner   # the installed binary
```

| Tool | What it does | Install |
|------|--------------|---------|
| [`cpm-planner`](https://github.com/praxec/cpm-planner) | Critical Path Method scheduling — earliest/latest starts, slack, critical path, and bottlenecks — plus lock-aware cohort coordination for parallel work. | `cargo install cpm-planner` |

## Worked examples

Runnable configs live in [`examples/`](examples/):

| Example | What it demonstrates |
|---------|---------------------|
| [`content-publish/`](examples/content-publish/) | Governance: draft → brand review → human approval → publish. The LLM's only path is through the workflow. |
| [`expense-approval/`](examples/expense-approval/) | Multi-tenant: two-tier approval, quorum evidence, idempotent payment. |
| [`tdd/`](examples/tdd/) | Discipline: enforced red → green → refactor, [dogfooded in CI](examples/tdd/dogfood-drive.py). |
| [`deploy-pipeline/`](examples/deploy-pipeline/) | Deterministic chaining: lint → test → build auto-execute; LLM only sees the deploy decision. |
| [`swe-agent.yaml`](examples/swe-agent.yaml) | The commodity-LLM coding agent: six states + external tools, the [docs/architecture/research.md](docs/architecture/research.md) thesis made runnable. |

More patterns (`pattern-*`) and the authoring loop are in the same directory ·
[Examples on the site](https://praxec.dev/examples).

## Going to production

The quick-start setup trades durability for speed. For real traffic, swap the
defaults — see [Going to production](https://praxec.dev/guides/production/)
and [docs/reference/configuration.md](docs/reference/configuration.md):

- Durable store: `store: { kind: sqlite, path: … }` — the default `memory` store
  loses state on restart. Backends are `memory`, `file`, and `sqlite`; durable
  governance state requires `sqlite`.
- Audit to disk: `audit: { sink: file, path: … }` — one JSON line per event.
- Validate in CI: `praxec check --config X.yaml` — the V1–V23 validation
  cloud catches dangling targets, unreachable states, type mismatches, and verb
  misuse at load, exiting non-zero. → [Validation rules](https://praxec.dev/reference/validation-rules/)
- Hot reload: `SIGHUP` reloads config without dropping in-flight workflows.
- Running agents in the workflow: point `gateway.models_yaml` at a `models.yaml`
  and set provider keys before you flip on `kind: agent` steps or `auto_drive` —
  `praxec check` fails fast without them. → [Agents & models](docs/guides/agents-and-models.md)

Scope: single-host is production-ready; multiple processes on one host share a
`sqlite` file (WAL). Cross-host HA isn't supported — there's no networked store
backend. Cross-trust-boundary deployments should front the gateway with an
identity proxy (Envoy, OAuth2-proxy).

## Check your workflows

| Command | What it checks |
|---------|---------------|
| `praxec check --config gateway.yaml` | Static validation at load time: schema, reachability, dead-ends. Exits non-zero in CI. |
| `praxec fuzz  --config gateway.yaml` | Structural graph walk (every state reachable, nothing orphaned) + per-transition isolation fuzz (each transition tested alone: fires on valid input, rejects on guard-violating input, handles failure, resolves output, never panics; human-gate branches each covered) + capped integration smoke (one full run). No real model or network needed. |
| `praxec test  --config gateway.yaml --scenarios tests.yaml` | Assert declared properties of a named workflow: `reaches` / `never_reaches` a state, `final_state`, `outcome_met`. |

`fuzz` is deterministic and CI-ready (non-zero exit on any problem). Coverage is
per-transition — linear in the number of edges, not a random traversal.

Full guide: [docs/guides/checking-workflows.md](docs/guides/checking-workflows.md)

## When to use it

Use praxec when you have multiple capabilities and any of these matter: fewer
tokens in context, audit, retries, approval gates, schema validation, or multi-step
workflows. If you have a single MCP server with no governance needs, point the host
at it directly instead.

## Documentation

Narrative and conceptual material lives on
[praxec.dev](https://praxec.dev). The repo carries the deep engineering
docs:

| Topic | Where |
|-------|-------|
| Mental model & concepts | [docs/architecture/concepts.md](docs/architecture/concepts.md) · [/concepts](https://praxec.dev/concepts) |
| Governance, guards & the execution trust model | [docs/reference/governance.md](docs/reference/governance.md) |
| Full config reference | [docs/reference/configuration.md](docs/reference/configuration.md) · [/reference/configuration](https://praxec.dev/reference/configuration/) |
| Connections (MCP/CLI/REST) | [docs/guides/connections.md](docs/guides/connections.md) · [/guides/connections](https://praxec.dev/guides/connections/) |
| LLM-authoring guidance | [docs/guides/llm-guidance.md](docs/guides/llm-guidance.md) |
| Agents & models setup | [docs/guides/agents-and-models.md](docs/guides/agents-and-models.md) |
| Composing at scale | [docs/architecture/mcp-control-architecture.md](docs/architecture/mcp-control-architecture.md) |
| Embedding as a library | [docs/guides/embeddings.md](docs/guides/embeddings.md) · [/advanced/embedding](https://praxec.dev/advanced/embedding/) |
| Runtime invariants | [docs/reference/invariants.md](docs/reference/invariants.md) |
| Agentic runtime (TUI) | [docs/guides/tui-agent.md](docs/guides/tui-agent.md) |
| Commodity-LLM thesis & SWE recipe | [docs/architecture/research.md](docs/architecture/research.md) |
| Design spec | [docs/reference/spec.md](docs/reference/spec.md) |
| Working on the codebase | [docs/development/internals.md](docs/development/internals.md) · [CONTRIBUTING.md](CONTRIBUTING.md) |
| Stress tests & invariant coverage | [docs/development/stress-tests.md](docs/development/stress-tests.md) |

## Project

- Status: pre-1.0 (`0.0.x`). Surfaces stabilize per [docs/reference/stability.md](docs/reference/stability.md);
  changes land in [CHANGELOG.md](CHANGELOG.md).
- Performance: microbenchmarks of core-operation overhead in
  [docs/reference/performance.md](docs/reference/performance.md) (`cargo bench --bench gateway_overhead`).
  Throughput under real load is not yet measured.
- Security: report vulnerabilities per [SECURITY.md](SECURITY.md).

## Contributing

Contributions are welcome. See [CONTRIBUTING.md](CONTRIBUTING.md) for the
development setup and workflow, and [docs/development/internals.md](docs/development/internals.md)
for a tour of the codebase.

## License

Licensed under [BSD-3-Clause](LICENSE).
