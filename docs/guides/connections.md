# Connections

How the gateway reaches downstream services. The gateway is
**vendor-neutral**: any MCP server, any CLI, any HTTP endpoint fits
behind one connection definition.

---

## Connection kinds

| Kind   | What it reaches                                                          |
|--------|---------------------------------------------------------------------------|
| `mcp`  | Another MCP server, over child process or HTTP (Streamable HTTP transport) |
| `cli`  | Any process the shell can find                                            |
| `rest` | Any HTTP endpoint with `baseUrl` + optional `headers`                     |

---

## MCP connections: five spawn patterns, all first-class

For `kind: mcp`, the gateway doesn't care **how** the MCP server is
reached — just that it speaks MCP. Five patterns, all equally
first-class:

```yaml
connections:
  # Native binary on PATH.
  github:
    kind: mcp
    command: github-mcp-server

  # Anything distributed via npm.
  filesystem:
    kind: mcp
    command: npx
    args: [-y, "@modelcontextprotocol/server-filesystem", "/tmp"]

  # Anything distributed via PyPI.
  fetcher:
    kind: mcp
    command: uvx
    args: [mcp-server-fetch]

  # Containerized — Docker, podman, nerdctl, apptainer all look the same.
  postgres:
    kind: mcp
    command: docker
    args: [run, -i, --rm, -e, DATABASE_URL, mcp/postgres:latest]
    env: { DATABASE_URL: postgres://… }

  # Hosted MCP server reached over HTTP.
  search:
    kind: mcp
    url: https://mcp.example.com/v1
```

**Lifecycle.** Process-launched connections spawn lazily on first use
and are reused for the gateway's lifetime. URL-launched connections
open a streamable-HTTP session on first use and reuse the same session
afterward.

There's no special handling per runtime — if you can express how to
reach the server, the gateway can use it.

---

## CLI connections

```yaml
connections:
  dotnet:
    kind: cli
    command: dotnet
    workingDirectory: /repo
    env: { CI: "1" }
```

`workingDirectory` and `env` are optional. CLI executors interpolate
`$.arguments.*` / `$.context.*` / `$.workflow.input.*` into their args.

---

## REST connections

```yaml
connections:
  github_api:
    kind: rest
    baseUrl: https://api.github.com
    headers:
      Authorization: "Bearer ${GITHUB_TOKEN}"
      Accept: application/vnd.github+json
```

