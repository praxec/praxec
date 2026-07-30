# flow.tools.provision

Phase 2 of the MCP tool lifecycle
(`docs/design/2026-07-30-mcp-tool-lifecycle-phase2.md`): takes a discovered
`ToolCandidate` (Phase 1 `tool_catalog`) and provisions it into a live,
usable praxec connection — for the trusted lane only (`verified`/`org`
candidates; `community` is refused, Phase 3 territory).

## What it composes, not what it writes

The workflow never writes `connections:` / `stagedConnections:` YAML
itself. It composes praxec's own connection-governance CLI, two calls
apart on purpose:

1. **`staging`** runs `praxec connections add` — this **stages** the
   connection: an inert body under `stagedConnections:`, never in the live
   `/connections` registry, so a spawn attempt fails typed with the grant
   remedy until it's granted.
2. **`validating`** runs `praxec doctor` against the staged connection —
   the poka-yoke: nothing is granted until its declared secrets actually
   resolve.
3. **`granting`** runs `praxec connections grant` — **the** operator trust
   act. This is a human gate (`actor: human`): in a headless drive it
   parks exactly like any other HITL step, and the operator's approval
   *through the workflow* is what lets the composed CLI call pass
   `--yes` (a non-interactive grant is otherwise refused fail-closed with
   `GRANT_REQUIRES_OPERATOR`). Declining never invokes the CLI — nothing
   is wired.

`select → trust_gate → installing → collecting → staging → validating → granting → done | failed`.

## Secrets

`collecting` elicits the candidate's `requires.secrets`/`requires.config`
from the operator as env-var **names** the operator has already set in
`providers.env` — never values. No secret material ever lands in this
workflow's YAML, context, or audit trail; `secrets_set` (the workflow
output) is names only.

**Known scope limit**, confirmed against the executor source (see the
comment block at the top of `flow.tools.provision.yaml`): `staging`'s
composed `connections add` call templates only the connection *identity*
(name/kind/command-or-url). The `kind: cli` executor's `args:` is a static
JSON array where each entry resolves to exactly one argv token (a whole
path read or a literal) — there's no loop/spread construct to turn an
arbitrary-length `requires.secrets` list into N repeated `--env
NAME=VALUE` flags, and no string-templating inside one token. Candidates
with empty `requires` (the common case) provision fully automatically
end to end; candidates that declare secrets/config get them fully
elicited and recorded (`secrets_set`), but wiring them into the staged
connection's `env:` block needs either a core `add_connection`-with-JSON
primitive or an `argsFrom` capability on the executor — neither exists
today, so this reference doesn't fake it.

## Running it

```sh
# Validate the shape (0 errors expected; 2 known, documented warnings —
# see the inline V36 note on `collecting` and the ephemeral-storage note).
praxec check --config examples/tool-provision/gateway.yaml

# Exercise it with mock executors (permission-guarded `granting` edges are
# expected to show as uncoverable by the static satisfier — the same
# residual as examples/expense-approval; no wedge/livelock/engine-error).
praxec fuzz --config examples/tool-provision/gateway.yaml

# Drive it for real. `praxec`/`npm` must be on PATH — see gateway.yaml.
praxec command --config examples/tool-provision/gateway.yaml \
  '{"definitionId":"flow.tools.provision","input":{
     "candidate": {
       "name": "browser-mcp",
       "transport": "stdio",
       "source": { "npm": { "pkg": "@playwright/mcp" } },
       "trust_tier": "verified",
       "requires": { "secrets": [], "config": [] }
     },
     "config_path": "examples/tool-provision/gateway.yaml"
  }}'
```

Then follow the returned `links` (submit `resolved`, `proceed_verified`,
`install_npm`, `submit_collection`, `stage_stdio_mcp`, `validate`, and
finally — as a human principal — `approve_grant`) exactly as any other
praxec workflow.
