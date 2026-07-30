# MCP Tool Discovery — Phase 1 Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Add a read-only MCP-tool discovery surface to praxec — a data-driven registry catalog normalizing to one `ToolCandidate`, exposed through two new `praxec.query` verbs (`discover`, `evaluate`).

**Architecture:** Mirror the proven `crates/praxec/src/currency.rs` shape: pure decision logic + typed data behind an injectable IO seam, so every adapter and the catalog assembly are unit-tested without network. Registries are declared in config (`registries:`), each parsed to a typed `RegistrySpec` and served by a `RegistryAdapter` that maps its native response to `ToolCandidate`. The catalog composes all adapters (dedup + 24h TTL cache). `discover`/`evaluate` are new read verbs on the existing two-tool query surface.

**Tech Stack:** Rust (praxec workspace), serde_json, existing `discovery::SearchRequest`/`SearchHit` ranked-search, `config.pointer("/…")` config access. No new external crates.

## Global Constraints

- **Reads only.** Phase 1 provisions nothing and mutates no config. No `flow.tools.*`, no installs.
- **Two-tool surface (SPEC §32) preserved.** `discover`/`evaluate` are dispatched by field-shape on `praxec.query`; no new top-level MCP tool.
- **Data, not code.** Registries come from the config `registries:` block. No hardcoded registry URLs in Rust beyond a well-known-default table that config overrides.
- **Verb vocabulary reuse.** A `ToolCandidate` is described by the cap-**verbs** it can serve (`praxec_core::cap_verb`), `tags` for the finer axis. No new taxonomy.
- **IO behind a seam.** All network/process IO goes through a `CatalogIo` trait (like `currency::CurrencyIo`); pure logic is unit-tested with a fake.
- **Fail-safe.** An adapter that errors yields zero candidates + a warning, never aborts the catalog. Advisory throughout.

---

## File Structure

- Create `crates/praxec-core/src/tool_catalog/mod.rs` — module root; re-exports.
- Create `crates/praxec-core/src/tool_catalog/candidate.rs` — `ToolCandidate`, `Transport`, `ToolSource`, `TrustTier`, `Requires`.
- Create `crates/praxec-core/src/tool_catalog/registry.rs` — `RegistrySpec` (config → typed), `RegistryAdapter` trait, `CatalogIo` trait.
- Create `crates/praxec-core/src/tool_catalog/adapters/static_adapter.rs` — inline `direct`/`static` candidates.
- Create `crates/praxec-core/src/tool_catalog/adapters/github_org.rs` — enumerate an org's repos → candidates.
- Create `crates/praxec-core/src/tool_catalog/catalog.rs` — compose adapters, dedup, TTL cache, `discover`/`evaluate` pure logic.
- Modify `crates/praxec-core/src/lib.rs` — `pub mod tool_catalog;`.
- Modify `crates/praxec-core/src/ports.rs` — route the `discover`/`evaluate` query shapes.
- Modify `crates/praxec-mcp-server/src/tools.rs` — surface the new verbs + the HATEOAS `home` link.

Each task ends with an independently testable deliverable.

---

### Task 1: `ToolCandidate` schema

**Files:**
- Create: `crates/praxec-core/src/tool_catalog/candidate.rs`
- Create: `crates/praxec-core/src/tool_catalog/mod.rs`
- Modify: `crates/praxec-core/src/lib.rs` (add `pub mod tool_catalog;`)

**Interfaces:**
- Produces: the normalized types every adapter emits and every verb consumes.

```rust
// candidate.rs
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Transport { Stdio, Docker, Remote, Rest }

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ToolSource {
    Repo { url: String },
    Crate { name: String },
    Npm { pkg: String },
    Image { image: String },
    Url { url: String },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TrustTier { Verified, Org, Community }

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Requires {
    #[serde(default)] pub secrets: Vec<RequiredField>,
    #[serde(default)] pub config: Vec<RequiredField>,
}
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RequiredField { pub name: String, pub description: String }

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ToolCandidate {
    pub name: String,
    pub description: String,
    pub transport: Transport,
    pub source: ToolSource,
    /// cap-verbs this tool can serve (praxec_core::cap_verb vocabulary).
    #[serde(default)] pub verbs: Vec<String>,
    #[serde(default)] pub tags: Vec<String>,
    pub trust_tier: TrustTier,
    #[serde(default)] pub requires: Requires,
    /// which registry surfaced it, for provenance + dedup tie-breaks.
    pub provenance: String,
}
```

- [ ] **Step 1: Write the failing test** — `candidate.rs` `#[cfg(test)]`

