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

---

## Where to next

- Full schema reference for connections + executors: [../reference/configuration.md](../reference/configuration.md)
- The trichotomy of capabilities, exposures, and workflows:
  [../architecture/mcp-control-architecture.md](../architecture/mcp-control-architecture.md)
- Reliability and retry semantics for executor calls:
  [../reference/governance.md](../reference/governance.md#reliability-timeout--retry--fallback)
