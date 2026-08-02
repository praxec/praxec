# Onboarding + governed tool provisioning — Increment I implementation plan

> **For agentic workers:** REQUIRED SUB-SKILL: superpowers:subagent-driven-development.
> Steps use TDD (red-first). One implementer at a time (cargo serializes on `.cargo-lock`).

**Goal:** A fresh praxec gateway can resolve → download → checksum-verify → place → spawn a
pack's required MCP tools as **prebuilt binaries (or docker)**, with zero compilation, through
one installer reachable from `doctor` and `init`.

**Architecture:** Implements ADR-0013 step (c) + the reconciliation in
`docs/design/2026-08-01-onboarding-tool-provisioning.md`. One new `ProvisionInstaller` in
praxec-core behind an IO seam; `doctor` gains resolve-and-offer; `init` wires packs + the
always-latest registry pointer + calls the doctor path; the npm-only / cargo-install dead
paths are removed. cpm-planner is the zero-new-work proof tool.

**Tech Stack:** Rust workspace (praxec-core, praxec, praxec-executors); existing `repos:`
`{uri, ref}` clone-and-reset machinery; existing `Registry` (`registry_v3.rs`), `currency.rs`
`CurrencyIo` seam pattern, `preflight.rs`, `provision.rs::detect`; `sha2` for checksum verify.

## Global Constraints

- **Design doc is authoritative** — `docs/design/2026-08-01-onboarding-tool-provisioning.md`;
  every task inherits it. Deferred items (cherry-pick closure, discovery→installer
  reconciliation, `pack list`, `include:`-packs in init) are OUT of scope.
- **Registry always-latest** — `discovery.registry` sourced via `{uri, ref: main}` through the
  proven `repos:` path; NO hash-pin, NO vendored copy. Currency > pinning.
- **Integrity at install** — mandatory `checksums.sha256` verify on every downloaded binary;
  refuse on mismatch. This is independent of registry sourcing.
- **Consent by construction** — no silent install; the explicit `--fix` / `--install-tools` /
  `--yes` flag is the consent. Default is offer-only.
- **No parallel abstractions** — the installer REPLACES the `npm install -g` branch in
  `flow.tools.provision` and the `cargo install --path --force` remediation in `currency.rs`.
  Dead paths (`INSTALL_RECIPE_UNAVAILABLE` states) are deleted, grep-clean.
- **Fail fast with context** — every resolve/download/verify failure carries the resolved
  URL/image + `(os, arch)` triple.
- **assert-don't-derive** — pin owned behavior in atomic declarative unit tests, red-first.
  IO (network/docker/fs) behind a seam; unit tests use a fake IO, never real network.
- **v2/v3 void** — the shipped `registry_v3.rs` parser reads the published `praxec.packs/v2`
  file (compatible superset). Do not "reconcile schemas."

## File Structure

- `crates/praxec-core/src/provision_install/` (NEW) — `mod.rs` (`ProvisionInstaller`,
  provider-chain resolution, consent), `provider.rs` (Release/Docker/Cargo resolution),
  `io.rs` (`InstallerIo` seam + `RealInstallerIo`). One responsibility: obtain a tool binary.
- `crates/praxec-core/src/registry_v3.rs` — read the per-tool release-asset pattern (see Task 2).
- `crates/praxec-core/src/config.rs` + `discovery/` — `discovery.registry` accepts `{uri, ref}`.
- `crates/praxec/src/gateway.rs` — `doctor` resolve-and-offer; `--fix` handling.
- `crates/praxec/src/gateway_config.rs` — `Doctor { fix, install_tools }`, `Init` new flags.
- `crates/praxec/src/init.rs` — pack wiring + registry pointer + call doctor path.
- `examples/tool-provision/flow.tools.provision.yaml` — install step delegates; delete dead states.
- `crates/praxec/src/currency.rs` — remediation → release-binary path.

---

### Task 1: `discovery.registry` accepts `{uri, ref}` (always-latest sourcing)

**Files:** Modify `crates/praxec-core/src/config.rs` (+ the discovery-config parse point the
implementer locates — the `discovery: { registry: <path> }` reader, per
`crates/praxec/tests/registry_wiring.rs:142`). Test: `crates/praxec/tests/registry_wiring.rs`.

**Interfaces:**
- Consumes: the existing `repos:` `{uri, ref}` clone-and-reset resolver (reuse; do NOT write a
  new fetcher). Ground its entry point (`merge_declared_repos` / repo-source resolution).
- Produces: `discovery.registry` resolves to a local `packs.yaml` path whether the config gives
  a string path (unchanged) OR `{uri, ref}` (resolved to the cached tip, reset on load).

**Contract & tests (red-first):**
1. A config with `discovery: { registry: <local path> }` loads the registry unchanged (pin the
   existing behavior first — regression guard).