```rust
#[test]
fn tool_candidate_roundtrips_json() {
    let c = ToolCandidate {
        name: "browser-mcp".into(), description: "Playwright browser".into(),
        transport: Transport::Stdio, source: ToolSource::Npm { pkg: "@playwright/mcp".into() },
        verbs: vec!["diagnose".into(), "research".into()], tags: vec!["browser".into()],
        trust_tier: TrustTier::Verified, requires: Requires::default(),
        provenance: "mcp-registry".into(),
    };
    let v = serde_json::to_value(&c).unwrap();
    assert_eq!(v["transport"], "stdio");
    assert_eq!(v["source"]["npm"]["pkg"], "@playwright/mcp");
    let back: ToolCandidate = serde_json::from_value(v).unwrap();
    assert_eq!(back, c);
}
```

- [ ] **Step 2: Run it — expect FAIL** (`cargo test -p praxec-core tool_catalog::candidate`) — module not found.
- [ ] **Step 3: Implement** the types above; create `mod.rs` with `pub mod candidate; pub use candidate::*;`; add `pub mod tool_catalog;` to `lib.rs`.
- [ ] **Step 4: Run — expect PASS.**
- [ ] **Step 5: Commit** `feat(tool-catalog): ToolCandidate schema`.

---

### Task 2: `RegistrySpec` config parsing + adapter/IO traits

**Files:**
- Create: `crates/praxec-core/src/tool_catalog/registry.rs`
- Modify: `crates/praxec-core/src/tool_catalog/mod.rs`

**Interfaces:**
- Consumes: `serde_json::Value` config (the `registries:` array).
- Produces: `RegistrySpec`, `RegistryAdapter`, `CatalogIo`.

```rust
// registry.rs
use serde_json::Value;
use super::candidate::ToolCandidate;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RegistrySpec {
    Static { name: String, candidates: Vec<ToolCandidate> },
    GithubOrg { name: String, org: String },
    // Phase-1 stubs return an empty adapter until their task lands:
    // McpRegistry { name, url }, Rest { name, url }, …
    Unknown { name: String, kind: String },
}

/// Parse the config `registries:` array into typed specs. Unknown kinds become
/// `Unknown` (a warning at assembly), never a hard error — data drives this.
pub fn registries_from(config: &Value) -> Vec<RegistrySpec> { /* config.pointer("/registries") … */ }

/// One registry's candidates. Fallible + async-free at this layer: the adapter
/// gets what it needs from `CatalogIo`, so it stays pure/testable.
pub trait RegistryAdapter {
    fn name(&self) -> &str;
    fn candidates(&self, io: &dyn CatalogIo) -> Result<Vec<ToolCandidate>, String>;
}

/// All host IO the adapters need (network/process), injectable like CurrencyIo.
pub trait CatalogIo {
    /// GitHub org repos as (repo_name, description, topics) — used by github_org.
    fn github_org_repos(&self, org: &str) -> Result<Vec<GhRepo>, String>;
    /// Fetch a JSON document from a URL (mcp-registry / rest adapters, later tasks).
    fn fetch_json(&self, url: &str) -> Result<Value, String>;
}
pub struct GhRepo { pub name: String, pub description: String, pub topics: Vec<String> }
```

- [ ] **Step 1: Write the failing test** — parsing a mixed `registries:` array:

```rust
#[test]
fn parses_static_and_github_org_and_flags_unknown() {
    let cfg = serde_json::json!({ "registries": [
        { "kind": "github-org", "name": "praxec", "org": "praxec" },
        { "kind": "static", "name": "local", "candidates": [] },
        { "kind": "smithery", "name": "sm" }
    ]});
    let specs = registries_from(&cfg);
    assert_eq!(specs.len(), 3);
    assert!(matches!(&specs[0], RegistrySpec::GithubOrg { org, .. } if org == "praxec"));
    assert!(matches!(&specs[1], RegistrySpec::Static { .. }));
    assert!(matches!(&specs[2], RegistrySpec::Unknown { kind, .. } if kind == "smithery"));
}
```

- [ ] **Step 2: Run — expect FAIL.**
- [ ] **Step 3: Implement** `registries_from` (match on `kind`; deserialize `static` candidates via serde), the two traits, `GhRepo`.
- [ ] **Step 4: Run — expect PASS.**
- [ ] **Step 5: Commit** `feat(tool-catalog): registries: parsing + adapter/IO traits`.

---

### Task 3: `static`/`direct` adapter

**Files:**
- Create: `crates/praxec-core/src/tool_catalog/adapters/static_adapter.rs`
- Modify: `mod.rs` (`pub mod adapters;` + re-export)

**Interfaces:**
- Consumes: `RegistrySpec::Static`. Produces: its inline candidates verbatim, stamped with provenance.

- [ ] **Step 1: Write the failing test** — inline candidates pass through with provenance set:

