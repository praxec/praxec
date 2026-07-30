# MCP Tool Lifecycle — Phase 2: Governed Provisioning

**Status:** draft for review · **Date:** 2026-07-30 · **Builds on:** Phase 1 (`tool_catalog` — PR #167) + the [lifecycle design](./2026-07-30-mcp-tool-lifecycle.md) §3.4

## 1. Scope

Phase 1 made tools *discoverable* (read-only catalog + `discover`/`evaluate`). Phase 2 makes a discovered tool **usable** through a governed, HITL-gated workflow — for the **trusted lane only** (`verified`/`org` candidates). Community/third-party provisioning (sandbox + approval) is Phase 3.

Deliverables:
1. `flow.tools.provision` — a governed workflow: select → trust-gate → install → collect secrets/config → validate → wire.
2. `flow.tools.deprovision` — reverse it.
3. **Connection secrets** — extend `providers.env` to arbitrary connection secrets.
4. **Governed config mutation** — the engine capability to safely write a `connections:` entry into the gateway config.
5. **TTL tool-update** — currency (v0.0.43) on a schedule; trust-gated auto-update for already-approved tools.

## 2. The two hard parts (everything else composes existing seams)

Most of Phase 2 reuses what praxec already has — `connections grant` / proxy-import (wiring), `provider_keys` (secrets), `hitl` (elicitation), `hot_reload` (reload). Two capabilities are genuinely new and carry the risk:

### 2.1 Governed config mutation (`connections:` write)
Provisioning must add a `connections:` entry to the operator's gateway config — praxec writing its **own** config. This is the highest-stakes new capability, so it is built as a narrow, safe primitive:

- **Atomic + validated + reversible.** Write to a temp file, run the full config validation (`praxec check` equivalent) against the candidate result, and only then swap — never leave a half-written or invalid config. Keep a timestamped backup of the prior config.
- **HITL-gated.** The mutation is the workflow's one irreversible-ish step; it is an `actor: human` gate that shows the exact diff (the connection block to be added, secrets as `env:`-refs only) and waits for explicit operator approval before the swap.
- **Additive-only in Phase 2.** Provision only *adds* a connection; deprovision only *removes* the one it added (matched by provenance). No edits to unrelated blocks.
- **Reload, don't restart.** After the swap, `hot_reload` picks up the new connection (the reload seam already exists), so a live gateway gains the tool without a restart where possible; otherwise the operator is told to restart.

### 2.2 Governed install execution
Installing a tool *runs code* on the operator's box (`cargo install` / `docker pull` / `npm i` / clone+build). In Phase 2 this is **trusted-lane only** (verified/org), executed via a deterministic `kind: script` step from the candidate's `install_recipe`, bounded (timeout + the v0.0.41/0.0.42 process-group kill so it can't hang or orphan), with output surfaced to the operator. Third-party install (untrusted, sandboxed) is Phase 3 — the trust gate (§3, state `trust_gate`) hard-stops community candidates here.

## 3. `flow.tools.provision` — the workflow

`initialState: select`. Every state is deterministic except the two human gates.

| state | does | gate/outcome |
|---|---|---|
| `select` | resolve the input candidate (by id from `discover`/`evaluate`, or an inline `direct` candidate) | → `trust_gate` |
| `trust_gate` | branch on `trust_tier` | `verified`/`org` → `installing`; `community` → `failed` (`COMMUNITY_REQUIRES_PHASE3`); unknown → `failed` |
| `installing` | run `install_recipe` per transport, bounded (`kind: script`) | ok → `collecting`; non-zero → `failed` (`INSTALL_FAILED`, output attached) |
| `collecting` | **elicit** the candidate's `requires.secrets` + `requires.config` from the operator (`hitl`) — secrets to `providers.env` (env-refs), config values captured | → `validating` |
| `validating` | doctor credential-check on the declared secrets + a bounded `initialize` connect probe with the collected config | ok → `wiring`; missing secret / unreachable → `failed` (`VALIDATION_FAILED`) — nothing is wired |
| `wiring` | **human gate**: show the exact `connections:` block (secrets as `env:`-refs) + the config diff; on approval, atomic+validated write + `hot_reload` | approved → `done` (`succeeded`, provenance recorded); declined → `failed` (`OPERATOR_DECLINED`) |

Inputs: `candidate` (object or catalog id), `config_path` (which gateway config to mutate). Outputs: `connection_name`, `wired` (bool), `secrets_set` (names only), `reason` (on failure).

The validate-before-wire order is the poka-yoke: a tool is never wired into a live config until its secrets resolve and it actually answers an `initialize` — the same fail-before-use stance as the browser preflight and the provider-credential guard.

## 4. `flow.tools.deprovision`

`select` (the connection by name/provenance) → `human confirm` (show what will be removed) → `unwire` (atomic config write removing the block) → optional `uninstall` (per transport: `cargo uninstall` / `docker rmi` / nothing for npx) → optional `purge_secrets` (or retain, operator's choice) → `done`. Deprovision only removes a connection **it provisioned** (provenance-matched); it refuses to remove a hand-authored connection.

## 5. Connection secrets (extend `providers.env`)

`providers.env` is already an env-var file loaded into the process env at startup. Phase 2 lets it hold arbitrary **connection** secrets (`FIGMA_TOKEN=…`), not just provider keys. A connection references them from its `env:` block. The doctor credential-check (`guard_provider_credentials`, generalized to a `required_secrets` list a connection/candidate declares) validates resolvability — the same "fail-fast if a needed secret is missing" gate that already exists for model providers. No secret material ever lands in a gateway config or a `ToolCandidate`; only env-var **names**.

## 6. TTL tool-update (trust-gated)

The currency check (v0.0.43) is the detector. A scheduled/`on-demand` refresh step (§design-doc §3.5) runs currency; for a `TOOL_BEHIND_SOURCE`/`DOCKER_IMAGE_BEHIND` tool:
- **verified/org, already provisioned + operator opted-in** → auto re-run the install step (a rebuild/repull — the same governed `installing` state, no re-elicitation since secrets/config persist).
- **otherwise** → surface it (the doctor warning) for operator-approved update. Never a silent third-party reinstall on a timer.

## 7. What it reuses (grounded)

- `crates/praxec-core/src/proxy_workflow.rs` + `connections grant` (gateway.rs) — connection wiring precedent.
- `crates/praxec-core/src/provider_keys.rs` — the secrets store to generalize.
- `crates/praxec-core/src/hitl.rs` + `runtime_submit.rs` — the elicitation/human-gate machinery for `collecting` + `wiring`.
- `crates/praxec-core/src/hot_reload.rs` — reload after the config write.
- `crates/praxec/src/preflight.rs::guard_provider_credentials` — the credential-check to extend to connection secrets.
- Phase 1 `tool_catalog` — candidates + `requires`.
- The v0.0.42 process-group kill + v0.0.43 currency — bounded install + staleness detection.

## 8. Open questions (for review)

- **Config-write primitive location** — a new `kind: config-mutation` executor vs a core `Runtime` method the `wiring` state calls. Leaning: a narrow core primitive (`config::add_connection` / `remove_connection`) with the atomic+validate+backup discipline baked in, invoked by a thin executor — so the safety can't be bypassed.
- **Which config file** — the active gateway config can be an `include:` tree. Provision must target the right file (the top-level, or a dedicated `connections.d/` the operator designates). Proposal: write to a designated, git-ignore-able `connections.provisioned.yaml` that the top-level `include:`s — so provisioned tools are isolated, easy to diff, and easy to deprovision, and hand-authored config is never touched.
- **Elicitation transport headless** — `collecting`/`wiring` are human gates; in a headless drive they park (the existing HITL park/resume). Confirm the operator-facing prompt carries enough (the `requires` descriptions).

## 9. Phasing within Phase 2 (build order)

1. **P2.1** — connection-secrets in `providers.env` + generalized doctor credential-check (`required_secrets`). Small, foundational, independently testable.
2. **P2.2** — the governed config-mutation primitive (`add_connection`/`remove_connection`: atomic + validate + backup, targeting `connections.provisioned.yaml`). The risky core; heavy tests.
3. **P2.3** — `flow.tools.provision` workflow (select→trust→install→collect→validate→wire) wiring P2.1+P2.2 + install recipes + elicitation.
4. **P2.4** — `flow.tools.deprovision` + TTL trust-gated update.
