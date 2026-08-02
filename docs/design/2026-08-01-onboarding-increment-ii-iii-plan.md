# Onboarding — Increment II/III + known-issue fixes — implementation plan

> **For agentic workers:** REQUIRED SUB-SKILL: superpowers:subagent-driven-development. TDD, red-first.
> praxec code tasks run ONE implementer at a time (cargo serializes on `.cargo-lock`).

**Goal:** Complete the deferred onboarding increments (II pack selection, III discovery→installer)
and fix all known residuals, building on Increment I (`docs/design/2026-08-01-onboarding-tool-provisioning.md`).

**Scope decisions (user-approved 2026-08-01):** pack-level selection (defer per-workflow closure);
add an **npx provider** for third-party npm MCP servers; **skip** frontrails in init (document);
**cut a corpus release** + add it to the registry.

## Global Constraints
- Build on Increment I; reuse `provision_install`, the registry, `load_repo`, `tool_catalog`. No parallel abstractions.
- Consent by construction preserved; checksum-verify preserved; fail-fast with context.
- assert-don't-derive (atomic tests, mock transport never skip); match style; fmt + clippy clean.
- Three tracks: **praxec code** (this branch `feat/onboarding-increment-ii-iii`), **`praxec/packs` data** (separate PR), **corpus release** (tag).

---

## Track A — praxec code (serial TDD tasks)

### Task A1: Docker connection-body recipe (clears `BUILD_RECIPE_UNAVAILABLE`)
**Files:** `examples/tool-provision/flow.tools.provision.yaml` (the `building` step); `crates/praxec-core/tests/examples_validate.rs`.
- The `building` step's `build.connection-body` currently fails a docker-transport candidate with `BUILD_RECIPE_UNAVAILABLE`. Add the docker recipe: wire `{kind: mcp, command: docker, args: [run, --rm, -i, <image>:<version>]}` (image from the registry tool's `providers.docker`, version pinned). Keep stdio (`command:<binary>`) + remote/rest lanes intact.
- **Tests:** the flow passes `praxec check`; a docker-transport candidate reaches a valid connection body (not `BUILD_RECIPE_UNAVAILABLE`). Grep-clean: the docker `BUILD_RECIPE_UNAVAILABLE` dead-end is gone (rest-transport-with-secrets may keep its own typed error — leave it).
- **Accept:** docker-provider tools flow end-to-end (install → build docker-run connection → stage → grant).

### Task A2: `praxec pack list <repo>` enumeration
**Files:** `crates/praxec/src/gateway_config.rs` (a `Pack { List { repo } }` subcommand), `crates/praxec/src/gateway.rs` (handler), test in `crates/praxec/tests/`.
- Reuse `praxec_core::repo::{load_repo, definition_files}` on a bare pack path (a local dir OR a `{uri,ref}` resolved clone — reuse the Increment-I registry-source resolution if trivial; else local path is enough for v1). Print the pack's namespace + its `flow.*` and `cap.*` definition ids (namespace-prefixed), grouped, WITHOUT loading a full gateway.
- **Tests:** on a fixture pack dir, lists the expected flow/cap ids; fails-fast with a clear error on a non-pack dir (no `praxec.repo.yaml`).
- **Accept:** an operator can see what a pack provides before wiring it.

### Task A3: `init` pack-level selection + frontrails-skip doc
**Files:** `crates/praxec/src/init.rs`, `crates/praxec/src/gateway_config.rs` (`Init { …, packs: Option<String> }` — comma-list; keep `--with-starter-packs`).
- `--packs cognitive-architectures,praxec-meta` selects a subset from the known open packs (map short id → `{uri, ref: main}` via the existing `STARTER_PACKS` table). `--with-starter-packs` stays = all open packs. Union/idempotent via the existing `merge_pack_wiring`. Unknown pack id → fail-fast listing the valid ids.
- Document (init help + `docs/…`) that frontrails is intentionally NOT a starter pack (include:{uri,hash}, needs licensed FrontRails servers) — wire it manually if licensed.
- **Tests:** `--packs a,b` wires exactly those; unknown id → error; combines with `--with-starter-packs` without duplicates.
- **Accept:** pack-level cherry-pick works; frontrails exclusion documented.