```rust
#[test]
fn static_adapter_returns_its_candidates_with_provenance() {
    let cand = /* a ToolCandidate with provenance "" */;
    let a = StaticAdapter::new("local", vec![cand]);
    let out = a.candidates(&NoopIo).unwrap();
    assert_eq!(out.len(), 1);
    assert_eq!(out[0].provenance, "local"); // stamped by the adapter
}
```

- [ ] **Step 2: Run — expect FAIL.**
- [ ] **Step 3: Implement** `StaticAdapter` (holds name + candidates; `candidates()` clones them and sets `provenance = self.name`). Add a `NoopIo` test double in the module's tests.
- [ ] **Step 4: Run — expect PASS.**
- [ ] **Step 5: Commit** `feat(tool-catalog): static/direct adapter`.

---

### Task 4: `github-org` adapter

**Files:**
- Create: `crates/praxec-core/src/tool_catalog/adapters/github_org.rs`

**Interfaces:**
- Consumes: `RegistrySpec::GithubOrg` + `CatalogIo::github_org_repos`. Produces: one `ToolCandidate` per repo, `trust_tier = Org`, `transport = Stdio` default, `source = Repo{url}`, `verbs`/`tags` mapped from repo topics.

- [ ] **Step 1: Write the failing test** — pure mapping over a fake IO:

```rust
struct FakeGh(Vec<GhRepo>);
impl CatalogIo for FakeGh {
    fn github_org_repos(&self, _org: &str) -> Result<Vec<GhRepo>, String> { Ok(self.0.clone()) }
    fn fetch_json(&self, _u: &str) -> Result<serde_json::Value, String> { Err("n/a".into()) }
}

#[test]
fn github_org_maps_repos_to_org_tier_candidates() {
    let io = FakeGh(vec![GhRepo {
        name: "fmeca".into(), description: "FMECA MCP".into(),
        topics: vec!["mcp".into(), "review".into()],
    }]);
    let out = GithubOrgAdapter::new("praxec-org", "praxec").candidates(&io).unwrap();
    assert_eq!(out.len(), 1);
    assert_eq!(out[0].trust_tier, TrustTier::Org);
    assert!(out[0].source == ToolSource::Repo { url: "https://github.com/praxec/fmeca".into() });
    assert!(out[0].verbs.contains(&"review".into())); // topic → verb when it's a known cap-verb
    assert!(out[0].tags.contains(&"mcp".into()));
    assert_eq!(out[0].provenance, "praxec-org");
}
```

- [ ] **Step 2: Run — expect FAIL.**
- [ ] **Step 3: Implement** the adapter: for each repo, build the candidate; split topics into `verbs` (those that are valid `praxec_core::cap_verb` verbs) vs `tags` (the rest). Errors from IO → `Err(String)` (assembly downgrades to a warning).
- [ ] **Step 4: Run — expect PASS.**
- [ ] **Step 5: Commit** `feat(tool-catalog): github-org adapter`.

---

### Task 5: Catalog assembly + 24h TTL cache + `discover`/`evaluate` logic

**Files:**
- Create: `crates/praxec-core/src/tool_catalog/catalog.rs`

**Interfaces:**
- Consumes: `Vec<RegistrySpec>` + `CatalogIo` + a `now: i64` (injected — no `Date::now` in core).
- Produces:
  - `assemble(specs, io) -> (Vec<ToolCandidate>, Vec<String> /*warnings*/)` — run each adapter, collect, dedup by `(name, source)` keeping the highest trust tier.
  - `discover(catalog, query) -> Vec<ToolCandidate>` — rank by name/description/tags match (reuse `discovery::SearchRequest` scoring or a substring+verb score).
  - `evaluate(catalog, needed_verbs: &[String]) -> Vec<ToolCandidate>` — deterministic: candidates whose `verbs` intersect `needed_verbs`, ranked by intersection size then trust tier.
  - `Cache { fetched_at: i64, catalog: Vec<ToolCandidate> }` + `is_stale(now, ttl_secs=86_400)`.

- [ ] **Step 1: Write failing tests:**

