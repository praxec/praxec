# Onboarding + governed tool provisioning — design

**Status:** draft for review · **Date:** 2026-08-01 · **Implements/amends:** ADR-0013

> **Design big, build small.** §2–§4 fix the *target* architecture (the one that kills
> the tool split-brain). §6 scopes the *first shippable slice* (Increment I). §7 lists
> what is deliberately deferred. The deliverable plan (praxec + cpm-planner as the proof
> tool) is decided after this doc is approved.

---

## 1. Problem

A new engineer (Windows + Cursor) got praxec v0.0.45 running — 167 workflows across five
packs, models.yaml + OpenRouter working, `repos: uri:` auto-clone flawless. Then hit a wall:
the **companion MCP tools the workflows spawn** (cpm-planner, fmeca-mcp, log-analyzer, …)
**would not compile on Windows** (MSVC couldn't find `msvcrt.lib`; GNU toolchain didn't help).
Their verdict: *"pre-built Windows binaries would save a lot of pain."*

They were right, and the tools already publish those binaries — nothing consumes them. The
gap is not discovery and not packs; it is **there is no installer**. Underneath sit three
overlapping tool models with no single owner of "get the binary onto the machine."

## 2. What already exists (grounded — do not rebuild)

| Piece | State | Where |
|---|---|---|
| Pack import via `repos: {uri, ref}` auto-clone | **Works** (dev-proven) | `config.rs::merge_declared_repos`, `examples/remote-packs/` |
| `praxec/packs` registry (`packs.yaml`, `praxec.packs/v2`) — declares each pack's `requires:[tool]` and each tool's `providers:{docker,release,cargo}` + `version` + `mcp_registry_id` | **Exists, populated** | `github.com/praxec/packs` |
| Registry loader — `Registry::load_path`, fed into the discovery index/selector | **Wired**, via `discovery.registry: <path>` (local file, **not** network-fetched) | `registry_v3.rs`, `discovery/discovery_indexer.rs` |
| `praxec.packs/v3` parser is a **compatible superset of v2** — reads the published file as-is | **True** (module doc, `registry_v3.rs:12-19`) | — |
| All `/praxec/*` tools publish prebuilt binaries + `checksums.sha256` + GHCR images (5 targets, taiki-e) | **Done** (ADR-0013 step b) | each tool repo `release.yml` |
| Governed staging — `connections add --block` → `connections grant/revoke` (audit + `GRANT_REQUIRES_OPERATOR`) | **Done** | `conn_write.rs`, `gateway.rs` |
| `flow.tools.provision` — governed select→stage→validate→grant flow | **Exists**, but install step only runs `npm install -g`; docker/cargo/release dead-end at `INSTALL_RECIPE_UNAVAILABLE` | `examples/tool-provision/` |
| `doctor` — preflight PATH existence + currency + `required_secrets` | **Detects** missing tools; does **not** install | `gateway.rs::doctor`, `provision.rs`, `currency.rs` |
| `tool_catalog::ToolCandidate` discover/evaluate (third-party registries) | **Done**, advisory-only; separate from the install model | `tool_catalog/`, `handlers.rs` |
| `praxec init` — scaffolds gateway/models/providers/editor | **Done** (v0.0.45); no `repos:` / no registry pointer | `init.rs` |

**The split-brain:** two half-descriptions of a tool (`ToolCandidate` for discovery,
`RegistryTool` for the provider chain) and one install flow that only knows `npm`. ADR-0013
already chose the winner (the provider chain) and the surface (`doctor`, with consent) — it
was never wired (step c).

## 3. Target architecture (design big)

**One install model — ADR-0013's provider chain — with exactly one installer, reachable from
three surfaces. Everything else becomes a projection or a delegation.**

```
                    praxec/packs  packs.yaml   (source of truth: requires[] + providers{})
                              │
                    Registry::load_path  (local, pinned; init obtains it)
                              │
        ┌─────────────────────┼─────────────────────┐
   doctor --fix          init --install-tools    flow.tools.provision
  (resolve & offer)     (--with-starter-packs)    (install step delegates)
        └─────────────────────┼─────────────────────┘
                              ▼
                    ProvisionInstaller  ◄── the one missing piece
       resolve provider chain → docker pull  OR  release download+checksum-verify+PATH
       (cargo = last-resort source path)  ·  consent-gated  ·  idempotent  ·  audited
```

