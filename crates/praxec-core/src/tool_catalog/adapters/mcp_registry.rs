//! `mcp-registry` adapter — the Official MCP Registry
//! (<https://registry.modelcontextprotocol.io>), the community-run index of
//! published MCP servers. Every candidate it surfaces is `TrustTier::Verified`
//! — this registry is the one Phase 1's design doc calls out by name as the
//! canonical "verified" source (contrast `github-org` → `Org`, `rest`/`static`
//! → whatever the config/entry says, defaulting to `Community`).
//!
//! SCHEMA ASSUMPTION (no network in this sandbox to confirm against a live
//! call): mapped against the registry's `v0` server-list schema as documented
//! as of 2026-07-30. A response is either `{ "servers": [...] }` or a bare
//! top-level array (some registry mirrors/proxies flatten the envelope); this
//! adapter accepts either. Each server entry is assumed to look like:
//!
//! ```json
//! {
//!   "name": "io.github.acme/browser-mcp",
//!   "description": "Playwright-backed browser automation MCP server.",
//!   "repository": { "url": "https://github.com/acme/browser-mcp", "source": "github" },
//!   "packages": [
//!     { "registry_name": "npm", "name": "@acme/browser-mcp", "version": "1.2.0" }
//!   ],
//!   "remotes": [
//!     { "transport_type": "sse", "url": "https://mcp.acme.dev/browser" }
//!   ]
//! }
//! ```
//!
//! If upstream's field names/paths differ from this, only this module needs
//! updating — the mapping below is deliberately isolated from the rest of
//! `tool_catalog`.
//!
//! Mapping precedence (first that applies wins), since a server entry may
//! list more than one of `packages`/`remotes`/`repository`:
//! 1. `packages[0]` — `registry_name: "npm"` → [`ToolSource::Npm`] +
//!    [`Transport::Stdio`]; `"docker"`/`"oci"` → [`ToolSource::Image`] +
//!    [`Transport::Docker`]; any other registry name falls back to the repo
//!    URL (if present) else an `Npm` source, still `Stdio` (most MCP server
//!    packages are stdio-launched).
//! 2. `remotes[0].url` (no packages) → [`ToolSource::Url`] +
//!    [`Transport::Remote`].
//! 3. `repository.url` alone (no packages/remotes) → [`ToolSource::Repo`] +
//!    [`Transport::Stdio`].
//!
//! An entry with none of the three is skipped — there's nothing to install
//! or connect to.
//!
//! This schema has no verb/tag taxonomy, so `verbs`/`tags` are always empty;
//! `requires` is always empty too (the registry doesn't publish a
//! secrets/config contract as of this writing).

use crate::tool_catalog::candidate::{Requires, ToolCandidate, ToolSource, Transport, TrustTier};
use crate::tool_catalog::registry::{CatalogIo, RegistryAdapter};
use serde_json::Value;

/// The Official MCP Registry's default servers-list endpoint. Used when a
/// `kind: mcp-registry` config entry omits `url`.
pub const DEFAULT_MCP_REGISTRY_URL: &str = "https://registry.modelcontextprotocol.io/v0/servers";

/// Serves the Official MCP Registry (or a compatible mirror at a different
/// `url`) as `Verified`-tier candidates.
pub struct McpRegistryAdapter {
    name: String,
    url: String,
}

impl McpRegistryAdapter {
    pub fn new(name: impl Into<String>, url: impl Into<String>) -> Self {
        Self {
            name: name.into(),
            url: url.into(),
        }
    }

    /// Map one server entry to a `ToolCandidate`. `None` means "skip" — a
    /// malformed entry, or one with no usable install/connect info.
    fn parse_one(&self, v: &Value) -> Option<ToolCandidate> {
        let name = v.get("name")?.as_str()?.to_string();
        let description = v
            .get("description")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_string();
        let repo_url = v.pointer("/repository/url").and_then(Value::as_str);
        let packages = v
            .get("packages")
            .and_then(Value::as_array)
            .filter(|p| !p.is_empty());
        let remotes = v
            .get("remotes")
            .and_then(Value::as_array)
            .filter(|r| !r.is_empty());

        let (transport, source) = if let Some(pkgs) = packages {
            let pkg = &pkgs[0];
            let registry_name = pkg
                .get("registry_name")
                .and_then(Value::as_str)
                .unwrap_or("");
            let pkg_name = pkg
                .get("name")
                .and_then(Value::as_str)
                .unwrap_or(&name)
                .to_string();
            match registry_name {
                "npm" => (Transport::Stdio, ToolSource::Npm { pkg: pkg_name }),
                "docker" | "oci" => (Transport::Docker, ToolSource::Image { image: pkg_name }),
                _ => (
                    Transport::Stdio,
                    repo_url
                        .map(|u| ToolSource::Repo { url: u.to_string() })
                        .unwrap_or(ToolSource::Npm { pkg: pkg_name }),
                ),
            }
        } else if let Some(rems) = remotes {
            let url = rems[0].get("url").and_then(Value::as_str)?.to_string();
            (Transport::Remote, ToolSource::Url { url })
        } else {
            let url = repo_url?; // no packages, no remotes, no repository — nothing to install/connect
            (
                Transport::Stdio,
                ToolSource::Repo {
                    url: url.to_string(),
                },
            )
        };

        Some(ToolCandidate {
            name,
            description,
            transport,
            source,
            verbs: Vec::new(),
            tags: Vec::new(),
            trust_tier: TrustTier::Verified,
            requires: Requires::default(),
            provenance: self.name.clone(),
        })
    }
}

