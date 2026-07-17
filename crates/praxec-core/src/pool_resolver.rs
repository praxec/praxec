//! Requirement-driven pool resolution (spec #2 §5).
//!
//! Given a behavior requirement (`affinity` / `tier` / `effort`) plus hard
//! constraints and the caller's value-ranking knobs (β/ε from a Stance), resolve
//! to a **pool** of `(provider, model, account)` members. Config-first,
//! catalog-fallback:
//!
//! 1. Walk `models.yaml` (`overrides`/`activity`/`default`) for the requirement's
//!    `affinity`/`tier` — the operator's explicit, **tier-aware** bindings.
//! 2. Only if that walk **exhausts** (no override and an empty default), fall
//!    back to the catalog value-ranking ([`pool_by_value`]) for the affinity.
//! 3. Filter the resulting models by hard constraints (`local_only`,
//!    `budget_cap`) and `effort` capability (the catalog's `reasoning_levels`,
//!    R4), then expand each surviving `(provider, model)` to
//!    `(provider, model, account)` members via the account registry, dropping
//!    unreachable accounts (R5).
//!
//! An empty pool is a hard [`ResolutionError::Unsatisfiable`] naming the facet —
//! **never** a silent default model (R1). The ranking never depends on hashmap
//! order (R8); accounts are expanded in sorted order.

use crate::accounts::AccountRegistry;
use crate::model_catalog::{ModelEntry, pool_by_value};
use crate::model_resolver::config::{Affinity, Effort, Provider};
use crate::model_resolver::walk::{ModelRef, Resolver};

/// A resolved pool member: which provider (slug), which provider-specific model
/// string, and which named account (`None` = the provider's single default key).
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct PoolMember {
    pub provider: String,
    pub model: String,
    pub account: Option<String>,
}

impl PoolMember {
    /// The runnable `provider:model` string.
    pub fn model_string(&self) -> String {
        format!("{}:{}", self.provider, self.model)
    }
}

/// Hard constraints — they **filter**, never rank (from the run's Priorities).
#[derive(Debug, Clone, Copy, Default)]
pub struct Constraints {
    /// Only models that run locally (no provider call).
    pub local_only: bool,
    /// Max blended `$`/M tokens; `None` = no cap.
    pub budget_cap: Option<f64>,
}

/// Terminal resolution failure — fail loud, never fall back to a default (R1).
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum ResolutionError {
    #[error("no reachable model satisfies requirement `{spec}`: {reason}")]
    Unsatisfiable { spec: String, reason: String },
}

/// Map an [`Effort`] facet to the reasoning-level token a model must advertise in
/// [`ModelEntry::reasoning_levels`] (rig's `ReasoningEffort` vocabulary).
fn effort_level(effort: Effort) -> &'static str {
    match effort {
        Effort::Fast => "low",
        Effort::Medium => "medium",
        Effort::Deep => "high",
    }
}

/// The provider slug (matches `ModelEntry::vendor` and account-registry keys).
fn provider_slug(p: &Provider) -> String {
    match p {
        Provider::Known(id) => id.slug().to_string(),
        // A custom OpenAI-shaped endpoint has no curated slug; key it by endpoint.
        Provider::Custom { endpoint } => endpoint.clone(),
    }
}

/// Blended cost (`$`/M) for the budget-cap check, using the same input weight the
/// value ranker uses.
fn blended_cost(m: &ModelEntry) -> f64 {
    let w = crate::tuning::tuning().blended_price_input_weight;
    m.input_usd_per_million * w + m.output_usd_per_million * (1.0 - w)
}

/// Does a `(provider, model)` pass the hard constraints + effort capability?
/// Missing catalog data is conservative — a constraint/effort we can't verify
/// **excludes** the model (R4), never assumes it satisfied.
fn passes(
    provider: &str,
    model: &str,
    effort: Option<Effort>,
    constraints: &Constraints,
    catalog: &[ModelEntry],
) -> bool {
    let entry = catalog
        .iter()
        .find(|m| m.vendor == provider && m.model == model);
    if constraints.local_only && !matches!(entry, Some(m) if m.local) {
        return false;
    }
    if let Some(cap) = constraints.budget_cap {
        match entry {
            Some(m) if blended_cost(m) <= cap => {}
            _ => return false, // unknown cost can't be shown under cap → exclude
        }
    }
    if let Some(e) = effort {
        let level = effort_level(e);
        // R4: the model must advertise the reasoning level. `reasoning_levels` is
        // the model-side declaration of the levels its provider can apply.
        match entry {
            Some(m) if m.reasoning_levels.iter().any(|l| l == level) => {}
            _ => return false,
        }
    }
    true
}