Principles (from the decisions):
1. **One install model.** The ADR-0013 `providers` chain is canonical. `ToolCandidate`
   discovery stays a **read-only projection**; when a discovered tool is acted on, it
   normalizes to a provider coordinate and hands to the one installer. (Wiring that path is
   **deferred** — §7 — it is not on the onboarding critical path.)
2. **Docker AND prebuilt binary are both first-class** for every `/praxec/*` tool. The chain
   falls through automatically; `doctor` reports *which* provider it will use before running.
   For **onboarding specifically** the effective order is **release-binary → docker → cargo**
   (lowest friction for a fresh machine with no Docker daemon); the reproducible/sandboxed
   docker path is one flag away. This re-weights ADR-0013's docker-default for the init path
   only — recorded as an amendment in §8.
3. **Consent by construction.** No silent install. The explicit `--install-tools` /
   `--with-starter-packs` / `--yes` flag *is* the consent (extends the ADR-0006 `doctor --fix`
   pattern). Default is offer-only.
4. **No parallel abstractions.** The installer replaces the `npm-only` branch in
   `flow.tools.provision` and the `cargo install --path --force` remediation in `currency.rs`
   — those dead paths are **removed**, not left beside the new one.
5. **Dependencies are data, not code.** Per-tool release-asset naming (musl vs gnu, per-tool
   binary names) lives in the registry as a declared pattern; the installer never guesses.

## 4. The installer (the one new component)

`ProvisionInstaller` (praxec-core), consumed behind a `CurrencyIo`-style IO seam so it is
unit-testable without a network or a daemon.

Input: a resolved `RegistryTool` (from the loaded registry) + the operator's consent + host
`(os, arch)`. Behaviour:

1. **Resolve provider** by the onboarding-weighted chain (release → docker → cargo), skipping
   unavailable providers (no Docker daemon → skip docker), reporting the chosen one.
2. **release provider** — resolve `(os, arch)` → the exact asset via the registry's declared
   asset pattern for that tool (or the GH releases API for the pinned `version`); download;
   **verify against `checksums.sha256`; refuse on mismatch**; unpack; place the binary on a
   praxec-managed PATH dir (e.g. `<config-dir>/bin`). *Trust model:* the `checksums.sha256`
   verify guarantees the binary matches the release's published checksum (anti-corruption /
   transport-integrity over the release page's TLS) — it is **not** independent
   provenance/anti-MITM, since the asset and its `checksums.sha256` are fetched from the same
   release page.
3. **docker provider** — `docker pull <image>@<pinned-digest-or-version>`; the connection's
   `command` becomes the `docker run …` form.
