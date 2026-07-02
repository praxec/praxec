# ADR-0013: `doctor` provisions a pack's MCP tools through a registry-driven provider chain

**Status:** Accepted

**Date:** 2026-07-02

## Context

A **pack** (a `cap.*`/`flow.*` library loaded via `repos:`) is pure YAML — nothing
to install. But its workflows spawn **MCP tools** as `kind: mcp` connections
(`command: cpm-planner`, `command: fmeca-mcp`, …). Those tools are real binaries
that must exist on the operator's machine. Today:

- The pack's connections name a bare `command`. If the binary isn't on `PATH`, the
  connection fails at spawn — and `px doctor` reports it missing (binary-discovery
  check, SPEC §29 / Tranche 3), but does nothing about it.
- Nothing tells the operator *which* tools a pack needs *before* they run it, nor
  *how* to get them. "Install cpm-planner" is a manual, undocumented, per-tool
  scavenger hunt.

Meanwhile the ecosystem has standardized: the official
[MCP registry](https://registry.modelcontextprotocol.io) is a directory of MCP
servers carrying "how to run" metadata (container / binary / npx / uvx), and the
Docker MCP Catalog runs MCP servers in pinned containers. We should ride that, not
build a bespoke package manager.

The tension is the usual Praxec one: we want provisioning to be **low-friction**
(a pack "just works") *and* **governed** (nothing installs itself behind the
operator's back — the same no-silent-fallback discipline as the rest of the
runtime).

## Decision

**Packs declare their tool dependencies; the pack registry describes how to obtain
each tool via an ordered provider chain; `px doctor` resolves and *offers* the
provisioning command — with consent, never silently.**

1. **Dependencies are declared, not discovered.** The pack registry
   ([`praxec/packs`](https://github.com/praxec/packs), `packs.yaml`, schema
   `praxec.packs/v2`) lists each pack's `requires: [tool-id]` and a `tools[]`
   catalog. Each tool carries a `command`, a `version`, an `mcp_registry_id`
   (`dev.praxec/<tool>`), and an ordered `providers` chain:

   ```
   docker  →  release (prebuilt binary)  →  cargo
   ```

2. **Docker is the default provider.** It is the only option that is
   simultaneously reproducible (pinned image), sandboxed, and language-agnostic —
   and "capabilities run in bounded execution contexts" *is* the Praxec thesis, so
   containerizing capability tools is consistent, not a bolt-on. MCP tools are
   long-lived stdio processes (one per connection, not per call), so container
   startup is amortized to a non-issue. The prebuilt binary is the low-friction
   native fallback; `cargo` is the source path.

3. **`doctor` provisions, with consent.** `px doctor` gains a resolution step:
   given a pack (or the active gateway config), resolve every `requires[]` tool
   against the registry, report which are missing, and for each **offer the exact
   command** for the highest-preference available provider (e.g. `docker pull
   ghcr.io/praxec/cpm-planner:<version>`). It **never runs it without the operator
   opting in** — extending the ADR-0006 pattern (`doctor --fix` may *offer* to run
   a remedy, only with explicit consent).

4. **The MCP registry is the interop standard, not our resolver.** Each tool
   publishes a `server.json` and registers under `dev.praxec/*`. praxec is a
   *consumer* of that metadata; any MCP host can resolve the same tools. We do not
   invent a proprietary distribution channel.

## Consequences

**Gains**
- A pack "just works": `doctor` tells you exactly what a pack needs and how to get
  it, before it fails at spawn.
- Reproducible + sandboxed by default (Docker), with a native escape hatch.
- No lock-in: standard MCP-registry ids + `server.json`; other hosts resolve the
  same tools.
- Governance preserved: provisioning is an operator-consented action, auditable,
  never a silent side effect.

**Costs / trade-offs**
- Docker as the default adds a Docker dependency for the reproducible path (TRIZ:
  resolved by the provider chain — the release binary needs no Docker).
- We maintain a release matrix (Docker images + cross-platform binaries) per tool,
  in CI. Justified: it is the thing that makes packs adoptable.
- MCP-registry publishing needs one-time namespace verification (DNS for
  `dev.praxec` or GitHub OIDC). A setup step, not runtime.

**Rejected alternatives**
- *A bespoke Praxec package manager* — reinvents the MCP registry + container
  ecosystem; lock-in; more surface to secure. No.
- *Docker-only* — forces Docker on every operator and every CI; the release-binary
  provider removes that hard requirement.
- *Hard-install-only (`cargo install`)* — Rust toolchain + platform friction; no
  sandbox; no cross-language story.
- *Silent auto-install* — violates the no-silent-fallback discipline; a governance
  regression. Provisioning is always operator-consented.

## Failure modes (FMECA)

| Mode | Prevention / detection | Residual |
|------|------------------------|----------|
| Version drift (pack expects tool behavior it no longer has) | `version` pin in the registry; `doctor` compares resolved vs required | Low |
| Silent install surprises the operator | Consent required by construction; `doctor` only *offers* | Low |
| Preferred provider unavailable (no Docker) | Ordered chain falls through to release binary / cargo; `doctor` reports which provider it will use | Low |
| Malicious/compromised tool | Container isolation for the Docker path; pinned digests; registry provenance | Medium — mitigated, not eliminated |

## Scope / status

This ADR fixes the **model**. Implementation lands incrementally: (a) the registry
schema + entries exist (`praxec/packs` `praxec.packs/v2`); (b) each tool repo ships
CI to publish its image + binaries + `server.json`; (c) `px doctor` gains the
resolve-and-offer step. Until (b) completes, the registry's provider coordinates
are the canonical *targets* and `doctor` degrades to "here's where it will live."
