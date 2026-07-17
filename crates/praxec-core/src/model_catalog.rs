//! The **model catalog** — the single source of truth for the models praxec
//! knows about and *what they're good at*. Data, sourced and dated (the "data in
//! config, not code" principle): provenance in the file header, refreshed by
//! editing data, never code. See `data/README_model_catalog.md`.
//!
//! This is the catalog both the cockpit's model picker and a `kind: llm` step's
//! `needs:` resolution rank over — capability is a property of the *model*, so it
//! lives in core, not in any one front-end. The suggestor ([`suggest_for_needs`])
//! ranks candidates by [`affinity_fit`](crate::model_resolver::affinity_fit)
//! against a step's needed affinities.

use crate::model_resolver::{Affinity, AffinityScores, affinity_fit};
use serde::{Deserialize, Serialize};

/// The canonical reachability check (keyless/local, or key present). Re-exported
/// so `suggest_for_needs` callers have it at hand.
pub use crate::providers::vendor_available;

/// One model the gateway knows about: how to run it (`vendor` + `model`), what it
/// costs and how fast it is, and **what it's good at** (`scores`).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ModelEntry {
    /// The SDK/provider slug (== `ProviderId`); with `model` forms the runnable
    /// `provider:model-id` string.
    pub vendor: String,
    pub model: String,
    #[serde(default)]
    pub input_usd_per_million: f64,
    #[serde(default)]
    pub output_usd_per_million: f64,
    #[serde(default)]
    pub context: usize,
    /// Overall capability index (Artificial-Analysis-style; sourced + dated).
    #[serde(default)]
    pub intelligence: f64,
    /// Output speed in tokens/sec (the "time" axis).
    #[serde(default)]
    pub speed_tps: f64,
    /// Supports tool/function calling — required for an agentic conductor.
    #[serde(default)]
    pub tools: bool,
    /// Reasoning-effort levels this model supports (rig's `ReasoningEffort`).
    #[serde(default)]
    pub reasoning_levels: Vec<String>,
    /// Runs locally (no provider call) — satisfies a privacy/local constraint.
    #[serde(default)]
    pub local: bool,
    /// What it's good at, per [`Affinity`]. Unscored affinities fall back to
    /// `intelligence` when ranking.
    #[serde(default)]
    pub scores: AffinityScores,
}

impl ModelEntry {
    /// The runnable `provider:model-id` string (what the provider factory parses).
    pub fn model_string(&self) -> String {
        format!("{}:{}", self.vendor, self.model)
    }

    /// This model's fit for a group of `needs` affinities (see [`affinity_fit`]).
    pub fn fit(&self, needs: &[Affinity]) -> f64 {
        affinity_fit(&self.scores, self.intelligence, needs)
    }
}

/// The catalog file: the model list plus its provenance.
#[derive(Debug, Clone, Deserialize)]
pub struct ModelCatalog {
    #[serde(default)]
    pub source: String,
    #[serde(default)]
    pub captured: String,
    pub models: Vec<ModelEntry>,
}

const DEFAULT_MODEL_CATALOG: &str = include_str!("../data/model_catalog.json");

/// The shipped default catalog with its provenance (ignores any override).
pub fn default_model_catalog() -> ModelCatalog {
    crate::catalog::load_default(DEFAULT_MODEL_CATALOG)
}

/// The active catalog (override-aware): `$PRAXEC_MODEL_CATALOG_FILE`, then
/// `~/.praxec/model_catalog.json`, then `./.praxec/…`, then the default.
pub fn model_catalog() -> ModelCatalog {
    crate::catalog::load_catalog(
        "PRAXEC_MODEL_CATALOG_FILE",
        "model_catalog.json",
        DEFAULT_MODEL_CATALOG,
    )
}