### Task A4: Discovery → installer reconciliation (Increment III)
**Files:** `crates/praxec-core/src/provision_install/` (a `from_candidate` normalizer), `crates/praxec/src/gateway.rs` (`tools install` accepts a discovered candidate), tests.
- Bridge `tool_catalog::ToolCandidate` (discovery) → the installer: a normalizer maps a `ToolCandidate`'s `ToolSource`/`Transport` to a provider coordinate the installer understands (`Image→docker`, `Repo/Crate→release/cargo`, `Url/Npm→remote/npx`). `praxec tools install` gains a path to install a tool surfaced by `praxec.query {discover}` (by id/name against the assembled catalog), routing through the SAME `provision_install::install`.
- Keep discovery read-only otherwise; this is the "act on a discovered tool" bridge, no second installer.
- **Tests:** a `ToolCandidate` with `ToolSource::Image` normalizes to a docker install plan; `ToolSource::Repo`+release → release plan; `tools install <discovered-name>` reaches `install` (fake IO). Unknown/ambiguous → fail-fast.
- **Accept:** discovered third-party tools are installable through the one installer.

### Task A5: npx provider + parked-minor hardening
**Files:** `crates/praxec-core/src/provision_install/{provider.rs, mod.rs, io.rs}`, tests.
- **npx provider:** add `Provider::Npx`. An npm-distributed stdio tool (registry `providers.npx` = the package, or `ToolSource::Npm`) "installs" as a **no-op** (npx fetches on run) and its connection wires `{command: npx, args: [-y, <pkg>]}`. Slot into the chain (release → docker → **npx** → cargo; npx before cargo since it needs no toolchain). npx availability gated on `io.which("npx")`. Never a source build.
- **M3:** `expected_sha256` — reject a malformed/short hash token with a distinct `CHECKSUM_MALFORMED` diagnostic (still fail-closed).
- **M4:** `installed_version` — make the `--version` probe sturdier (tolerate a `name x.y.z` prefix / stderr) but keep the safe direction (unknown → reinstall).
- **Tests:** npx candidate → no download, connection wires `npx -y <pkg>`, gated on `which("npx")`; malformed checksum → `CHECKSUM_MALFORMED`; version-probe parses `tool 1.2.3`.
- **Accept:** third-party npm MCP servers provisionable; minors hardened (fail-safe).

### Task A6: docs + CHANGELOG
**Files:** `docs/guides/connections.md`, `docs/reference/configuration.md`, README, `CHANGELOG.md` (`## [Unreleased]`).
- Document: `praxec pack list`, `init --packs`, discovered-tool install, the npx provider, the docker-recipe fix, frontrails-skip. Grep-verify every flag/command against source. Extend the design doc §12 as-built notes.
- **Accept:** docs match the shipped surface; CHANGELOG updated.

---

## Track B — `praxec/packs` registry data (separate PR, no cargo)
**Repo:** `github.com/praxec/packs`, file `packs.yaml`.
- Version currency: `cpm-planner` 0.0.1→**0.0.2**, `crossmatrix` 0.1.0→**0.2.0** (match latest releases).
- cog-arch `requires:` completeness: add **corpus** and **markdown-administrator** (its flows spawn them).
- Add a **corpus** tool entry (`command: corpus`, `version` = the release cut in Track C, `providers: {docker: ghcr.io/praxec/corpus, release: https://github.com/praxec/corpus/releases, cargo: corpus}`, `mcp_registry_id`).
- **Accept:** registry is current + `requires:` reflects real tool deps; a PR to `praxec/packs`.

## Track C — corpus release (tag)
**Repo:** `github.com/praxec/corpus`.
- Verify corpus builds + has a release workflow (taiki-e) + a Cargo.toml version; cut a release tag so its prebuilt binaries publish (making it release-installable and matching the Track B registry entry).
- **Accept:** `gh release view -R praxec/corpus` shows published binaries.

---

## Sequencing
- Track A serial (A1→A6), whole-branch review + fixes at the end (like Increment I), then PR to dev.
- Track B + Track C run in parallel with Track A (different repos, no cargo). Track B's corpus `version:` = Track C's tag.