impl RegistryAdapter for McpRegistryAdapter {
    fn name(&self) -> &str {
        &self.name
    }

    fn candidates(&self, io: &dyn CatalogIo) -> Result<Vec<ToolCandidate>, String> {
        let body = io.fetch_json(&self.url)?;
        let entries = body
            .get("servers")
            .and_then(Value::as_array)
            .or_else(|| body.as_array())
            .ok_or_else(|| {
                format!(
                    "mcp-registry '{}' response had no 'servers' array",
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

    struct FakeRegistry(Value);
    impl CatalogIo for FakeRegistry {
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
            Err("timed out".into())
        }
    }

    #[test]
    fn maps_npm_package_entry_to_stdio_npm_verified() {
        let io = FakeRegistry(serde_json::json!({ "servers": [
            {
                "name": "io.github.acme/browser-mcp",
                "description": "Playwright-backed browser automation MCP server.",
                "repository": { "url": "https://github.com/acme/browser-mcp", "source": "github" },
                "packages": [
                    { "registry_name": "npm", "name": "@acme/browser-mcp", "version": "1.2.0" }
                ]
            }
        ]}));
        let out = McpRegistryAdapter::new("official", DEFAULT_MCP_REGISTRY_URL)
            .candidates(&io)
            .unwrap();
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].name, "io.github.acme/browser-mcp");
        assert_eq!(out[0].transport, Transport::Stdio);
        assert_eq!(
            out[0].source,
            ToolSource::Npm {
                pkg: "@acme/browser-mcp".into()
            }
        );
        assert_eq!(out[0].trust_tier, TrustTier::Verified);
        assert_eq!(out[0].provenance, "official");
    }

    #[test]
    fn maps_docker_package_entry_to_docker_image() {
        let io = FakeRegistry(serde_json::json!({ "servers": [
            {
                "name": "io.github.acme/sandbox-mcp",
                "description": "Sandboxed exec MCP server.",
                "packages": [
                    { "registry_name": "docker", "name": "acme/sandbox-mcp", "version": "0.4.0" }
                ]
            }
        ]}));
        let out = McpRegistryAdapter::new("official", DEFAULT_MCP_REGISTRY_URL)
            .candidates(&io)
            .unwrap();
        assert_eq!(out[0].transport, Transport::Docker);
        assert_eq!(
            out[0].source,
            ToolSource::Image {
                image: "acme/sandbox-mcp".into()
            }
        );
    }

    #[test]
    fn maps_remote_only_entry_to_remote_url() {
        let io = FakeRegistry(serde_json::json!({ "servers": [
            {
                "name": "io.github.acme/remote-mcp",
                "description": "Hosted MCP server.",
                "remotes": [
                    { "transport_type": "sse", "url": "https://mcp.acme.dev/remote" }
                ]
            }
        ]}));
        let out = McpRegistryAdapter::new("official", DEFAULT_MCP_REGISTRY_URL)
            .candidates(&io)
            .unwrap();
        assert_eq!(out[0].transport, Transport::Remote);
        assert_eq!(
            out[0].source,
            ToolSource::Url {
                url: "https://mcp.acme.dev/remote".into()
            }
        );
    }

    #[test]
    fn maps_repository_only_entry_to_repo_stdio() {
        let io = FakeRegistry(serde_json::json!({ "servers": [
            {
                "name": "io.github.acme/repo-only-mcp",
                "description": "Repo-only entry, no packages/remotes.",
                "repository": { "url": "https://github.com/acme/repo-only-mcp", "source": "github" }
            }
        ]}));
        let out = McpRegistryAdapter::new("official", DEFAULT_MCP_REGISTRY_URL)
            .candidates(&io)
            .unwrap();
        assert_eq!(out[0].transport, Transport::Stdio);
        assert_eq!(
            out[0].source,
            ToolSource::Repo {
                url: "https://github.com/acme/repo-only-mcp".into()
            }
        );
    }

    #[test]
    fn skips_entries_with_no_packages_remotes_or_repository() {
        let io = FakeRegistry(serde_json::json!({ "servers": [
            { "name": "io.github.acme/nothing-mcp", "description": "no install info" },
            {
                "name": "io.github.acme/good-mcp",
                "packages": [ { "registry_name": "npm", "name": "@acme/good-mcp" } ]
            }
        ]}));
        let out = McpRegistryAdapter::new("official", DEFAULT_MCP_REGISTRY_URL)
            .candidates(&io)
            .unwrap();
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].name, "io.github.acme/good-mcp");
    }

    #[test]
    fn accepts_bare_top_level_array_envelope() {
        let io = FakeRegistry(serde_json::json!([
            {
                "name": "io.github.acme/bare-mcp",
                "packages": [ { "registry_name": "npm", "name": "@acme/bare-mcp" } ]
            }
        ]));
        let out = McpRegistryAdapter::new("official", DEFAULT_MCP_REGISTRY_URL)
            .candidates(&io)
            .unwrap();
        assert_eq!(out.len(), 1);
    }

    #[test]
    fn errors_when_no_servers_array_present() {
        let io = FakeRegistry(serde_json::json!({ "message": "not found" }));
        let err = McpRegistryAdapter::new("official", DEFAULT_MCP_REGISTRY_URL)
            .candidates(&io)
            .unwrap_err();
        assert!(err.contains("no 'servers' array"));
    }

    #[test]
    fn propagates_fetch_error() {
        let err = McpRegistryAdapter::new("official", DEFAULT_MCP_REGISTRY_URL)
            .candidates(&ErrIo)
            .unwrap_err();
        assert_eq!(err, "timed out");
    }
}