/// Compute the realized USD cost for a model run given prompt + completion
/// token counts, pricing it off the **active model catalog** (the same data the
/// suggestor ranks over — no dependency on the llm-executor's cost crate).
///
/// `model_string` is the runnable `"provider:model-id"` string (what an
/// [`AgentSession`](crate::model_catalog) carries). It is matched against each
/// entry's [`ModelEntry::model_string`]; failing that, against a bare
/// `vendor + model` join in case the caller passed only the model id.
///
/// Cost = `prompt/1e6 · input_usd_per_million + completion/1e6 ·
/// output_usd_per_million`.
///
/// Returns `None` when the model isn't catalogued — degrade gracefully, never
/// fail the run (mirrors the llm-executor leaving `cost_usd: None`). Zero tokens
/// against a known model yield `Some(0.0)`.
pub fn cost_usd(model_string: &str, prompt_tokens: u64, completion_tokens: u64) -> Option<f64> {
    cost_usd_in(
        &model_catalog().models,
        model_string,
        prompt_tokens,
        completion_tokens,
    )
}

/// [`cost_usd`] against an explicit model list — the testable core (lets unit
/// tests price against a fixture catalog without an env override).
pub fn cost_usd_in(
    models: &[ModelEntry],
    model_string: &str,
    prompt_tokens: u64,
    completion_tokens: u64,
) -> Option<f64> {
    let entry = models
        .iter()
        .find(|m| m.model_string() == model_string || m.model == model_string)?;
    let per_million = 1_000_000_f64;
    let input = (prompt_tokens as f64) * entry.input_usd_per_million / per_million;
    let output = (completion_tokens as f64) * entry.output_usd_per_million / per_million;
    Some(input + output)
}

/// **The model suggestor.** Among tool-capable, reachable (`available`) models,
/// return the best fit for the `needs` affinities — the highest
/// [`ModelEntry::fit`]. `None` if nothing is both runnable and tool-capable.
/// (Capability-only — the cockpit layers the user's cost/speed stance on top.)
pub fn suggest_for_needs<'a>(
    models: &'a [ModelEntry],
    needs: &[Affinity],
    available: impl Fn(&str) -> bool,
) -> Option<&'a ModelEntry> {
    models
        .iter()
        .filter(|m| m.tools && available(&m.vendor))
        .max_by(|a, b| {
            a.fit(needs)
                .partial_cmp(&b.fit(needs))
                .unwrap_or(std::cmp::Ordering::Equal)
        })
}

/// **The value-based model suggestor.** Encodes the operator's selection
/// philosophy: *"simplest model possible but not simpler; accuracy over
/// precision — for marginal differences prefer the stronger model; price-sensitive
/// **and** capability → best value for money, not always cheapest."*
///
/// Among tool-capable, reachable (`available`) models, return the best **value**
/// for the `needs` affinities:
///
/// - `value(m) = fit(m, needs) / blended_cost(m)^β`, where
///   `blended_cost(m) = input·w + output·(1 − w)` with `w =
///   tuning().blended_price_input_weight`, and `β = price_sensitivity`. A
///   non-positive blended cost (local/free) is floored to a tiny epsilon so it
///   ranks on capability rather than dividing by zero. `β → 0` ignores cost
///   (pick the most capable); larger `β` makes price weigh more.
/// - **Capability floor:** only candidates with `fit(needs) >=
///   tuning().sufficient_intelligence` are considered. If *none* clear the bar,
///   fall back to all candidates (never return nothing just because every model
///   is below the bar).
/// - **Marginal band → most capable:** let `best_value` be the maximum value
///   among the considered candidates; the in-band set is
///   `{ m : value(m) >= best_value · (1 − marginal_band) }`. Return the candidate
///   with the **highest `fit(needs)`** in that band — the most capable model
///   within the best-value band ("prefer stronger when value is close"). Ties
///   break to higher `intelligence`, then to the earlier (stable) entry.
///
/// `None` only if nothing is runnable and tool-capable.
pub fn suggest_by_value<'a>(
    models: &'a [ModelEntry],
    needs: &[Affinity],
    available: impl Fn(&str) -> bool,
    price_sensitivity: f64,
    marginal_band: f64,
) -> Option<&'a ModelEntry> {
    let t = crate::tuning::tuning();
    let w = t.blended_price_input_weight;
    let floor = t.sufficient_intelligence;

    // value(m) = fit / blended_cost^β, cost floored to epsilon for local/free.
    let value = |m: &ModelEntry| -> f64 { model_value(m, needs, w, price_sensitivity) };

    let candidates: Vec<&ModelEntry> = models
        .iter()
        .filter(|m| m.tools && available(&m.vendor))
        .collect();
    if candidates.is_empty() {
        return None;
    }

    // Capability floor: prefer those that clear the bar; if none do, keep all.
    let cleared: Vec<&ModelEntry> = candidates
        .iter()
        .copied()
        .filter(|m| m.fit(needs) >= floor)
        .collect();
    let considered = if cleared.is_empty() {
        &candidates
    } else {
        &cleared
    };

    let best_value = considered.iter().map(|m| value(m)).fold(f64::MIN, f64::max);
    let threshold = best_value * (1.0 - marginal_band);

    // Among the marginal band of best value, the most capable wins. Ties → higher
    // intelligence, then stable (max_by keeps the *last* max, so iterate so the
    // earlier entry wins on a tie).
    considered
        .iter()
        .copied()
        .filter(|m| value(m) >= threshold)
        .fold(None, |best: Option<&ModelEntry>, m| match best {
            None => Some(m),
            Some(b) => {
                let key_m = (m.fit(needs), m.intelligence);
                let key_b = (b.fit(needs), b.intelligence);
                if key_m.partial_cmp(&key_b) == Some(std::cmp::Ordering::Greater) {
                    Some(m)
                } else {
                    Some(b)
                }
            }
        })
}

