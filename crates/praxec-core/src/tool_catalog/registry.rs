//! `registries:` config parsing + the adapter/IO seam (Phase 1, §T2).
//!
//! A `RegistrySpec` is the typed projection of one entry in the config
//! `registries:` array. [`registries_from`] never hard-errors on a bad entry
//! — an unrecognized `kind` becomes [`RegistrySpec::Unknown`], surfaced as an
//! assembly warning ([`super::catalog::assemble`]), because data (config)
//! drives this, not compile-time knowledge of every registry that will ever
//! exist.
//!
//! [`RegistryAdapter`] is the per-registry mapping to [`ToolCandidate`]s; all
//! host IO it needs (network, `gh`, …) goes through [`CatalogIo`] — like
//! [`crate::currency::CurrencyIo`], this keeps every adapter pure and
//! unit-testable with a fake.

use super::adapters::mcp_registry::DEFAULT_MCP_REGISTRY_URL;
use super::candidate::ToolCandidate;
use serde_json::Value;

/// One registry entry from config, typed. Unknown kinds are preserved (not
/// dropped) so `assemble` can report them rather than silently ignore them.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RegistrySpec {
    Static {
        name: String,
        candidates: Vec<ToolCandidate>,
    },
    GithubOrg {
        name: String,
        org: String,
    },
    /// A generic REST endpoint serving the normalized shape documented in
    /// `adapters::rest`.
    Rest {
        name: String,
        url: String,
    },
    /// The Official MCP Registry (or a compatible mirror) — see
    /// `adapters::mcp_registry`. `url` defaults to
    /// [`DEFAULT_MCP_REGISTRY_URL`] when the config entry omits it.
    McpRegistry {
        name: String,
        url: String,
    },
    // Phase-3/Phase-1.5 follow-ups (Smithery/Glama/PulseMCP need their own
    // native-schema adapter, or can front the generic `rest` adapter behind a
    // normalizing endpoint — see the design doc's Phase 3 note).
    Unknown {
        name: String,
        kind: String,
    },
}

/// Parse the config `registries:` array into typed specs. Unknown kinds
/// become `Unknown` (a warning at assembly), never a hard error — data drives
/// this.
pub fn registries_from(config: &Value) -> Vec<RegistrySpec> {
    let Some(entries) = config.pointer("/registries").and_then(Value::as_array) else {
        return Vec::new();
    };
    entries
        .iter()
        .map(|entry| {
            let name = entry
                .get("name")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_string();
            let kind = entry
                .get("kind")
                .and_then(Value::as_str)
                .unwrap_or_default();
            match kind {
                "static" => {
                    let candidates = entry
                        .get("candidates")
                        .cloned()
                        .map(|c| serde_json::from_value(c).unwrap_or_default())
                        .unwrap_or_default();
                    RegistrySpec::Static { name, candidates }
                }
                "github-org" => {
                    let org = entry
                        .get("org")
                        .and_then(Value::as_str)
                        .unwrap_or_default()
                        .to_string();
                    RegistrySpec::GithubOrg { name, org }
                }
                "rest" => {
                    let url = entry
                        .get("url")
                        .and_then(Value::as_str)
                        .unwrap_or_default()
                        .to_string();
                    RegistrySpec::Rest { name, url }
                }
                "mcp-registry" => {
                    let url = entry
                        .get("url")
                        .and_then(Value::as_str)
                        .map(str::to_string)
                        .unwrap_or_else(|| DEFAULT_MCP_REGISTRY_URL.to_string());
                    RegistrySpec::McpRegistry { name, url }
                }
                _ => RegistrySpec::Unknown {
                    name,
                    kind: kind.to_string(),
                },
            }
        })
        .collect()
}

/// One registry's candidates. Fallible + async-free at this layer: the
/// adapter gets what it needs from `CatalogIo`, so it stays pure/testable.
pub trait RegistryAdapter {
    fn name(&self) -> &str;
    fn candidates(&self, io: &dyn CatalogIo) -> Result<Vec<ToolCandidate>, String>;
}

/// All host IO the adapters need (network/process), injectable like
/// `CurrencyIo`. `Send + Sync` so the production impl can be held as
/// `Arc<dyn CatalogIo>` and moved into a `tokio::task::spawn_blocking`
/// closure (catalog assembly does blocking network IO — see
/// [`super::real_io::RealCatalogIo`]).
pub trait CatalogIo: Send + Sync {
    /// GitHub org repos as (repo_name, description, topics) — used by
    /// `github_org`.
    fn github_org_repos(&self, org: &str) -> Result<Vec<GhRepo>, String>;
    /// Fetch a JSON document from a URL (mcp-registry / rest adapters, later
    /// tasks).
    fn fetch_json(&self, url: &str) -> Result<Value, String>;
}

/// One GitHub repo as enumerated for an org, projected to what the
/// `github_org` adapter needs.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GhRepo {
    pub name: String,
    pub description: String,
    pub topics: Vec<String>,
}

#[cfg(test)]
mod tests {
    use super::*;

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

    #[test]
    fn missing_registries_key_yields_empty() {
        let cfg = serde_json::json!({});
        assert!(registries_from(&cfg).is_empty());
    }

    #[test]
    fn parses_rest_with_explicit_url() {
        let cfg = serde_json::json!({ "registries": [
            { "kind": "rest", "name": "acme-rest", "url": "https://tools.acme.dev/registry.json" }
        ]});
        let specs = registries_from(&cfg);
        assert_eq!(specs.len(), 1);
        assert!(matches!(&specs[0], RegistrySpec::Rest { name, url }
            if name == "acme-rest" && url == "https://tools.acme.dev/registry.json"));
    }

    #[test]
    fn parses_mcp_registry_with_explicit_url() {
        let cfg = serde_json::json!({ "registries": [
            { "kind": "mcp-registry", "name": "official", "url": "https://mirror.example.com/v0/servers" }
        ]});
        let specs = registries_from(&cfg);
        assert!(matches!(&specs[0], RegistrySpec::McpRegistry { name, url }
            if name == "official" && url == "https://mirror.example.com/v0/servers"));
    }

    #[test]
    fn mcp_registry_defaults_url_when_omitted() {
        let cfg = serde_json::json!({ "registries": [
            { "kind": "mcp-registry", "name": "official" }
        ]});
        let specs = registries_from(&cfg);
        assert!(matches!(&specs[0], RegistrySpec::McpRegistry { url, .. }
            if url == DEFAULT_MCP_REGISTRY_URL));
    }
}
