//! `ToolCandidate` — the one normalized shape every registry adapter emits and
//! every discovery/evaluate verb consumes (Phase 1, MCP tool discovery §T1).
//!
//! Mirrors [`crate::currency`]'s split: typed data here, pure decision logic
//! in [`super::catalog`], host IO behind [`super::registry::CatalogIo`]. This
//! module is data only — no IO, no adapters.

use serde::{Deserialize, Serialize};

/// How the tool is launched once installed.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Transport {
    Stdio,
    Docker,
    Remote,
    Rest,
}

/// Where the tool comes from — enough to install or reference it later
/// (Phase 2 provisioning), advisory-only in Phase 1.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ToolSource {
    Repo { url: String },
    Crate { name: String },
    Npm { pkg: String },
    Image { image: String },
    Url { url: String },
}

/// How much a registry's provenance is trusted. Drives dedup tie-breaks in
/// [`super::catalog::assemble`] — when the same tool surfaces from more than
/// one registry, the highest tier wins.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TrustTier {
    Community,
    Org,
    Verified,
}

/// One field a candidate needs at provision time (Phase 2) — named here so
/// Phase 1 can surface it advisory, without acting on it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RequiredField {
    pub name: String,
    pub description: String,
}

/// What a candidate needs before it can run — secrets and/or config. Empty by
/// default: most catalog entries need nothing beyond installation.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Requires {
    #[serde(default)]
    pub secrets: Vec<RequiredField>,
    #[serde(default)]
    pub config: Vec<RequiredField>,
}

/// A normalized MCP tool as surfaced by any registry adapter.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ToolCandidate {
    pub name: String,
    pub description: String,
    pub transport: Transport,
    pub source: ToolSource,
    /// cap-verbs this tool can serve ([`crate::cap_verb`] vocabulary).
    #[serde(default)]
    pub verbs: Vec<String>,
    #[serde(default)]
    pub tags: Vec<String>,
    pub trust_tier: TrustTier,
    #[serde(default)]
    pub requires: Requires,
    /// which registry surfaced it, for provenance + dedup tie-breaks.
    pub provenance: String,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tool_candidate_roundtrips_json() {
        let c = ToolCandidate {
            name: "browser-mcp".into(),
            description: "Playwright browser".into(),
            transport: Transport::Stdio,
            source: ToolSource::Npm {
                pkg: "@playwright/mcp".into(),
            },
            verbs: vec!["diagnose".into(), "research".into()],
            tags: vec!["browser".into()],
            trust_tier: TrustTier::Verified,
            requires: Requires::default(),
            provenance: "mcp-registry".into(),
        };
        let v = serde_json::to_value(&c).unwrap();
        assert_eq!(v["transport"], "stdio");
        assert_eq!(v["source"]["npm"]["pkg"], "@playwright/mcp");
        let back: ToolCandidate = serde_json::from_value(v).unwrap();
        assert_eq!(back, c);
    }
}
