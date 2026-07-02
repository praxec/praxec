# Security policy

## Supported versions

| Version | Supported          |
|---------|--------------------|
| 0.1.x   | :white_check_mark: |
| < 0.1   | :x:                |

We currently support the most recent minor version on the `0.x` line.
Once the project reaches `1.0`, this table will be updated to cover the
current and previous minor versions.

## Reporting a vulnerability

**Do not file a public GitHub issue for security vulnerabilities.**

Please report via **GitHub Security Advisories**:

<https://github.com/praxec/praxec/security/advisories/new>

Include, where possible:

- A description of the issue and its impact.
- Steps to reproduce, ideally with a minimal config.
- The affected version (`praxec --version`).
- Any suggested mitigation.

## What to expect

- **Acknowledgement** within **3 business days**.
- **Initial assessment** within **10 business days**.
- **Coordinated disclosure window**: 90 days from the acknowledgement,
  unless an earlier public disclosure is required to protect users.

A CVE will be requested for any vulnerability with a CVSS v3.1 score of
4.0 (medium) or higher. Patched releases will reference the advisory in
the [CHANGELOG](CHANGELOG.md) under a `### Security` heading.

## Scope

In scope:

- The published crates in this workspace (`praxec`,
  `praxec-core`, `praxec-executors`, `praxec-mcp-server`,
  `praxec-schema`).
- Default executor implementations.
- Audit, store, and reliability subsystems.
- Example configurations that are bundled in this repository.

Out of scope (please report to the appropriate upstream):

- Vulnerabilities in the MCP servers, REST endpoints, or CLI tools that
  the gateway proxies to.
- Vulnerabilities in third-party crates we depend on (e.g. `rmcp`,
  `rusqlite`, `reqwest`) — please report those to their maintainers,
  then notify us so we can pin patched versions.
- Misconfiguration leading to over-permissive policy — but please open
  a regular issue if you believe a misconfiguration is *easy to fall
  into*; documentation hardening matters.

## Hardening recommendations

For production deployments, see the
[README's "Going to production"](README.md#going-to-production)
and [`docs/reference/stability.md`](docs/reference/stability.md). In particular: do not run with the
default `memory` store or `stderr` audit sink in any deployment where
state loss or log mixing matters.

## Identity and the caller trust boundary

The gateway resolves a caller `Principal` (subject + roles + permissions)
that drives `actor: human` gates and permission guards. Identity is taken
**only** from the MCP request `_meta` field, under the reserved key
`io.praxec/principal` (`{ subject, roles, permissions }`), or — for
single-tenant deployments — from a `gateway.principal` default in config.

This is a deliberate trust boundary:

- `_meta` is set by the **embedding host** (the trusted parent process that
  spawns the gateway over stdio and has authenticated the human). The host is
  responsible for populating `_meta.principal` *only* from authenticated
  identity, never from model/agent output.
- Tool **`arguments`** — the only request field an LLM/agent influences — are
  **never** consulted for identity. `resolve_principal` reads `_meta` exclusively.
- Absent any claim, the caller is **anonymous** (no roles, no permissions):
  fail-closed. Human-gated transitions are then unreachable, by design, until
  an authenticated host supplies a human principal.

An agent therefore cannot escalate to `human` or grant itself permissions —
it cannot write to `_meta`, and arguments are not an identity channel.

## Dependency posture

`cargo audit` is expected to pass. Triaged, justified exceptions live in
[`.cargo/audit.toml`](.cargo/audit.toml); each carries a reachability analysis
and an upstream-tracking note. That file is currently **empty**: the `rsa`
exception (`RUSTSEC-2023-0071`) left with the postgres/`sqlx` removal, and the
`rustls-webpki 0.101` advisories left with the AWS SDK / Bedrock path (the
gateway's own TLS uses `rustls 0.23`). Re-review it when bumping `aether-llm`.
Unmaintained-crate warnings in the transitive `aether-*` tree are left visible
(not ignored) for upstream tracking.