4. **cargo provider** — last-resort source build; emitted only when release+docker both
   unavailable, and never silently (this is the path the dev's pain came from).
5. **Fail fast** with the resolved URL/image + triple in the error on any 404 / mismatch /
   unpack failure. Idempotent: a re-run detects an already-current binary and no-ops.

Every step emits an audit event (`tool.install_resolved`, `tool.install_verified`,
`tool.install_placed`) so `doctor` and the audit log are the observability surface.

## 5. Open item — resolved

*How does praxec obtain the registry?* Today: only from a **local** `packs.yaml` an operator
points `discovery.registry` at; the published `praxec/packs/packs.yaml` is not fetched.

Resolution: **source the registry always-latest, the same way packs already are** — point
`discovery.registry` at `praxec/packs` via `{uri, ref: main}`, resolved through the *exact*
`repos:` machinery that re-fetches and resets to the tip on every load (and via `praxec sync`).
No pinning, no vendored copy, no `{uri, hash}` freeze. This is a deliberate **currency-over-pinning**
choice, consistent with the workflow-currency stance (a lockfile was rejected for workflows for
the same reason): the operator always resolves the latest tool coordinates + the latest pack
`requires[]`, so a newly released tool or a newly added dependency shows up without an operator
edit. `init` writes this `discovery.registry` block; no new fetch subsystem (reuse `{uri, ref}`);
the v2/v3 concern is void (superset).

**Currency and integrity are separable and both satisfied.** Registry freshness comes from
always-latest sourcing; binary safety comes from the `checksums.sha256` verify at *install*
time (§4) — which runs regardless of how the registry was obtained. Pinning the registry was
never the integrity mechanism, so dropping it costs no safety.

**Finer point — tool `version:`.** The registry's per-tool `version:` is the org's *blessed-latest*
pin, kept current in `praxec/packs`; always-fetching-latest-registry is therefore how "latest"
is delivered — a curated currency point, not each operator independently chasing raw
`releases/latest` (which would reintroduce the version-drift ADR-0013 guards against).

## 6. Increment I — build small (the first shippable slice)

The minimum that turns the dev's dead-end into a working path:

1. **`ProvisionInstaller`** (§4): release + docker providers, checksum-verify, PATH placement,
   fail-fast, idempotent. Behind an IO seam; unit-tested with a fixture registry + fake
   assets. cargo path emits the command but is not the default.
2. **`doctor` resolve-and-offer** (ADR-0013 step c): for each `requires[]` tool the active
   config/pack needs and that preflight reports missing, resolve against the registry and
   **offer** the exact provider command; run it only under `doctor --fix` consent.
3. **`init` registry pointer + pack wiring:** `praxec init --with-starter-packs` writes the
   `repos:` block for cognitive-architectures + praxec-meta, obtains + points at a pinned
   `packs.yaml` (`discovery.registry`), and invokes the same doctor resolve path (offer by
   default; install under `--install-tools`/`--yes`). `--pack <uri>` does one pack.
   `init` gets **no installer of its own** — it calls the doctor path.
4. **Cleanup (removal, not addition):** `flow.tools.provision`'s install step **delegates** to
   `ProvisionInstaller` — delete the `npm install -g` special case and the
   `INSTALL_RECIPE_UNAVAILABLE` dead-end states. Flip `currency.rs` remediation from
   `cargo install --path --force` to the release-binary path.

**Proof tool: cpm-planner.** It already publishes all-OS binaries + a GHCR image + a registry
entry with a full `providers` block — the cleanest end-to-end proof that a fresh gateway can
resolve → download → verify → place → spawn a tool with zero compilation.

## 7. Deferred (design-big, not-built-now)

- **Cherry-pick with dependency-closure.** Ship "install all workflows in the pack" (the dev
  wanted all; it works). If a user cherry-picks, offer per-tool `(y/n)` or a pick-menu — but
  **flow-level selection with closure over flows→sub-flows→caps is a fast-follow**, not I.
  Rationale: uncertain value, real complexity (27 flows / 82 caps in cog-arch).
- **Discovery→installer reconciliation.** Making `ToolCandidate` a projection that feeds the
  installer is correct target-state debt-payoff, but onboarding uses the *curated* registry,
  not third-party discovery. Deferred; the split-brain shrinks to "two read surfaces, one
  installer," which is acceptable interim.
- **`praxec pack list <repo>` enumeration primitive.** Needed only for cherry-pick UX; ships
  with that fast-follow.
- **frontrails-style `include:` packs in `--with-starter-packs`.** frontrails has no
  `praxec.repo.yaml` by design (sha256 `include:`). `init` should eventually wire both shapes;
  Increment I covers `repos:` packs only.

## 8. Amendment to ADR-0013

ADR-0013 stands; this doc **implements its step (c)** and makes two adjustments:
- **Provider default is context-dependent.** ADR-0013 makes Docker the global default; for the
  **onboarding/init path** the effective order is release-binary → docker → cargo (fresh-machine
  friction). Both providers remain first-class; the reproducible docker path is opt-in per run.
- **`requires[]` is authoritative from the registry**, not re-declared per pack manifest — the
  central `packs.yaml` is the single source of tool coordinates.

## 9. FMECA residuals (from the validity pass)

All High/Medium driven to Low in one iteration; carried here as the acceptance bar for I.

| Failure mode | Poka-yoke | Observability | Residual |
|---|---|---|---|
| Wrong/missing release asset (os/arch) | asset pattern in registry **data**, resolve-or-fail-fast with URL+triple | `tool.install_resolved` | Low |
| Tampered/corrupt binary | mandatory `checksums.sha256` verify; refuse on mismatch | `tool.install_verified{ok:false}` | Low |
| Docker-absent blocks onboarding | chain falls through to release binary; doctor names chosen provider | doctor per-tool line | Low |
| Silent install | consent = explicit flag; default offer-only | `tool.install_consented{flag}` | Low |
| Partial/half-wired (install ok, grant fails) | staged checkpoints install→verify→stage→grant; idempotent re-run resumes | doctor shows stage reached | Low |
| Registry unobtainable | `{uri, ref: main}` resolved via the proven `repos:` path; fail-fast if unreachable (offline reuses the last cached tip, warns) | startup logs registry source+resolved commit | Low |
| Registry stale (operator behind latest) | **always-latest by construction** (re-fetch/reset to tip on load; `sync`); no pin to drift | staleness warning already shipped; startup logs resolved commit | Low |

## 10. Verification (for I)

- Unit: installer resolves a fixture tool → downloads a fake asset → **rejects a bad
  checksum** → places a good one; docker path selected only when a daemon is present; cargo
  emitted only when both others unavailable.
- Integration: a fresh scaffolded gateway + a pinned registry → `doctor --fix` installs
  cpm-planner from its release binary (no compiler) → the connection spawns.
- Cleanup: `flow.tools.provision` install step reaches the installer; the `npm`-only branch
  and `INSTALL_RECIPE_UNAVAILABLE` states are gone (grep-clean); currency remediation string
  references the release path, not `cargo install`.
- Live: the Windows dev runs `praxec init --with-starter-packs --install-tools` on a clean
  machine → cpm-planner et al. arrive as binaries, zero compilation.

## 11. Non-goals

- A bespoke package manager (ADR-0013 rejected; we consume the MCP registry + release/GHCR
  artifacts).
- Managing git/registry credentials (install piggybacks on the operator's own auth, as
  `repos:` already does).
- Installing the `praxec` binary itself (release + optional wrapper handle that).

## 12. Increment I — as-built notes

Increment I shipped as designed (always-latest `discovery.registry: {uri, ref}`; the
`praxec_core::provision_install` `ProvisionInstaller` with the release + docker providers,
checksum-verify, and PATH placement; `doctor --fix`; `init --with-starter-packs
[--install-tools]`; `praxec tools install <id>`; the provision flow delegated and the
npm/cargo dead paths deleted). The cpm-planner live-proof
(`crates/praxec/tests/provision_cpm_planner.rs`) drives the real resolve→verify→place chain
against the host's own triple with a fake IO and confirms the cargo/source path is never taken.
Two residuals surfaced during build, recorded here for truthfulness:

- **(a) Docker candidate → `BUILD_RECIPE_UNAVAILABLE`.** `flow.tools.provision`'s install step
  delegates docker to the one installer (which pulls the image), but a docker-transport candidate
  then still hits the flow's *pre-existing* `BUILD_RECIPE_UNAVAILABLE` in the `building` state —
  there is no docker connection-body recipe (the `docker run …` form) wired yet. This is a typed
  fail-fast, not a wedge, and is out of Increment I's release-binary critical path. **Fast-follow:**
  synthesize the docker connection body so a docker candidate completes to a ready connection.

- **(b) Community sandbox removed, not replaced 1:1.** The community lane's
  `confinement: confined` npm-install sandbox state was deleted rather than ported. The one
  installer downloads **checksum-verified release binaries**, which run no arbitrary install
  scripts, so the per-step install sandbox protected nothing the checksum verify does not. The
  community-tier protection is now the existing **double-approval** (`community_gate`) — an extra
  operator consent — not an install-time sandbox.

- **(c) stdio lane wires the installed binary, not npx.** The installer distributes
  **release/docker/cargo binaries only** — there is no npm provider. `flow.tools.provision`'s
  `building` step accordingly wires an stdio candidate's connection body as
  `{kind: mcp, command: <the tool's command>}` — the same `command` (`$.context.name`, the id
  passed to `praxec tools install`) the registry tool and installer place on PATH — rather than
  the old `{command: "npx", args: ["-y", <pkg>]}`; the stdio lane's `npmPkg != null` guard was
  dropped. **Consequence / known limitation (not a silent regression):** npm-*distribution* of a
  stdio tool is no longer supported. An npm-only third-party tool (one with no release/docker/cargo
  provider) cannot provision under Increment I; this is out of scope, and the flow fails fast at
  `installing` (`INSTALL_FAILED`) for such a candidate rather than silently wiring an npx command
  the installer never placed.

### Increment II/III — as-built notes

Increment II (pack-level selection) and Increment III (discovery→installer) shipped on
`feat/onboarding-increment-ii-iii`, and residuals (a) and (c) above are now cleared:

- **`praxec pack list <repo>`** (Task A2). Reuses `praxec_core::repo::load_repo` on a bare pack
  dir — the same `praxec.repo.yaml` walk `check`/`serve` use — and prints the namespace-prefixed
  `flow.*` / `cap.*` ids, grouped + counted, with no store/runtime. Read-only, fail-fast on a
  non-pack dir. Local-dir path only for v1 (remote `{uri,ref}` clone deferred).

- **`init --packs <comma-list>`** (Task A3). Selects a subset of the open `STARTER_PACK_URIS` by
  short id (the final `/`-segment, derived — not a parallel list), wires each as `{uri, ref: main}`
  through the existing `merge_pack_wiring`, and sets `registry=true`. Unions idempotently with
  `--with-starter-packs` / `--pack`; unknown id → fail-fast listing valid ids. **frontrails is
  deliberately excluded** — an `include:{uri,hash}` pack needing licensed FrontRails servers; wire
  it by hand with `--pack <uri>`. (Documented in `docs/guides/connections.md`.)

- **Discovery → installer** (Task A4) clears the split-brain gap in §7. `provision_install::from_candidate`
  normalizes a `tool_catalog::ToolCandidate` to a provider coordinate (`Image→docker`,
  `Repo→release`, `Crate→cargo`, `Npm→npx`, `Url→Remote/no-install`) and `praxec tools install`
  falls back to a discovered candidate (matched by name) when the id isn't in the curated
  `discovery.registry`, routing through the **one** `provision_install::install`. Curated wins on
  a name collision. **No-version caveat:** discovery candidates carry no version, so docker defaults
  to the honest `:latest` tag while a discovered `Repo`/release tool needs a pinned semver before
  it resolves a real asset — the curated registry stays the pinned path.

- **npx provider** (Task A5) clears residual (c). `Provider::Npx` slots into the chain **release →
  docker → npx → cargo** (before cargo — no toolchain). An npm-distributed stdio tool "installs"
  as a **no-op** (`NoInstallNeeded`; npx fetches on run) and wires `{command: npx, args: [-y,
  <pkg>]}`, gated on `io.which("npx")`. Never a source build. (Also hardened: `CHECKSUM_MALFORMED`
  for a short/malformed hash token, and a sturdier `--version` probe.)

- **docker connection-body recipe** (Task A1) clears residual (a). `flow.tools.provision`'s
  `building` step now emits a `docker run --rm -i <image>` connection body for a docker-transport
  candidate (image from `providers.docker`, version pinned), so docker candidates complete to a
  ready connection instead of hitting `BUILD_RECIPE_UNAVAILABLE`. stdio / remote / rest lanes
  unchanged.

Two cross-repo deliverables landed alongside the code:

- **`praxec/packs` registry currency** (Track B, separate PR
  [praxec/packs#9](https://github.com/praxec/packs/pull/9)). `cpm-planner` 0.0.1→0.0.2,
  `crossmatrix` 0.1.0→0.2.0, cognitive-architectures `requires:` completed with `corpus` +
  `markdown-administrator`, and a new `corpus` tool entry (docker/release/cargo providers).

- **corpus v0.0.1 release** (Track C). Tagged so corpus's prebuilt binaries publish, making the
  Track B `corpus` registry entry release-installable and matching the pinned `version:`.