2. A config with `discovery: { registry: { uri: "git+https://github.com/praxec/packs", ref: main } }`
   resolves to the cached clone's `packs.yaml` and loads it. (Mock the git transport with a
   local bare-repo fixture — the `url.insteadOf → bare-repo` pattern used by
   `examples/remote-packs/_mock/`; run the FULL resolve, never skip.)
3. Offline / unreachable uri with an existing cache → uses the last cached tip + emits a soft
   diagnostic (warn), does not hard-fail. No cache + unreachable → fail-fast with the uri.
4. Assert it is NOT hash-pinned: a `{uri, hash}` form is rejected here (hash-freeze is the
   include path, not the registry) — or, if simpler, `{uri, ref}` only is accepted; document.

**Acceptance:** always-latest registry sourcing works through the proven repos path; local-path
form unchanged; offline degrades not breaks.

---

### Task 2: `ProvisionInstaller` — release provider (the core)

**Files:** Create `crates/praxec-core/src/provision_install/{mod.rs, provider.rs, io.rs}`; wire
`pub mod provision_install;` in `lib.rs`. Extend `registry_v3.rs::RegistryTool` to expose an
optional `release_asset` pattern (per-tool asset-name template, e.g.
`{name}-{target}.{ext}`) — **data, not code**; default derivable from `command` + praxec's
target-triple convention. Test: unit tests in `provision_install/mod.rs`.

**Interfaces:**
- Consumes: a `RegistryTool` (from the loaded `Registry`), host `(os, arch)`, `InstallerIo`.
- Produces:
  ```rust
  pub struct InstallerIo { /* trait: http_get(url)->bytes, write_exec(path,bytes), which(cmd)->bool, ... */ }
  pub struct RealInstallerIo;               // real network + fs
  pub enum InstallOutcome { Installed { provider, path, version }, AlreadyCurrent, Refused { reason } }
  pub fn install_release(tool: &RegistryTool, host: Host, io: &dyn InstallerIo) -> Result<InstallOutcome, InstallError>
  ```
- Install dir: `<config-dir>/bin` (praxec-managed PATH dir; resolve via `dirs`).

**Contract & tests (red-first):**
1. Resolve `(os, arch)` → the exact asset via the tool's `release_asset` pattern (or GH releases
   API for the pinned `version`); a fixture tool + fake asset bytes install to `<bin>/<command>`.
