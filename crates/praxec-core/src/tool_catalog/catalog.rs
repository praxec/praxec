//! Catalog assembly + TTL cache + `discover`/`evaluate` pure logic
//! (Phase 1, §T5).
//!
//! [`assemble`] composes every configured registry's adapter into one
//! deduplicated catalog. Fail-safe throughout, mirroring [`crate::currency`]:
//! an adapter `Err` — or a [`super::registry::RegistrySpec::Unknown`] kind —
//! becomes a warning string, never an abort. The rest of this module is pure
//! ranking over an already-assembled `Vec<ToolCandidate>` plus `now: i64`
//! passed in by the caller (no `Date::now`/`Instant::now` in this crate).

use super::adapters::{GithubOrgAdapter, StaticAdapter};
use super::candidate::ToolCandidate;
use super::registry::{CatalogIo, RegistryAdapter, RegistrySpec};
use std::collections::HashMap;

/// A dedup key: same name + same source string-form is "the same tool" for
/// tie-breaking, even when two registries both surface it.
fn dedup_key(c: &ToolCandidate) -> (String, String) {
    (c.name.clone(), format!("{:?}", c.source))
}

/// Run one adapter, folding its candidates into `by_key` (keeping the
/// highest-trust copy on a collision) or recording a warning on `Err`.
fn collect(
    adapter: &dyn RegistryAdapter,
    io: &dyn CatalogIo,
    by_key: &mut HashMap<(String, String), ToolCandidate>,
    warnings: &mut Vec<String>,
) {
    match adapter.candidates(io) {
        Ok(candidates) => {
            for c in candidates {
                let key = dedup_key(&c);
                match by_key.get(&key) {
                    Some(existing) if existing.trust_tier >= c.trust_tier => {}
                    _ => {
                        by_key.insert(key, c);
                    }
                }
            }
        }
        Err(e) => warnings.push(format!("registry '{}' failed: {e}", adapter.name())),
    }
}

/// Assemble every configured registry into one deduplicated catalog. Never
/// aborts: a failing adapter or an `Unknown` registry kind downgrades to a
/// warning in the second return value, and every other registry still lands.
pub fn assemble(specs: &[RegistrySpec], io: &dyn CatalogIo) -> (Vec<ToolCandidate>, Vec<String>) {
    let mut by_key: HashMap<(String, String), ToolCandidate> = HashMap::new();
    let mut warnings = Vec::new();
    for spec in specs {
        match spec {
            RegistrySpec::Static { name, candidates } => {
                let adapter = StaticAdapter::new(name.clone(), candidates.clone());
                collect(&adapter, io, &mut by_key, &mut warnings);
            }
            RegistrySpec::GithubOrg { name, org } => {
                let adapter = GithubOrgAdapter::new(name.clone(), org.clone());
                collect(&adapter, io, &mut by_key, &mut warnings);
            }
            RegistrySpec::Unknown { name, kind } => {
                warnings.push(format!(
                    "registry '{name}' has unknown kind '{kind}' — skipped"
                ));
            }
        }
    }
    let mut catalog: Vec<ToolCandidate> = by_key.into_values().collect();
    catalog.sort_by(|a, b| a.name.cmp(&b.name));
    (catalog, warnings)
}

/// Rank the catalog against a free-text query: substring hits on `name`
/// (strongest), `description`, then `tags`/`verbs` (weakest), case-insensitive.
/// Zero-score candidates are dropped; ties keep catalog order (name-sorted).
pub fn discover(catalog: &[ToolCandidate], query: &str) -> Vec<ToolCandidate> {
    let q = query.to_lowercase();
    if q.is_empty() {
        return catalog.to_vec();
    }
    let mut scored: Vec<(i32, &ToolCandidate)> = catalog
        .iter()
        .map(|c| (discover_score(c, &q), c))
        .filter(|(score, _)| *score > 0)
        .collect();
    scored.sort_by_key(|(score, _)| std::cmp::Reverse(*score));
    scored.into_iter().map(|(_, c)| c.clone()).collect()
}

fn discover_score(c: &ToolCandidate, query_lower: &str) -> i32 {
    let mut score = 0;
    if c.name.to_lowercase().contains(query_lower) {
        score += 3;
    }
    if c.description.to_lowercase().contains(query_lower) {
        score += 2;
    }
    let tag_hit = c
        .tags
        .iter()
        .chain(c.verbs.iter())
        .any(|t| t.to_lowercase().contains(query_lower));
    if tag_hit {
        score += 1;
    }
    score
}

/// Rank the catalog by overlap with `needed_verbs`: candidates whose `verbs`
/// intersect at all, sorted by intersection size (desc) then trust tier
/// (desc). Deterministic, no scoring magic beyond those two keys.
pub fn evaluate(catalog: &[ToolCandidate], needed_verbs: &[String]) -> Vec<ToolCandidate> {
    let mut hits: Vec<(usize, &ToolCandidate)> = catalog
        .iter()
        .map(|c| {
            let overlap = c.verbs.iter().filter(|v| needed_verbs.contains(v)).count();
            (overlap, c)
        })
        .filter(|(overlap, _)| *overlap > 0)
        .collect();
    hits.sort_by(|a, b| {
        b.0.cmp(&a.0)
            .then_with(|| b.1.trust_tier.cmp(&a.1.trust_tier))
    });
    hits.into_iter().map(|(_, c)| c.clone()).collect()
}

