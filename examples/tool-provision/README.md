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

`select → trust_gate → installing → collecting → building → staging → validating → granting → done | failed`.

## Secrets

`collecting` elicits the candidate's `requires.secrets`/`requires.config`
from the operator as env-var **names** the operator has already set in
`providers.env` — never values. No secret material ever lands in this
workflow's YAML, context, or audit trail; `secrets_set` (the workflow
output) is names only.

**The env-wiring gap is closed (P2.3b)**, via a new `building` state
between `collecting` and `staging`: a `kind: script` step
(`build.connection-body`, declared in `flow.tools.provision.yaml`'s own
`scripts:` block) assembles the WHOLE staged-connection body — identity
(`kind`/`command`-or-`url`) *and* the arbitrary-length collected
secrets/config — as one JSON object. `staging` then passes that whole
object to `praxec connections add --block <json>` (a new P2.3b CLI flag,
`crates/praxec-executors/src/conn_write.rs`) as a single templated argv
token, collapsing from 3 kind-guarded `connections add` calls to 1.

This works around the same confirmed grammar limit the earlier version of
this doc described: the `kind: cli` executor's `args:` is still a static
JSON array (no loop/spread construct for an arbitrary-length list into N
repeated flags) — but `building`'s script is a real interpreter (bash +
jq), not that grammar, so the arity problem is solved there instead. The
two collected kinds are wired differently, deliberately:

- `requires.config` values (non-secret) → literal `env:` entries on the
  built body. Safe to store — no secret material.
- `requires.secrets` → `required_secrets: [ENVVAR, ...]` on the built body
  — **names only, never values**. The actual secret reaches the spawned
  connection's child process via the operator's own process environment
  (`std::process::Command` inherits the parent env by default — see
  `cli.rs`/`mcp.rs`), so nothing here ever holds real secret material;
  `required_secrets:` just lets `praxec doctor`'s P2.1 check (advisory)
  report a name that doesn't resolve. Closing this also required adding
  `required_secrets` to `mcpConnection` in
  `schemas/gateway-config.schema.json` — P2.1 added the doctor-side check
  but never the schema property, so granting a connection that declared it
  previously failed `INVALID_STAGED_CONNECTION`.

`rest`-transport candidates get identity only (`kind`/`baseUrl`) —
`restConnection`'s schema has no `env:`/`required_secrets:` analog (no
child process to inherit into, no elicited path to `headers:`), so
`building` fails fast (`BUILD_RECIPE_UNAVAILABLE`) rather than silently
dropping a rest candidate's collected secrets/config.

## Running it

```sh
# Validate the shape (0 errors expected; 2 known, documented warnings —
# see the inline V36 note on `collecting` and the ephemeral-storage note).
praxec check --config examples/tool-provision/gateway.yaml

# Exercise it with mock executors (permission-guarded `granting` edges are
# expected to show as uncoverable by the static satisfier — the same
# residual as examples/expense-approval; no wedge/livelock/engine-error).
praxec fuzz --config examples/tool-provision/gateway.yaml

# Drive it for real. `praxec`/`npm`/`jq` must be on PATH — see gateway.yaml
# (`jq` backs `building`'s `build.connection-body` script).
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
`install_npm`, `submit_collection`, `build_stdio_mcp`, `stage_connection`,
`validate`, and finally — as a human principal — `approve_grant`) exactly
as any other praxec workflow.