/// Resolve a behavior requirement to a pool of `(provider, model, account)`
/// members. See the module docs. `vendor_available(slug)` reports whether a
/// provider's default credential is reachable (for account-less providers).
#[allow(clippy::too_many_arguments)]
pub fn resolve_pool(
    spec: &ModelRef,
    constraints: &Constraints,
    resolver: &Resolver,
    catalog: &[ModelEntry],
    accounts: &AccountRegistry,
    vendor_available: impl Fn(&str) -> bool,
    price_sensitivity: f64,
    marginal_band: f64,
) -> Result<Vec<PoolMember>, ResolutionError> {
    // 1. Config-first (tier-aware); catalog-fallback only on config exhaustion.
    let base: Vec<(String, String)> = match resolver.walk(spec) {
        Ok((bindings, _label)) => bindings
            .iter()
            .map(|b| (provider_slug(&b.provider), b.model.clone()))
            .collect(),
        Err(_exhausted) => {
            let needs: Vec<Affinity> = spec.affinity.into_iter().collect();
            pool_by_value(
                catalog,
                &needs,
                &vendor_available,
                price_sensitivity,
                marginal_band,
            )
            .iter()
            .map(|m| (m.vendor.clone(), m.model.clone()))
            .collect()
        }
    };

    // 2. Filter by hard constraints + effort (catalog cross-ref), preserving order.
    let qualified: Vec<(String, String)> = base
        .into_iter()
        .filter(|(p, m)| passes(p, m, spec.effort, constraints, catalog))
        .collect();

    // 3. Expand to (provider, model, account) members, dropping unreachable ones.
    let mut members = Vec::new();
    for (provider, model) in &qualified {
        let mut accts: Vec<&str> = accounts
            .accounts_for_provider(provider)
            .into_iter()
            .collect();
        accts.sort_unstable(); // deterministic member order (R8)
        if accts.is_empty() {
            if vendor_available(provider) {
                members.push(PoolMember {
                    provider: provider.clone(),
                    model: model.clone(),
                    account: None,
                });
            }
        } else {
            for a in accts {
                if accounts.account_available(provider, a) {
                    members.push(PoolMember {
                        provider: provider.clone(),
                        model: model.clone(),
                        account: Some(a.to_string()),
                    });
                }
            }
        }
    }

    if members.is_empty() {
        return Err(ResolutionError::Unsatisfiable {
            spec: spec.to_string(),
            reason: unsatisfiable_reason(spec, constraints),
        });
    }
    Ok(members)
}

/// A human-actionable reason a pool came out empty (names the binding facets).
fn unsatisfiable_reason(spec: &ModelRef, c: &Constraints) -> String {
    let mut parts = Vec::new();
    if let Some(e) = spec.effort {
        parts.push(format!("effort `{e}`"));
    }
    if c.local_only {
        parts.push("local-only".to_string());
    }
    if let Some(cap) = c.budget_cap {
        parts.push(format!("budget ≤ ${cap}/M"));
    }
    if parts.is_empty() {
        "no reachable account for any qualifying model".to_string()
    } else {
        format!(
            "no reachable model meets the requested facets: {}",
            parts.join(", ")
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model_resolver::{AffinityScores, ConfigSource, ModelsFile};
    use std::path::PathBuf;

    fn resolver_from(yaml: &str) -> Resolver {
        Resolver::from_loaded(
            ModelsFile::from_yaml(yaml).expect("yaml parses"),
            ConfigSource::Project(PathBuf::from("/tmp/models.yaml")),
        )
    }

    fn entry(vendor: &str, model: &str, levels: &[&str], local: bool, coding: f64) -> ModelEntry {
        ModelEntry {
            vendor: vendor.into(),
            model: model.into(),
            input_usd_per_million: 1.0,
            output_usd_per_million: 1.0,
            context: 0,
            intelligence: 80.0,
            speed_tps: 50.0,
            tools: true,
            reasoning_levels: levels.iter().map(|s| s.to_string()).collect(),
            local,
            scores: AffinityScores {
                coding,
                ..Default::default()
            },
        }
    }

    // A models.yaml with an explicit coding-frontier override → config-first path.
    const YAML: &str = "\
version: 1
default:
  - provider: { name: openai }
    model: gpt-5
overrides:
  coding-frontier:
    - provider: { name: anthropic }
      model: claude-x
";

    #[test]
    fn config_first_expands_override_bindings_to_members() {
        let r = resolver_from(YAML);
        let cat = vec![entry("anthropic", "claude-x", &["high"], false, 90.0)];
        let acc = AccountRegistry::default();
        let spec = ModelRef::parse("coding-frontier").unwrap();
        let pool = resolve_pool(
            &spec,
            &Constraints::default(),
            &r,
            &cat,
            &acc,
            |v| v == "anthropic",
            0.5,
            0.15,
        )
        .expect("resolves");
        assert_eq!(pool.len(), 1);
        assert_eq!(pool[0].provider, "anthropic");
        assert_eq!(pool[0].model, "claude-x");
        assert_eq!(pool[0].account, None);
    }

    #[test]
    fn effort_filter_excludes_models_without_the_level_r4() {
        let r = resolver_from(YAML);
        // The override model advertises only `low` — a `deep`(=high) requirement
        // must exclude it → Unsatisfiable (R1), not a silent fallback.
        let cat = vec![entry("anthropic", "claude-x", &["low"], false, 90.0)];
        let acc = AccountRegistry::default();
        let spec = ModelRef::parse("coding-frontier-deep").unwrap();
        let err = resolve_pool(
            &spec,
            &Constraints::default(),
            &r,
            &cat,
            &acc,
            |_| true,
            0.5,
            0.15,
        )
        .unwrap_err();
        assert!(matches!(err, ResolutionError::Unsatisfiable { .. }));
    }

    #[test]
    fn unreachable_vendor_yields_unsatisfiable_r1() {
        let r = resolver_from(YAML);
        let cat = vec![entry("anthropic", "claude-x", &["high"], false, 90.0)];
        let acc = AccountRegistry::default();
        let spec = ModelRef::parse("coding-frontier").unwrap();
        // No reachable vendor → empty pool → Unsatisfiable, never a default.
        let err = resolve_pool(
            &spec,
            &Constraints::default(),
            &r,
            &cat,
            &acc,
            |_| false,
            0.5,
            0.15,
        )
        .unwrap_err();
        assert!(matches!(err, ResolutionError::Unsatisfiable { .. }));
    }
}