/// A cached catalog snapshot with a fetch time, for the 24h TTL policy.
/// `now`/`ttl_secs` are always caller-supplied — this crate never reads the
/// clock itself.
#[derive(Debug, Clone)]
pub struct Cache {
    pub fetched_at: i64,
    pub catalog: Vec<ToolCandidate>,
}

impl Cache {
    /// `true` once `now` is more than `ttl_secs` past `fetched_at`.
    pub fn is_stale(&self, now: i64, ttl_secs: i64) -> bool {
        now - self.fetched_at > ttl_secs
    }
}

/// Default TTL for the catalog cache: 24 hours.
pub const DEFAULT_TTL_SECS: i64 = 86_400;

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tool_catalog::candidate::{Requires, ToolSource, Transport, TrustTier};
    use crate::tool_catalog::registry::GhRepo;
    use serde_json::Value;

    struct NoopIo;
    impl CatalogIo for NoopIo {
        fn github_org_repos(&self, _org: &str) -> Result<Vec<GhRepo>, String> {
            Err("unused".into())
        }
        fn fetch_json(&self, _url: &str) -> Result<Value, String> {
            Err("unused".into())
        }
    }

    struct ErroringGh;
    impl CatalogIo for ErroringGh {
        fn github_org_repos(&self, _org: &str) -> Result<Vec<GhRepo>, String> {
            Err("rate limited".into())
        }
        fn fetch_json(&self, _url: &str) -> Result<Value, String> {
            Err("unused".into())
        }
    }

    fn candidate(name: &str, trust_tier: TrustTier) -> ToolCandidate {
        ToolCandidate {
            name: name.to_string(),
            description: format!("{name} description"),
            transport: Transport::Stdio,
            source: ToolSource::Crate {
                name: name.to_string(),
            },
            verbs: vec![],
            tags: vec![],
            trust_tier,
            requires: Requires::default(),
            provenance: "".into(),
        }
    }

    fn candidate_with_verbs(name: &str, verbs: Vec<&str>, trust_tier: TrustTier) -> ToolCandidate {
        ToolCandidate {
            verbs: verbs.into_iter().map(String::from).collect(),
            ..candidate(name, trust_tier)
        }
    }

    #[test]
    fn assemble_dedups_keeping_highest_trust() {
        // same tool from a community and a verified registry → one candidate, verified.
        let specs = vec![
            RegistrySpec::Static {
                name: "community-reg".into(),
                candidates: vec![candidate("x", TrustTier::Community)],
            },
            RegistrySpec::Static {
                name: "verified-reg".into(),
                candidates: vec![candidate("x", TrustTier::Verified)],
            },
        ];
        let (cat, warns) = assemble(&specs, &NoopIo);
        assert_eq!(cat.iter().filter(|c| c.name == "x").count(), 1);
        assert_eq!(
            cat.iter().find(|c| c.name == "x").unwrap().trust_tier,
            TrustTier::Verified
        );
        assert!(warns.is_empty());
    }

    #[test]
    fn evaluate_ranks_by_verb_overlap_then_trust() {
        // A verbs=[diagnose] community, B verbs=[diagnose,verify] org
        let a = candidate_with_verbs("A", vec!["diagnose"], TrustTier::Community);
        let b = candidate_with_verbs("B", vec!["diagnose", "verify"], TrustTier::Org);
        let cat = vec![a, b];
        let hits = evaluate(&cat, &["diagnose".into(), "verify".into()]);
        assert_eq!(hits[0].name, "B"); // 2 overlaps beats 1
    }

    #[test]
    fn cache_is_stale_after_ttl() {
        let c = Cache {
            fetched_at: 0,
            catalog: vec![],
        };
        assert!(c.is_stale(86_401, 86_400));
        assert!(!c.is_stale(100, 86_400));
    }

    #[test]
    fn a_failing_adapter_becomes_a_warning_not_an_abort() {
        let specs = vec![
            RegistrySpec::GithubOrg {
                name: "gh".into(),
                org: "org".into(),
            },
            RegistrySpec::Static {
                name: "static-ok".into(),
                candidates: vec![candidate("y", TrustTier::Community)],
            },
        ];
        let (cat, warns) = assemble(&specs, &ErroringGh);
        assert_eq!(warns.len(), 1);
        assert!(!cat.is_empty()); // the static one still landed
    }

    #[test]
    fn discover_ranks_name_hits_above_description_hits() {
        let cat = vec![candidate("browser-mcp", TrustTier::Community), {
            let mut c = candidate("other", TrustTier::Community);
            c.description = "a browser automation tool".into();
            c
        }];
        let hits = discover(&cat, "browser");
        assert_eq!(hits.len(), 2);
        assert_eq!(hits[0].name, "browser-mcp");
    }

    #[test]
    fn discover_empty_query_returns_full_catalog() {
        let cat = vec![candidate("a", TrustTier::Community)];
        assert_eq!(discover(&cat, "").len(), 1);
    }
}
