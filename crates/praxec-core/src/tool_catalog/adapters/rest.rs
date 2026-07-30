//! Generic `rest` registry adapter — integrates with ANY HTTP endpoint that
//! serves a JSON array of tool descriptors in the NORMALIZED SHAPE below.
//! This is the "bring your own registry" adapter: anything exposing this
//! shape works, no bespoke mapping needed (contrast [`super::github_org`] or
//! [`super::mcp_registry`], which map a specific upstream's native schema).
//!
//! Normalized response shape — a top-level JSON array, each element:
//!
//! ```json
//! [
//!   {
//!     "name": "browser-mcp",
//!     "description": "Playwright-backed browser automation MCP server.",
//!     "transport": "stdio",                 // stdio | docker | remote | rest
//!     "source": { "npm": { "pkg": "@playwright/mcp" } },
//!     // source is one of: {"repo":{"url":..}}, {"crate":{"name":..}},
//!     // {"npm":{"pkg":..}}, {"image":{"image":..}}, {"url":{"url":..}} —
//!     // the same tagged shape [`crate::tool_catalog::candidate::ToolSource`]
//!     // serializes to.
//!     "verbs": ["diagnose"],                // optional, default []
//!     "tags": ["browser", "e2e"],            // optional, default []
//!     "trust_tier": "verified",              // optional, default "community"
//!     "requires": { "secrets": [], "config": [] }  // optional, default empty
//!   }
//! ]
//! ```
//!
//! Only `name`, `transport`, and `source` are required per entry; a missing
//! or unparseable required field skips that one entry (never a hard error —
//! one bad entry must not sink the whole registry). A fetch-level failure
//! (bad URL, non-2xx, non-JSON body, or a top-level shape that isn't a JSON
//! array) is the only thing that surfaces as `Err`, which `assemble`
//! downgrades to a warning.

use crate::tool_catalog::candidate::{Requires, ToolCandidate, ToolSource, Transport, TrustTier};
use crate::tool_catalog::registry::{CatalogIo, RegistryAdapter};
use serde_json::Value;

/// Serves whatever a generic REST endpoint returns in the normalized shape
/// documented above.
pub struct RestAdapter {
    name: String,
    url: String,
}

impl RestAdapter {
    pub fn new(name: impl Into<String>, url: impl Into<String>) -> Self {
        Self {
            name: name.into(),
            url: url.into(),
        }
    }

    /// Map one entry of the normalized shape to a `ToolCandidate`. `None`
    /// means "skip" — a malformed entry, not a hard error.
    fn parse_one(&self, v: &Value) -> Option<ToolCandidate> {
        let name = v.get("name")?.as_str()?.to_string();
        let description = v
            .get("description")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_string();
        let transport: Transport = serde_json::from_value(v.get("transport")?.clone()).ok()?;
        let source: ToolSource = serde_json::from_value(v.get("source")?.clone()).ok()?;
        let verbs = v
            .get("verbs")
            .and_then(|x| serde_json::from_value(x.clone()).ok())
            .unwrap_or_default();
        let tags = v
            .get("tags")
            .and_then(|x| serde_json::from_value(x.clone()).ok())
            .unwrap_or_default();
        let trust_tier = v
            .get("trust_tier")
            .and_then(|x| serde_json::from_value(x.clone()).ok())
            .unwrap_or(TrustTier::Community);
        let requires: Requires = v
            .get("requires")
            .and_then(|x| serde_json::from_value(x.clone()).ok())
            .unwrap_or_default();
        Some(ToolCandidate {
            name,
            description,
            transport,
            source,
            verbs,
            tags,
            trust_tier,
            requires,
            provenance: self.name.clone(),
        })
    }
}

impl RegistryAdapter for RestAdapter {
    fn name(&self) -> &str {
        &self.name
    }