REST executors then refer to this connection and fill in method/path/
query/body. See [../reference/configuration.md](../reference/configuration.md#executor-kinds) for the executor
shape.

---

## Importing tools you didn't write

The most powerful connection feature: ask the gateway to walk a
downstream MCP server, list its tools, and turn each into a proxy
capability — automatically.

```yaml
proxy:
  import:
    - connection: github
      prefix: github
      include: [list_issues, create_issue, create_pull_request]
      tags: [github, source-control]

    - connection: filesystem
      prefix: fs
      tags: [filesystem]
```

At startup the gateway connects to each named connection, calls the
standard `tools/list` MCP method, and turns every returned tool into a
`Capability` with `source: Imported { connection, tool }`. Each becomes
a transition in `proxy_default` and joins the discovery index, so
the search operation (via `praxec.query`) and submit (via
`praxec.command`) can use it just like a declared exposure.

### Filtering knobs

| Field      | Effect                                                          |
|------------|------------------------------------------------------------------|
| `include`  | Allowlist. Empty = all tools allowed.                            |
| `exclude`  | Denylist. Applied after `include`.                               |
| `prefix`   | Names returned tools `<prefix>.<tool>` so they don't collide.    |
| `tags`     | Tags applied to every imported capability (helps discovery).     |

### Resilience

Each successful import emits a `capability.discovered` audit event.
Connection failures emit `capability.discovery_failed` and the gateway
keeps starting with whatever did succeed — one broken downstream MCP
server can't take down the whole gateway.

### Adding governance to imported tools

When you want governance on an imported tool, declare a workflow whose
transition uses the same `executor: { kind: mcp, connection: …, tool: … }`
and add guards / reliability / output mapping there. Or, more
ergonomically, declare a named capability that wraps the imported tool
with policy:

```yaml
capabilities:
  safe.create_pr:
    wraps: github.create_pull_request   # imported above
    guards: [{ kind: evidence, requires: [tests_passed] }]
```

See [../architecture/mcp-control-architecture.md](../architecture/mcp-control-architecture.md) for the
design patterns around composing imports with policy.

## Managing connections from the CLI (stage → grant → revoke)

Beyond declaring connections in the config file, praxec governs adding them as an
explicit operator trust flow:

- `praxec connections add --config <cfg> --name <n> --kind mcp --command …` —
  **stages** a connection under `stagedConnections:` (inert — never in the live
  registry until granted). `--block '<json>'` stages a whole connection body
  (including `env:`) in one argument.
- `praxec connections grant --config <cfg> <n>` — the explicit, auditable
  operator **trust act** that promotes a staged connection into the live
  `/connections` registry (records a `connections.granted` audit event;
  fail-closed with `GRANT_REQUIRES_OPERATOR` when run non-interactively).
- `praxec connections revoke --config <cfg> <n>` — the mirror: un-grants a
  connection (records `connections.revoked`).

`praxec doctor` additionally validates a connection's declared `required_secrets:`
(the env-var names it needs at runtime, resolved from `providers.env` — never
secret values in config).

## Discovering and provisioning tools

You don't have to hand-write every connection. praxec can **discover** MCP tools
from configured `registries:` and **provision** them as prebuilt binaries (or
docker images) through the governed stage→grant flow above:

- `praxec.query { discover: "<text>" }` / `{ evaluate: { verbs: [...] } }` — find
  installable tool candidates from the `registries:` catalog (adapters:
  `github-org`, `static`, `rest`, `mcp-registry`).
- `flow.tools.provision` — a governed workflow that installs, collects
  secrets/config, stages, validates, then grants a chosen candidate (community
  candidates run through an extra double-approval gate);
  `flow.tools.deprovision` reverses it.

### The `praxec/packs` registry + the installer

The workflows a pack ships (cpm-planner, fmeca-mcp, …) declare the MCP tools they
need. praxec resolves and installs those tools from the central **`praxec/packs`
registry** — sourced **always-latest** so a newly released tool or a newly added
dependency shows up without an operator edit:

```yaml
discovery:
  registry: { uri: "git+https://github.com/praxec/packs", ref: main }
```

Each tool declares a `providers:` chain (`release`, `docker`, `npx`, `cargo`).
For a fresh machine the installer prefers the **prebuilt release binary** (no
compiler, no Docker daemon required), falling through to docker, then **npx**
(an npm-distributed stdio MCP server — nothing to download or place; the
connection wires `npx -y <pkg>` and npx fetches it on run, gated on `npx` being
on PATH), then — last-resort, emit-only — cargo. Every downloaded binary is
**checksum-verified against the
release `checksums.sha256`** and refused on mismatch, so integrity holds however
the registry was sourced. This verify guarantees the binary matches the
release's published checksum (anti-corruption / transport-integrity over the
release page's TLS) — it is **not** independent provenance or anti-MITM, since
the asset and its `checksums.sha256` come from the same release page.

- `praxec doctor` — reports each required-but-missing tool with the exact
  provider + command it *would* run (offer-only; no install without consent).
- `praxec doctor --fix` — installs the offered tools (consent), verifies, and
  leaves the connection ready.
- `praxec tools install <tool-id>` — install one tool by its registry id. If
  the id isn't in the curated `discovery.registry`, the installer falls back to a
  **discovered** candidate from the configured `registries:` (matched by name),
  normalizing it to a provider coordinate (image→docker, repo/crate→release/cargo,
  npm→npx) and routing it through the **same** installer — so a tool surfaced by
  `praxec.query { discover }` is installable, with the curated registry always
  taking precedence over a same-named discovered candidate.
- `praxec pack list <repo>` — enumerate a pack's `flow.*` and `cap.*` definition
  ids (namespace-prefixed, grouped + counted) WITHOUT loading a full gateway (no
  store, no runtime), so you can see what a pack provides before wiring it under
  `repos:`. Fails fast on a directory with no `praxec.repo.yaml`.
- `praxec init --with-starter-packs` — scaffold a gateway with the starter
  packs' `repos:` + the always-latest `discovery.registry` pointer wired, then run
  the doctor resolve path (offer by default; add `--install-tools` to install).
- `praxec init --packs cognitive-architectures,praxec-meta` — wire a **subset**
  of the known open starter packs by short id (the last `/`-segment of each pack
  uri) + the registry pointer; unions with `--with-starter-packs`, no duplicates.
  An unknown id fails fast, listing the valid ids.
- **`frontrails` is intentionally NOT in the starter set** — it is an
  `include:{uri,hash}` pattern pack that needs licensed FrontRails servers. If you
  are licensed, wire it by hand with `praxec init --pack <uri>` (arbitrary-uri).

See the [tool-discovery reference](../reference/tool-discovery.md) and
[`configuration.md`](../reference/configuration.md#discovery) for the
`discovery.registry` schema.

---

## Where to next

- Full schema reference for connections + executors: [../reference/configuration.md](../reference/configuration.md)
- The trichotomy of capabilities, exposures, and workflows:
  [../architecture/mcp-control-architecture.md](../architecture/mcp-control-architecture.md)
- Reliability and retry semantics for executor calls:
  [../reference/governance.md](../reference/governance.md#reliability-timeout--retry--fallback)