2. **Bad checksum → `Refused`/`InstallError`**, binary NOT placed. (Fake IO returns a
   `checksums.sha256` that doesn't match the asset bytes.)
3. Idempotent: a re-run when the current version is already placed → `AlreadyCurrent`, no write.
4. Missing asset for the host triple → fail-fast error naming the resolved URL + triple.
5. Unpack `.tar.gz` (unix) and `.zip` (windows) both handled (fixture both forms).

**Acceptance:** a tool binary is obtained + integrity-verified + placed, or fails loud; no
network in tests (all via `InstallerIo`).

---

### Task 3: `ProvisionInstaller` — docker provider + provider chain

**Files:** `provision_install/provider.rs`, `mod.rs`. Test: unit.

**Interfaces:**
- Produces:
  ```rust
  pub enum Provider { Release, Docker, Cargo }
  pub fn resolve_provider(tool: &RegistryTool, io: &dyn InstallerIo) -> Option<(Provider, Plan)>  // onboarding order: Release → Docker → Cargo
  pub fn install(tool: &RegistryTool, host: Host, consent: Consent, io: &dyn InstallerIo) -> Result<InstallOutcome, InstallError>
  ```

**Contract & tests (red-first):**
1. Docker provider: `docker pull <image>` (pinned `version`), connection command becomes the
   `docker run …` form; selected only when `io.which("docker")` is true.
2. Chain order for onboarding = **Release → Docker → Cargo**; a tool with a release asset picks
   Release even if docker is present (fresh-machine friction). Docker chosen when no release
   asset resolves. Cargo emitted only when neither release nor docker available — and **never
   run silently** (returns the command / requires explicit consent).
3. `resolve_provider` reports the chosen provider WITHOUT installing (for doctor's "offer").
4. Consent gate: `install(..., Consent::OfferOnly, ...)` never mutates — returns the planned
   command; `Consent::Granted` performs it.

**Acceptance:** the chain resolves + reports + installs per the design's release-first order,
consent-gated, docker skipped without a daemon.

---

### Task 4: `doctor` resolve-and-offer

**Files:** `crates/praxec/src/gateway.rs` (`doctor`), `crates/praxec/src/gateway_config.rs`
(`Doctor { config, fix: bool, install_tools: bool }`). Test: `crates/praxec/tests/` (a doctor
integration test with a fixture config + fixture registry + fake installer IO).

**Interfaces:**
- Consumes: `preflight::detect` (which tools are missing), the loaded `Registry`,
  `provision_install::{resolve_provider, install}`.
- Produces: doctor output gains a "tool provisioning" section: for each `requires[]` tool the
  config needs and preflight reports missing, print the resolved provider + exact command
  (offer). Under `--fix`, run it (consent), verify, and — reusing the existing governed staging
  — leave the connection ready. Emit audit `tool.install_*` events.

**Contract & tests (red-first):**
1. Missing required tool + no `--fix` → offer text names the tool, provider, and command; **no
   install performed** (fake IO asserts zero writes).
2. `--fix` with fake IO installs the tool (Release), verifies, reports success; a second run →
   `AlreadyCurrent`.
3. A tool already on PATH → not offered.
4. Consent: without `--fix`/`--yes`, never mutates. Advisory-only for currency stays intact.

**Acceptance:** `doctor` detects → offers; `doctor --fix` installs with consent; audited.

---

### Task 5: `init` pack wiring + always-latest registry pointer

**Files:** `crates/praxec/src/init.rs`, `crates/praxec/src/gateway_config.rs`
(`Init { …, with_starter_packs: bool, pack: Option<String>, install_tools: bool }`). Test:
`init.rs` unit tests (scaffold-and-parse).

**Interfaces:**
- Consumes: the Task 1 `{uri, ref}` registry form; the Task 4 doctor path.
- Produces: `praxec init --with-starter-packs` writes a `repos:` block (cognitive-architectures
  + praxec-meta via `{uri, ref: main}`), a `discovery: { registry: { uri: praxec/packs, ref: main } }`
  block, then invokes the doctor resolve path (offer by default; install under
  `--install-tools`/`--yes`). `--pack <uri>` wires one pack. Re-run is idempotent (merge, not
  clobber; skip already-present blocks unless `--force`).

**Contract & tests (red-first):**
1. `--with-starter-packs` scaffolds a gateway whose `repos:` has both packs and whose
   `discovery.registry` is the `{uri, ref: main}` form; the scaffolded config parses + loads
   (mock transport) — mirror the existing `scaffolded-gateway-parses-and-loads` test.
2. `--pack <uri>` wires exactly that one pack.
3. Idempotent re-run: no duplication; existing blocks preserved.
4. `--install-tools` invokes the installer path (fake IO); default does not.

**Acceptance:** one command scaffolds a pack-wired, registry-pointed, doctor-checked gateway.

---

### Task 6: Cleanup — delegate provision, remove dead paths (no parallel abstractions)

**Files:** `examples/tool-provision/flow.tools.provision.yaml`; `crates/praxec/src/currency.rs`;
any executor support for the flow's install step. Test: existing provision + currency tests,
plus a grep-clean assertion.

**Contract & tests (red-first):**
1. `flow.tools.provision`'s install step routes through `ProvisionInstaller` (or the CLI/executor
   that calls it) for release + docker; the `npm install -g` special case and the
   `INSTALL_RECIPE_UNAVAILABLE` / `install_unsupported` states are **deleted**. Provision e2e
   still stages → grants.
2. `currency.rs` remediation for a behind/local tool references the **release-binary** path, not
   `cargo install --path --force`. Update the test that pins that string.
3. Grep-clean: no remaining `INSTALL_RECIPE_UNAVAILABLE`, no `npm install -g` install branch, no
   `cargo install --path --force` remediation.

**Acceptance:** one installer, no dead paths; provision + currency consistent with the design.

---

### Task 7: Live proof (cpm-planner) + docs

**Files:** `crates/praxec/tests/` (integration); `docs/guides/connections.md`,
`docs/reference/configuration.md`, and the `praxec init` docs; `CHANGELOG.md`. cpm-planner needs
**no change** (already release + GHCR + registry-ready) — it is the proof subject.

**Contract & tests (red-first):**
1. Integration: a freshly scaffolded gateway + a fixture registry entry for cpm-planner (release
   provider, real asset-name shape) + fake installer IO → `doctor --fix` resolves Release,
   "downloads", verifies a good checksum, places the binary, reports ready — **no compiler
   invoked** (assert the cargo path was never taken).
2. Docs: connections.md "Discovering and provisioning tools" + configuration.md updated to
   describe `discovery.registry: {uri, ref}` (always-latest) and `praxec init --with-starter-packs`
   / `--install-tools`. CHANGELOG entry under the next version.

**Acceptance:** the dev's exact dead-end (compile-on-Windows) is proven closed end-to-end in a
test; docs match the shipped surface.

---

## Self-review notes
- Coverage: installer (T2/T3), doctor offer+fix (T4), init (T5), always-latest registry (T1),
  cleanup/no-parallel (T6), proof+docs (T7) — every §6 Increment-I item has a task.
- Deferred items are explicitly excluded (Global Constraints).
- Type consistency: `InstallerIo`, `InstallOutcome`, `Provider`, `resolve_provider`/`install`
  names are shared across T2–T5.
- cpm-planner deliverable role = proof subject, zero new work — confirmed in T7.