    fn candidates(&self, io: &dyn CatalogIo) -> Result<Vec<ToolCandidate>, String> {
        let body = io.fetch_json(&self.url)?;
        let entries = body.as_array().ok_or_else(|| {
            format!(
                "rest registry '{}' response was not a JSON array",
                self.name
            )
        })?;
        Ok(entries.iter().filter_map(|e| self.parse_one(e)).collect())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tool_catalog::registry::GhRepo;

    struct FakeRest(Value);
    impl CatalogIo for FakeRest {
        fn github_org_repos(&self, _org: &str) -> Result<Vec<GhRepo>, String> {
            Err("n/a".into())
        }
        fn fetch_json(&self, _u: &str) -> Result<Value, String> {
            Ok(self.0.clone())
        }
    }

    struct ErrIo;
    impl CatalogIo for ErrIo {
        fn github_org_repos(&self, _org: &str) -> Result<Vec<GhRepo>, String> {
            Err("n/a".into())
        }
        fn fetch_json(&self, _u: &str) -> Result<Value, String> {
            Err("connection refused".into())
        }
    }

    #[test]
    fn rest_adapter_maps_full_entry() {
        let io = FakeRest(serde_json::json!([
            {
                "name": "browser-mcp",
                "description": "Playwright browser automation",
                "transport": "stdio",
                "source": { "npm": { "pkg": "@playwright/mcp" } },
                "verbs": ["diagnose"],
                "tags": ["browser", "e2e"],
                "trust_tier": "verified",
                "requires": { "secrets": [{"name": "API_KEY", "description": "token"}], "config": [] }
            }
        ]));
        let out = RestAdapter::new("acme-rest", "https://example.com/tools")
            .candidates(&io)
            .unwrap();
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].name, "browser-mcp");
        assert_eq!(out[0].transport, Transport::Stdio);
        assert_eq!(
            out[0].source,
            ToolSource::Npm {
                pkg: "@playwright/mcp".into()
            }
        );
        assert_eq!(out[0].verbs, vec!["diagnose".to_string()]);
        assert_eq!(out[0].trust_tier, TrustTier::Verified);
        assert_eq!(out[0].requires.secrets.len(), 1);
        assert_eq!(out[0].provenance, "acme-rest");
    }

    #[test]
    fn rest_adapter_defaults_missing_optional_fields() {
        let io = FakeRest(serde_json::json!([
            {
                "name": "minimal-tool",
                "transport": "remote",
                "source": { "url": { "url": "https://tools.example.com/minimal" } }
            }
        ]));
        let out = RestAdapter::new("acme-rest", "https://example.com/tools")
            .candidates(&io)
            .unwrap();
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].description, "");
        assert!(out[0].verbs.is_empty());
        assert!(out[0].tags.is_empty());
        assert_eq!(out[0].trust_tier, TrustTier::Community); // default when omitted
        assert!(out[0].requires.secrets.is_empty());
    }

    #[test]
    fn rest_adapter_skips_malformed_entries_without_failing() {
        let io = FakeRest(serde_json::json!([
            { "description": "no name or transport or source" },
            { "name": "bad-transport", "transport": "carrier-pigeon", "source": { "url": { "url": "x" } } },
            { "name": "good", "transport": "docker", "source": { "image": { "image": "acme/good:latest" } } }
        ]));
        let out = RestAdapter::new("acme-rest", "https://example.com/tools")
            .candidates(&io)
            .unwrap();
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].name, "good");
    }

    #[test]
    fn rest_adapter_errors_on_non_array_body() {
        let io = FakeRest(serde_json::json!({ "message": "not an array" }));
        let err = RestAdapter::new("acme-rest", "https://example.com/tools")
            .candidates(&io)
            .unwrap_err();
        assert!(err.contains("not a JSON array"));
    }

    #[test]
    fn rest_adapter_propagates_fetch_error() {
        let err = RestAdapter::new("acme-rest", "https://example.com/tools")
            .candidates(&ErrIo)
            .unwrap_err();
        assert_eq!(err, "connection refused");
    }
}
