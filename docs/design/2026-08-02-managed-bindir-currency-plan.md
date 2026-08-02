# Managed-bin-dir awareness + doctor --fix freshen — plan

> REQUIRED SUB-SKILL: subagent-driven-development. TDD. One praxec implementer at a time (cargo lock).

**Goal:** Close two rough edges found dogfooding v0.0.46: (1) tools installed by `praxec tools install` into the managed bin dir (`~/.config/praxec/bin`) read as **missing** to preflight and **stale** to currency (both only look at PATH/cargo); (2) `doctor --fix` only installs *missing* tools, not *stale-present* ones, yet the currency remediation says "(or `praxec doctor --fix`)".

**Context:** Increment I added `provision_install::managed_bin_dir() -> Option<PathBuf>` and prepends it to MCP child spawns — but `provision::detect` (existence) and `currency::classify` (currency) don't consult it. Registry (`registry_v3`) is loaded by `gateway.rs::load_registry`.

## Global Constraints
- Reuse `managed_bin_dir()` (single source), `parse_version` (io.rs), the installer, the registry. No parallel abstractions.
- Fail-safe currency (advisory; never block). Consent preserved (doctor --fix is the consent).
- assert-don't-derive; fmt+clippy clean; match style.

---

### Task 1: managed-bin-dir awareness in `provision::detect` + `currency`
**Files:** `crates/praxec/src/provision.rs`, `crates/praxec/src/currency.rs`, `crates/praxec/src/gateway.rs` (thread registry version into the currency call).

1. **provision::detect** (`provision.rs:29`): a `kind: mcp` command counts as PRESENT if `which::which(cmd)` OR `managed_bin_dir()/cmd` (or `.exe`) exists. So a managed-installed tool shows `ok`, not `missing`.
2. **currency**: add `ConnSource::ManagedRelease { command, installed_version: Option<String>, expected_version: Option<String> }`. In `classify`, **before** the crates2/PATH arms, if `managed_bin_dir()/command` exists, classify as `ManagedRelease` (a praxec-installed release binary supersedes a stale cargo copy for currency purposes). `installed_version` via `<managed>/<command> --version` (reuse `parse_version`). `expected_version` = the registry tool's `version` for that command — thread a `command -> version` map (from the loaded `Registry`) into `check_currency`/`conn_specs_from` so a `ManagedRelease` tool compares installed-vs-registry: equal → `TOOL_CURRENT`; differ → `TOOL_BEHIND_REGISTRY` (Warn) with fix `praxec tools install <id>`; unknown version → `CURRENCY_UNKNOWN` (Info). Do NOT change the other ConnSource arms' behavior.

**Tests:** (a) `detect` reports a tool present when only in the managed dir (fake managed dir via an injectable path or a temp `HOME`/config dir). (b) `classify` returns `ManagedRelease` when the managed binary exists, even if a cargo entry also exists (managed wins). (c) a `ManagedRelease` tool whose `--version` == registry version → `TOOL_CURRENT`; older → `TOOL_BEHIND_REGISTRY`. Use the `CurrencyIo` seam / injected managed dir — no real global state.

**Accept:** a `praxec tools install`-ed tool shows `ok` (preflight) and current (currency), not missing/cargo-stale.

---

### Task 2: `doctor --fix` freshens stale registry tools + accurate remediation
**Files:** `crates/praxec/src/gateway.rs` (doctor), `crates/praxec/src/currency.rs` (remediation text).

1. **doctor --fix**: after the currency pass, for each tool currency reports **behind** (`TOOL_BEHIND_SOURCE`/`TOOL_BEHIND_REGISTRY`) that is a **registry tool** (has an entry), run `provision_install::install(..., Consent::Granted, ...)` to freshen it (place the current release binary in the managed dir). Print the outcome. Offer-only (no `--fix`) still just reports. Continue past per-tool failures. A non-registry stale tool (e.g. `EXTERNAL_UNCHECKABLE`, or a local-cargo tool with no registry entry) is NOT auto-freshened — say so.
2. **remediation text** (`currency.rs`): keep `praxec tools install <id>` as the primary fix; only mention `doctor --fix` where it now actually acts (registry tools). For a local-cargo-path tool with no registry entry, the fix is `cargo install --path … --force` OR "add it to the registry" — not `doctor --fix`. Make the message accurate to what will happen.

**Tests:** (a) `doctor --fix` with a fake installer IO + a fixture registry: a `TOOL_BEHIND_REGISTRY` registry tool gets `install` called (Granted); a non-registry stale tool does not. (b) offer-only → no install. (c) the remediation string for a registry tool mentions `doctor --fix`; for a non-registry local-cargo tool it does not.

**Accept:** `doctor --fix` actually freshens stale registry tools; the remediation text no longer overpromises.

---

## After merge + local install
Build+install locally (`make install`), then Track 2 (env cleanup): switch the org packs to `uri:{ref:main}` in the user config; `praxec doctor --fix` to freshen cpm-planner/crossmatrix onto release binaries (now recognized); leave writable run-targets; note github-mcp-server.
