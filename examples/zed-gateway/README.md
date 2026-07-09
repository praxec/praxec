# Zed Editor Gateway

A complete praxec configuration that gates all AI tool access
in [Zed](https://zed.dev) behind a governed proxy surface.

## What this does

When you connect Zed to this gateway (and **only** this gateway), the AI
assistant sees exactly the tools named in the config's `proxy.expose`
block. Every downstream capability — reading files, running tests,
creating PRs — is a proxied tool or a link in a response payload, not a
separate raw MCP server.

| Action | How |
|--------|-----|
| Read files | `gateway.search` → `workflow.start(proxy_default)` → follow `fs.read` link |
| Run tests | Same, follow `test.run` link |
| Start a TDD cycle | `workflow.start(tdd)` → follow red → green → refactor links |
| Start a governed PR | `workflow.start(governed_change)` → plan → test → human approval |

The model **cannot**: write files without tests passing, create a PR
without tests, run arbitrary shell commands, skip TDD phases, or merge
without human approval.

## Setup

### Prerequisites

- [Zed editor](https://zed.dev)
- Rust toolchain (for building `praxec`)
- Node.js (for `npx`-based MCP servers)

### 1. Build and validate

```bash
cargo build --release -p praxec
praxec check --config examples/zed-gateway/gateway.yaml
```

### 2. Configure Zed

Edit `~/.config/zed/settings.json`:

```json
{
  "context_servers": [
    {
      "id": "praxec",
      "executable": "/absolute/path/to/praxec",
      "args": ["serve", "--config", "/absolute/path/to/examples/zed-gateway/gateway.yaml"]
    }
  ]
}
```

### 3. Verify

Open Zed and ask: *"What tools do you have available?"* — should list
exactly the tools named in the config's `proxy.expose` block.

Or run the automated check:

```bash
bash examples/zed-gateway/verify.sh
```

## Hardening

**This only works if praxec is your ONLY MCP source in Zed.** If you
also configure a raw `github-mcp-server` or `filesystem` server, the
model gets those tools directly and routes around governance.

Verify: `~/.config/zed/settings.json` should have exactly one
`context_servers` entry with `"id": "praxec"`.

## Audit trail

```bash
# View all events
cat ~/.local/share/praxec/audit.jsonl | jq .

# Tail in real time
praxec audit tail --config examples/zed-gateway/gateway.yaml

# Filter for approval requests
cat ~/.local/share/praxec/audit.jsonl | jq 'select(.event_type == "human.approval.requested")'
```

## See also

- [Config reference](../../docs/reference/configuration.md)
- [Governance knobs](../../docs/reference/governance.md)
- [TDD example](../tdd/)
- [Content publishing example](../content-publish/)