```rust
#[test]
fn assemble_dedups_keeping_highest_trust() {
    // same tool from a community and a verified registry → one candidate, verified.
    let specs = vec![/* Static community "x" */, /* Static verified "x" */];
    let (cat, warns) = assemble(&specs, &NoopIo);
    assert_eq!(cat.iter().filter(|c| c.name == "x").count(), 1);
    assert_eq!(cat.iter().find(|c| c.name == "x").unwrap().trust_tier, TrustTier::Verified);
    assert!(warns.is_empty());
}

#[test]
fn evaluate_ranks_by_verb_overlap_then_trust() {
    let cat = vec![/* A verbs=[diagnose] community, B verbs=[diagnose,verify] org */];
    let hits = evaluate(&cat, &["diagnose".into(), "verify".into()]);
    assert_eq!(hits[0].name, "B"); // 2 overlaps beats 1
}

#[test]
fn cache_is_stale_after_ttl() {
    let c = Cache { fetched_at: 0, catalog: vec![] };
    assert!(c.is_stale(86_401, 86_400));
    assert!(!c.is_stale(100, 86_400));
}

#[test]
fn a_failing_adapter_becomes_a_warning_not_an_abort() {
    let specs = vec![/* GithubOrg with a FakeGh that errors */, /* Static ok */];
    let (cat, warns) = assemble(&specs, &ErroringGh);
    assert_eq!(warns.len(), 1);
    assert!(!cat.is_empty()); // the static one still landed
}
```

- [ ] **Step 2: Run — expect FAIL.**
- [ ] **Step 3: Implement** `assemble` (build the adapter per spec via a `spec.adapter()` factory; `Unknown` → warning), dedup, `discover`, `evaluate`, `Cache`.
- [ ] **Step 4: Run — expect PASS.**
- [ ] **Step 5: Commit** `feat(tool-catalog): assembly, TTL cache, discover/evaluate logic`.

---

### Task 6: Real `CatalogIo` + wire `discover`/`evaluate` into the query surface

**Files:**
- Create: `crates/praxec-core/src/tool_catalog/catalog.rs` (append `RealCatalogIo`)
- Modify: `crates/praxec-core/src/ports.rs` (route the query shapes)
- Modify: `crates/praxec-mcp-server/src/tools.rs` (field-shape dispatch + `home` link)

**Interfaces:**
- `RealCatalogIo`: `github_org_repos` shells `gh api` / the GitHub REST API (bounded, best-effort → `Err` on failure); `fetch_json` a bounded HTTP GET.
- `praxec.query { discover: "<q>" }` and `praxec.query { evaluate: { verbs: [...] } }` dispatched by present-field shape, returning ranked `ToolCandidate`s as JSON (HATEOAS: each carries a follow-up hint, no provision link in Phase 1).

- [ ] **Step 1: Write the failing test** — dispatch routing (unit over the port, fake catalog):

```rust
#[test]
fn query_discover_shape_routes_to_catalog_discover() {
    // a QueryRequest with `discover: "browser"` returns candidates, not a search error.
    let resp = handle_query(json!({ "discover": "browser" }), &fixture_catalog());
    assert!(resp["items"].as_array().unwrap().iter().any(|i| i["name"] == "browser-mcp"));
}
```

- [ ] **Step 2: Run — expect FAIL.**
- [ ] **Step 3: Implement** `RealCatalogIo`; add the two field-shapes to the query dispatch in `ports.rs`; register them in `tools.rs` and add a `discover` link to the `home` response.
- [ ] **Step 4: Run — expect PASS** (+ `cargo test -p praxec-core -p praxec-mcp-server tool_catalog`).
- [ ] **Step 5: Commit** `feat(tool-catalog): real IO + discover/evaluate query verbs`.

---

### Task 7: Docs + example config

**Files:**
- Modify: `docs/` (a short `docs/reference/tool-discovery.md`)
- Create: `examples/tool-discovery/gateway.yaml` — a `registries:` block with a `github-org: praxec` + a `static` entry, runnable via `praxec.query { discover: … }`.

- [ ] **Step 1:** Write the example config + a `praxec check` smoke (the config loads).
- [ ] **Step 2:** Document the `registries:` shape, the `ToolCandidate` fields, and the two verbs.
- [ ] **Step 3: Commit** `docs(tool-catalog): registries: reference + example`.

---

## Non-goals (Phase 2+)

- Provisioning / `flow.tools.*`, secrets elicitation, deprovision (Phase 2).
- `mcp-registry` (Official Index), Smithery/Glama/PulseMCP, generic `rest` adapters (Phase 1.5 / 3 — the framework here makes each a small task).
- The praxec-meta `suggest-tools` authoring integration (Phase 3).
- Tool auto-update on the TTL (Phase 2 — trust-gated).

## Self-Review

- **Spec coverage:** registries-as-data ✅ (T2), ToolCandidate ✅ (T1), discover/evaluate ✅ (T5/T6), verb-vocabulary reuse ✅ (T4/T5), two adapters proving the framework ✅ (T3/T4). Provision/secrets/authoring correctly deferred.
- **Type consistency:** `ToolCandidate`/`ToolSource`/`TrustTier` defined in T1 are used unchanged through T7; `CatalogIo`/`RegistryAdapter` from T2 are the only IO seam.
- **No placeholders:** each task carries real interfaces + representative test code; the implementer reads `ports.rs`/`tools.rs` for the exact dispatch site (pointed to, not guessed).
