//! `github-org` registry adapter — enumerates an org's repos via
//! [`CatalogIo::github_org_repos`] and maps each to one [`ToolCandidate`].
//!
//! Topic mapping: a repo's GitHub topics split into `verbs` (topics that are
//! valid [`crate::cap_verb`] tokens) and `tags` (everything else) — the
//! closed cap-verb vocabulary is the only taxonomy this reuses, per the
//! design doc's "verb vocabulary reuse" constraint.

use crate::cap_verb::CapVerb;
use crate::tool_catalog::candidate::{Requires, ToolCandidate, ToolSource, Transport, TrustTier};
use crate::tool_catalog::registry::{CatalogIo, RegistryAdapter};

/// Serves the repos of one GitHub org as `Org`-tier candidates.
pub struct GithubOrgAdapter {
    name: String,
    org: String,
}

impl GithubOrgAdapter {
    pub fn new(name: impl Into<String>, org: impl Into<String>) -> Self {
        Self {
            name: name.into(),
            org: org.into(),
        }
    }
}

impl RegistryAdapter for GithubOrgAdapter {
    fn name(&self) -> &str {
        &self.name
    }

    fn candidates(&self, io: &dyn CatalogIo) -> Result<Vec<ToolCandidate>, String> {
        let repos = io.github_org_repos(&self.org)?;
        Ok(repos
            .into_iter()
            .map(|repo| {
                let (verbs, tags): (Vec<String>, Vec<String>) = repo
                    .topics
                    .into_iter()
                    .partition(|t| CapVerb::from_token(t).is_some());
                ToolCandidate {
                    name: repo.name.clone(),
                    description: repo.description,
                    transport: Transport::Stdio,
                    source: ToolSource::Repo {
                        url: format!("https://github.com/{}/{}", self.org, repo.name),
                    },
                    verbs,
                    tags,
                    trust_tier: TrustTier::Org,
                    requires: Requires::default(),
                    provenance: self.name.clone(),
                }
            })
            .collect())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tool_catalog::registry::GhRepo;
    use serde_json::Value;

    struct FakeGh(Vec<GhRepo>);
    impl CatalogIo for FakeGh {
        fn github_org_repos(&self, _org: &str) -> Result<Vec<GhRepo>, String> {
            Ok(self.0.clone())
        }
        fn fetch_json(&self, _u: &str) -> Result<Value, String> {
            Err("n/a".into())
        }
    }

    #[test]
    fn github_org_maps_repos_to_org_tier_candidates() {
        let io = FakeGh(vec![GhRepo {
            name: "fmeca".into(),
            description: "FMECA MCP".into(),
            topics: vec!["mcp".into(), "review".into()],
        }]);
        let out = GithubOrgAdapter::new("praxec-org", "praxec")
            .candidates(&io)
            .unwrap();
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].trust_tier, TrustTier::Org);
        assert!(
            out[0].source
                == ToolSource::Repo {
                    url: "https://github.com/praxec/fmeca".into()
                }
        );
        assert!(out[0].verbs.contains(&"review".into())); // topic → verb when it's a known cap-verb
        assert!(out[0].tags.contains(&"mcp".into()));
        assert_eq!(out[0].provenance, "praxec-org");
    }

    #[test]
    fn adapter_propagates_io_error() {
        struct ErrIo;
        impl CatalogIo for ErrIo {
            fn github_org_repos(&self, _org: &str) -> Result<Vec<GhRepo>, String> {
                Err("rate limited".into())
            }
            fn fetch_json(&self, _u: &str) -> Result<Value, String> {
                Err("n/a".into())
            }
        }
        let err = GithubOrgAdapter::new("praxec-org", "praxec")
            .candidates(&ErrIo)
            .unwrap_err();
        assert_eq!(err, "rate limited");
    }
}
