# Development

Working on the codebase, running tests, and what the trait seams give
you for future work.

---

## Workspace layout

```
crates/
  praxec-schema/        typify-generated types from schemas/*.json
  praxec-core/          runtime, ports, audit, reliability, discovery,
                              capability, evidence, in-memory + file + sqlite stores,
                              config preprocessor (capabilities / wraps / include)
  praxec-executors/     cli, mcp (process + HTTP), rest, human, noop,
                              registry, import (tools/list)
  praxec-agents/        kind: agent subprocess executor (default-on)
  praxec-llm-executor/  governed in-runtime kind: llm executor
  praxec-embeddings/    embedding-backed DiscoveryIndex
  praxec-mcp-server/    PraxecServer (rmcp ServerHandler) — the two tools
  praxec-cockpit/       praxec-cockpit binary (mediator/cockpit)
  praxec-cockpit-mcp/   praxec-cockpit-mcp binary (cockpit MCP server)
  praxec-tui/           praxec / praxec-tui binaries (ratatui cockpit)
  praxec-test/          shared test harness + fuzz fixtures
  praxec/               binary: praxec (serve | check)

The default-on `agents` cargo feature folds the retired pro-gateway
wiring into the `praxec` binary. User-facing binaries: `praxec`
and `praxec-tui` (same source), `praxec-cockpit`, plus the
`praxec` server binary.

schemas/
  praxec-repo.schema.json
  gateway-config.schema.json
  transition-record.schema.json
  workflow-response.schema.json

examples/
  simple-proxy.yaml           proxy mode, one cli + one mcp tool
  governed-change.yaml        full workflow with guards + human approval
  import-and-discovery.yaml   import block across native/npx/uvx/container/HTTP

docs/                         topical deep-dives
```

---

## Common commands

```bash
# Build the whole workspace.
cargo build --workspace

# Run every test across the workspace (suite counts in testing-strategy.md).
cargo test --workspace

# Lint with all warnings denied.
cargo clippy --workspace --all-targets -- -D warnings

# Validate a config without serving.
cargo run -p praxec -- check --config examples/simple-proxy.yaml

# Serve over stdio (logs go to stderr, MCP wire protocol on stdout).
cargo run -p praxec -- serve --config examples/simple-proxy.yaml

# Tracing filter — defaults to info; everything goes to stderr.
RUST_LOG=praxec=debug cargo run -p praxec -- serve --config examples/simple-proxy.yaml
```

---

## Test layout

| File                                                           | What it covers                                                       |
|----------------------------------------------------------------|-----------------------------------------------------------------------|
| `crates/praxec-core/tests/invariants_actor_audit.rs`, `invariants_governance.rs`, `invariants_proxy.rs` | The invariants suite (actor/audit, governance, proxy) + audit emission |
| `crates/praxec-core/tests/composability.rs`              | `capabilities:`, `wraps:`, `include:`, capability refs                |
| `crates/praxec-core/tests/discovery.rs`                  | `praxec.query` search / describe / home                             |
| `crates/praxec-core/tests/capability.rs`                 | Capability registry + proxy compilation from registry                 |
| `crates/praxec-core/tests/evidence_guard.rs`             | End-to-end evidence guard                                             |
| `crates/praxec-core/tests/persistent_stores.rs`          | File + SQLite WorkflowStore round-trips                               |
| `crates/praxec-executors/tests/rest_executor.rs`         | REST executor (wiremock-driven)                                       |
| `crates/praxec-executors/tests/human_audit.rs`           | Human executor's `human.approval.requested` event                     |
| `crates/praxec-mcp-server/tests/stable_tool_surface.rs`  | Invariant 9 — tool list is exactly the documented two                 |

When adding a feature, mirror this taxonomy: one test file per topic,
fail-loud assertions, real backends where cheap (wiremock for HTTP,
tempfile for filesystem, in-memory SQLite for the DB).

---

## Schema regeneration

`praxec-schema/build.rs` reads `schemas/*.json` and emits Rust
types via [typify](https://github.com/oxidecomputer/typify). Edits to
the schemas trigger a rebuild of the schema crate automatically.

If you change a schema, you'll usually also need to:

1. Update `praxec-core/src/config.rs` if the field affects
   resolution (capabilities, wraps, include, etc.).
2. Update `crates/praxec-core/src/runtime.rs` or downstream
   consumers if it changes runtime behavior.
3. Add an example in `examples/`.
4. Update the relevant `/docs/*.md`.

---

## Status

What's implemented:

- Two link layers (HATEOAS-inspired): reads via `praxec.query`
  (home/search/describe/get/explain) for discovery and writes via
  `praxec.command` (start/submit/define) for action.
  Stable two-tool surface.
- Configurable proxy and multi-state governed workflows (one engine).
- Connection runtimes: `mcp` over child process or Streamable HTTP URL,
  `cli` for any process, `rest` for any HTTP endpoint.
- Vendor-neutral `proxy.import`: connect to any MCP server and import
  its `tools/list` as proxy capabilities.
- Reliability per executor invocation: timeout, retry (none / fixed /
  exponential), fallback executors with `first_success`.
- Audit taxonomy with stderr / file / memory / null sinks.
- Evidence store backing the `evidence` guard.
- Persistent `WorkflowStore`: in-memory, file-backed, SQLite.
- Lexical `DiscoveryIndex` over workflows / capabilities / connections.
- Composability: named `capabilities:`, capability references in
  exposures and workflow executors, `wraps:` for stacking policy,
  `include:` for multi-file config composition.

Trait seams left for future work — implement the trait and drop in:

- Distributed / networked `WorkflowStore` (Redis, a SQL database, …)
- Vector / hybrid `DiscoveryIndex` (Tantivy, embeddings)
- Persistent `EvidenceStore` (file / DB-backed)
- Postgres / Kafka / OTel `AuditSink`
- Domain-specific `Executor` and `GuardEvaluator` impls

---

## Where to next

- The runtime contract: [../reference/invariants.md](../reference/invariants.md)
- Embedding the crates as a library: [../guides/embeddings.md](../guides/embeddings.md)
- Composing for larger systems: [../architecture/mcp-control-architecture.md](../architecture/mcp-control-architecture.md)