/// `value(m) = fit(m, needs) / blended_cost(m)^β` — the shared value formula used
/// by both [`suggest_by_value`] and [`pool_by_value`]. A non-positive blended cost
/// (local/free) is floored to a tiny epsilon so it ranks on capability.
fn model_value(
    m: &ModelEntry,
    needs: &[Affinity],
    input_weight: f64,
    price_sensitivity: f64,
) -> f64 {
    let fit = m.fit(needs);
    let blended =
        m.input_usd_per_million * input_weight + m.output_usd_per_million * (1.0 - input_weight);
    let cost = if blended > 0.0 { blended } else { 1e-9 };
    fit / cost.powf(price_sensitivity)
}

/// The ranked **value band** for a pool: the same value / capability-floor /
/// marginal-band logic as [`suggest_by_value`], but returns the whole in-band set
/// — the members a `distribute` strategy may balance across (R2). Ordered
/// most-capable-first (so the head equals `suggest_by_value`'s pick and `ordered`
/// failover is unchanged), with a total-order deterministic tie-break (R8):
/// `(fit desc, intelligence desc, value desc, vendor asc, model asc)`.
pub fn pool_by_value<'a>(
    models: &'a [ModelEntry],
    needs: &[Affinity],
    available: impl Fn(&str) -> bool,
    price_sensitivity: f64,
    marginal_band: f64,
) -> Vec<&'a ModelEntry> {
    let t = crate::tuning::tuning();
    let w = t.blended_price_input_weight;
    let floor = t.sufficient_intelligence;

    let candidates: Vec<&ModelEntry> = models
        .iter()
        .filter(|m| m.tools && available(&m.vendor))
        .collect();
    if candidates.is_empty() {
        return Vec::new();
    }
    let cleared: Vec<&ModelEntry> = candidates
        .iter()
        .copied()
        .filter(|m| m.fit(needs) >= floor)
        .collect();
    let considered = if cleared.is_empty() {
        &candidates
    } else {
        &cleared
    };

    let best_value = considered
        .iter()
        .map(|m| model_value(m, needs, w, price_sensitivity))
        .fold(f64::MIN, f64::max);
    let threshold = best_value * (1.0 - marginal_band);

    let mut band: Vec<&ModelEntry> = considered
        .iter()
        .copied()
        .filter(|m| model_value(m, needs, w, price_sensitivity) >= threshold)
        .collect();
    band.sort_by(|a, b| {
        b.fit(needs)
            .total_cmp(&a.fit(needs))
            .then(b.intelligence.total_cmp(&a.intelligence))
            .then(
                model_value(b, needs, w, price_sensitivity).total_cmp(&model_value(
                    a,
                    needs,
                    w,
                    price_sensitivity,
                )),
            )
            .then(a.vendor.cmp(&b.vendor))
            .then(a.model.cmp(&b.model))
    });
    band
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn shipped_catalog_parses_with_provenance() {
        let cat = default_model_catalog();
        assert!(!cat.models.is_empty());
        assert!(!cat.source.is_empty());
        assert!(cat.captured.starts_with("2026-"));
        for m in &cat.models {
            assert!(m.intelligence > 0.0, "{} unscored", m.model);
        }
    }

    #[test]
    fn model_string_is_provider_colon_model() {
        let cat = default_model_catalog();
        let or = cat
            .models
            .iter()
            .find(|m| m.vendor == "openrouter")
            .unwrap();
        let s = or.model_string();
        assert!(s.starts_with("openrouter:"));
        assert!(s.split_once(':').is_some());
    }

    fn entry(
        vendor: &str,
        model: &str,
        intelligence: f64,
        coding: f64,
        agentic: f64,
    ) -> ModelEntry {
        ModelEntry {
            vendor: vendor.into(),
            model: model.into(),
            input_usd_per_million: 1.0,
            output_usd_per_million: 1.0,
            context: 0,
            intelligence,
            speed_tps: 50.0,
            tools: true,
            reasoning_levels: vec![],
            local: false,
            scores: AffinityScores {
                coding,
                agentic,
                ..Default::default()
            },
        }
    }

    #[test]
    fn suggest_routes_to_the_best_fit_factoring_overall_and_domain() {
        // A frontier generalist vs a coding specialist (lower overall, higher coding).
        let models = vec![
            entry("a", "frontier", 60.0, 60.0, 60.0),
            entry("b", "coder", 50.0, 75.0, 45.0),
        ];
        // Coding: 0.5*75+0.5*50=62.5 (coder) > 0.5*60+0.5*60=60 (frontier) → coder.
        let c = suggest_for_needs(&models, &[Affinity::Coding], |_| true).unwrap();
        assert_eq!(c.model, "coder");
        // Agentic: frontier's overall + domain both higher → frontier.
        let a = suggest_for_needs(&models, &[Affinity::Agentic], |_| true).unwrap();
        assert_eq!(a.model, "frontier");
        // No needs → pure overall intelligence → frontier.
        let n = suggest_for_needs(&models, &[], |_| true).unwrap();
        assert_eq!(n.model, "frontier");
    }

    #[test]
    fn suggest_respects_availability_and_tool_calling() {
        let mut no_tools = entry("a", "smart-no-tools", 99.0, 99.0, 99.0);
        no_tools.tools = false;
        let models = vec![no_tools, entry("b", "tools-ok", 50.0, 50.0, 50.0)];
        // The smarter model can't call tools → never chosen.
        let pick = suggest_for_needs(&models, &[Affinity::Coding], |_| true).unwrap();
        assert_eq!(pick.model, "tools-ok");
        // Unreachable vendor filtered out → no candidate.
        assert!(suggest_for_needs(&models, &[], |v| v == "nope").is_none());
    }

    /// A model with explicit prices + a single (coding) score, so the value
    /// arithmetic is checkable by hand. `intelligence` doubles as the overall.
    fn priced(model: &str, intelligence: f64, coding: f64, input: f64, output: f64) -> ModelEntry {
        ModelEntry {
            vendor: "v".into(),
            model: model.into(),
            input_usd_per_million: input,
            output_usd_per_million: output,
            context: 0,
            intelligence,
            speed_tps: 50.0,
            tools: true,
            reasoning_levels: vec![],
            local: false,
            scores: AffinityScores {
                coding,
                ..Default::default()
            },
        }
    }

    // Default tuning knobs the way the spec defaults them, so the philosophy
    // tests read against the shipped behaviour.
    const BETA: f64 = 0.5; // price_sensitivity
    const BAND: f64 = 0.15; // value_marginal_band

    #[test]
    fn value_picks_cheaper_when_capability_equal() {
        // Same capability; one is 4× cheaper → value is 2× higher (sqrt cost).
        // Both fit == 70 (>= floor 52). The cheaper one wins outright.
        let models = vec![
            priced("pricey", 70.0, 70.0, 4.0, 4.0),
            priced("cheap", 70.0, 70.0, 1.0, 1.0),
        ];
        let pick = suggest_by_value(&models, &[Affinity::Coding], |_| true, BETA, BAND).unwrap();
        assert_eq!(pick.model, "cheap");
    }

    #[test]
    fn pool_by_value_returns_the_band_head_matches_suggest() {
        // Two models within the marginal band → the pool contains both; the head
        // is the most-capable (== suggest_by_value's single pick); order is stable.
        let models = vec![
            priced("cheaper-weaker", 60.0, 60.0, 1.0, 1.0),
            priced("stronger", 64.0, 64.0, 1.15, 1.15),
        ];
        let pool = pool_by_value(&models, &[Affinity::Coding], |_| true, BETA, BAND);
        assert_eq!(pool.len(), 2, "both within the value band");
        let pick = suggest_by_value(&models, &[Affinity::Coding], |_| true, BETA, BAND).unwrap();
        assert_eq!(
            pool[0].model, pick.model,
            "pool head == suggest_by_value pick"
        );
        assert_eq!(pool[0].model, "stronger");
        let order2: Vec<_> = pool_by_value(&models, &[Affinity::Coding], |_| true, BETA, BAND)
            .iter()
            .map(|m| m.model.clone())
            .collect();
        let order1: Vec<_> = pool.iter().map(|m| m.model.clone()).collect();
        assert_eq!(order1, order2, "deterministic order (R8)");
    }

    #[test]
    fn pool_by_value_excludes_below_floor_and_unavailable() {
        let models = vec![
            priced("best", 80.0, 80.0, 1.0, 1.0),
            priced("far-worse", 30.0, 30.0, 8.0, 8.0), // fit 30 < floor
        ];
        let pool = pool_by_value(&models, &[Affinity::Coding], |_| true, BETA, BAND);
        assert!(
            pool.iter().all(|m| m.model != "far-worse"),
            "below-floor model excluded"
        );
        assert!(
            pool_by_value(&models, &[Affinity::Coding], |_| false, BETA, BAND).is_empty(),
            "no reachable vendor → empty pool"
        );
    }

    #[test]
    fn value_prefers_stronger_when_value_is_marginal() {
        // A: slightly cheaper + slightly weaker; B: stronger but a touch pricier.
        // fit_A = 60, cost_A = 1.0 → value_A = 60.
        // fit_B = 64, cost_B = 1.15 → value_B = 64/sqrt(1.15) ≈ 59.7.
        // value_B is within 15% of value_A (the best), so the band includes both
        // → the *stronger* model B wins (accuracy over precision).
        let models = vec![
            priced("cheaper-weaker", 60.0, 60.0, 1.0, 1.0),
            priced("stronger", 64.0, 64.0, 1.15, 1.15),
        ];
        let pick = suggest_by_value(&models, &[Affinity::Coding], |_| true, BETA, BAND).unwrap();
        assert_eq!(pick.model, "stronger");
    }

    #[test]
    fn value_takes_big_capability_gain_for_small_cost() {
        // Much stronger (90 vs 55) for only a little more money → stronger wins.
        // value_weak = 55/sqrt(1) = 55; value_strong = 90/sqrt(1.21) ≈ 81.8.
        // Strong dominates on value outright; no band tie needed.
        let models = vec![
            priced("weak", 55.0, 55.0, 1.0, 1.0),
            priced("strong", 90.0, 90.0, 1.21, 1.21),
        ];
        let pick = suggest_by_value(&models, &[Affinity::Coding], |_| true, BETA, BAND).unwrap();
        assert_eq!(pick.model, "strong");
    }

    #[test]
    fn value_rejects_false_economy_below_floor() {
        // A dirt-cheap model below sufficient_intelligence (52) would have huge
        // raw value, but the capability floor excludes it; the capable model
        // above the floor wins despite costing more.
        let models = vec![
            priced("dirt-cheap-dumb", 30.0, 30.0, 0.05, 0.05),
            priced("capable", 60.0, 60.0, 2.0, 2.0),
        ];
        let pick = suggest_by_value(&models, &[Affinity::Coding], |_| true, BETA, BAND).unwrap();
        assert_eq!(pick.model, "capable");
    }

    #[test]
    fn price_sensitivity_zero_picks_most_capable() {
        // β = 0 → cost^0 = 1 for all → value == fit → most capable wins regardless
        // of how cheap the alternative is.
        let models = vec![
            priced("cheap-weaker", 55.0, 55.0, 0.01, 0.01),
            priced("expensive-stronger", 60.0, 60.0, 50.0, 50.0),
        ];
        let pick = suggest_by_value(&models, &[Affinity::Coding], |_| true, 0.0, BAND).unwrap();
        assert_eq!(pick.model, "expensive-stronger");
    }

    #[test]
    fn high_price_sensitivity_picks_cheaper() {
        // Large β makes cost dominate: a cheap-but-adequate model (above the
        // floor) beats a marginally stronger expensive one.
        // β = 3: value_cheap = 55/1^3 = 55; value_exp = 60/3^3 ≈ 2.2 → cheap wins.
        let models = vec![
            priced("cheap-adequate", 55.0, 55.0, 1.0, 1.0),
            priced("pricey-stronger", 60.0, 60.0, 3.0, 3.0),
        ];
        let pick = suggest_by_value(&models, &[Affinity::Coding], |_| true, 3.0, BAND).unwrap();
        assert_eq!(pick.model, "cheap-adequate");
    }

    #[test]
    fn value_respects_availability_and_tool_calling() {
        let mut no_tools = priced("smart-no-tools", 90.0, 90.0, 1.0, 1.0);
        no_tools.tools = false;
        let mut other_vendor = priced("reachable", 60.0, 60.0, 1.0, 1.0);
        other_vendor.vendor = "ok".into();
        let models = vec![no_tools, other_vendor];
        // The smarter model can't call tools → never chosen.
        let pick = suggest_by_value(&models, &[Affinity::Coding], |_| true, BETA, BAND).unwrap();
        assert_eq!(pick.model, "reachable");
        // Unreachable vendor filtered out → no candidate.
        assert!(
            suggest_by_value(&models, &[Affinity::Coding], |v| v == "nope", BETA, BAND).is_none()
        );
    }

    #[test]
    fn cost_usd_prices_known_model_off_catalog() {
        // glm-5.2-shaped entry: 0.98 in / 3.08 out per million.
        let models = vec![ModelEntry {
            vendor: "openrouter".into(),
            model: "z-ai/glm-5.2".into(),
            input_usd_per_million: 0.98,
            output_usd_per_million: 3.08,
            context: 0,
            intelligence: 56.0,
            speed_tps: 80.0,
            tools: true,
            reasoning_levels: vec![],
            local: false,
            scores: AffinityScores::default(),
        }];
        // 1_000_000 prompt → 0.98; 1_000_000 completion → 3.08; sum 4.06.
        let c = cost_usd_in(&models, "openrouter:z-ai/glm-5.2", 1_000_000, 1_000_000).unwrap();
        assert!((c - 4.06).abs() < 1e-9, "expected 4.06, got {c}");
        // Mixed realistic counts: 250k in, 40k out.
        let c2 = cost_usd_in(&models, "openrouter:z-ai/glm-5.2", 250_000, 40_000).unwrap();
        let expected = 250_000.0 * 0.98 / 1e6 + 40_000.0 * 3.08 / 1e6;
        assert!(
            (c2 - expected).abs() < 1e-9,
            "expected {expected}, got {c2}"
        );
    }

    #[test]
    fn cost_usd_zero_tokens_is_zero_for_known_model() {
        let models = vec![priced("m", 60.0, 60.0, 1.0, 1.0)];
        let c = cost_usd_in(&models, "v:m", 0, 0).expect("known model must price");
        assert!(c.abs() < f64::EPSILON);
    }

    #[test]
    fn cost_usd_uncatalogued_model_is_none() {
        let models = vec![priced("m", 60.0, 60.0, 1.0, 1.0)];
        assert!(cost_usd_in(&models, "v:unknown", 100, 100).is_none());
    }

    #[test]
    fn value_falls_back_when_none_clear_the_floor() {
        // Both below the floor (52) → don't return nothing; rank all by value.
        // value_a = 40/sqrt(2) ≈ 28.3; value_b = 45/sqrt(4) = 22.5 → a wins.
        let models = vec![
            priced("a", 40.0, 40.0, 2.0, 2.0),
            priced("b", 45.0, 45.0, 4.0, 4.0),
        ];
        let pick = suggest_by_value(&models, &[Affinity::Coding], |_| true, BETA, BAND).unwrap();
        assert_eq!(pick.model, "a");
    }
}
