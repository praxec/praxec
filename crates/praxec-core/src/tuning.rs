//! **Recommendation tuning** — the knobs that shape model recommendation and
//! cost estimation, as *configuration* rather than constants compiled into the
//! source. Users tune the recommendation to their judgement (how much task
//! strength weighs vs general capability, the reliability bar, the cost-estimate
//! assumptions, reasoning-cost multipliers) without rebuilding.
//!
//! Loaded once via the shared `core::catalog` override precedence:
//! `$PRAXEC_TUNING_FILE` → `~/.praxec/tuning.json` → `./.praxec/…` →
//! the shipped default (`data/tuning.json`). Every field has a default, so an
//! override file may set only the knobs it cares about.

use std::collections::BTreeMap;
use std::sync::LazyLock;

use serde::Deserialize;

#[derive(Debug, Clone, Deserialize)]
pub struct RecommendationTuning {
    /// In `affinity_fit`, how much the needed-affinity scores weigh vs overall
    /// intelligence (the rest). 0.5 = half each; raise it to favour task
    /// specialists, lower it to favour generally-capable models.
    #[serde(default = "default_affinity_weight")]
    pub affinity_weight: f64,

    /// The capability bar below which a model isn't reliable enough for the job;
    /// most stances prefer to stay at or above it.
    #[serde(default = "default_sufficient_intelligence")]
    pub sufficient_intelligence: f64,

    /// The requests/day assumed when evaluating a budget ceiling before the user
    /// dials in their own.
    #[serde(default = "default_default_requests_per_day")]
    pub default_requests_per_day: usize,

    /// The requests/day presets the gate cycles through (←→) to show where cost
    /// lands.
    #[serde(default = "default_requests_per_day_levels")]
    pub requests_per_day_levels: Vec<usize>,

    /// Assumed input tokens per request (prompt: system + transcript + schemas).
    #[serde(default = "default_cost_input_tokens")]
    pub cost_input_tokens_per_request: f64,

    /// Assumed output tokens per request (a short action, before reasoning).
    #[serde(default = "default_cost_output_tokens")]
    pub cost_output_tokens_per_request: f64,

    /// Weight on the input price in the blended (output-weighted) cost-ranking
    /// price; the output weight is `1 - this`.
    #[serde(default = "default_blended_price_input_weight")]
    pub blended_price_input_weight: f64,

    /// Cost exponent `β` in value-based selection: `value = capability /
    /// blended_cost^β`. `β → 0` ignores cost (pick the most capable); larger
    /// `β` makes price weigh more. Default `0.5` (sqrt cost) so cost matters but
    /// capability isn't easily overwhelmed — "accuracy over precision".
    #[serde(default = "default_price_sensitivity")]
    pub price_sensitivity: f64,

    /// The relative value band `ε` that counts as "marginal": candidates whose
    /// value is within `ε` of the best value are treated as equivalent, and the
    /// *most capable* among them is chosen. Default `0.15` — within 15% of best
    /// value, take the stronger model.
    #[serde(default = "default_value_marginal_band")]
    pub value_marginal_band: f64,

    /// Output-token multiplier per reasoning-effort level — more thinking emits
    /// more billed reasoning tokens. An unlisted level falls back to `medium`.
    #[serde(default = "default_reasoning_multipliers")]
    pub reasoning_multipliers: BTreeMap<String, f64>,

    /// The USD/day boundaries between cost-magnitude buckets (Pennies < first,
    /// TensOfCents < second, …). Below the first is "pennies", at/above the last
    /// is "tens of thousands or more".
    #[serde(default = "default_cost_magnitude_thresholds")]
    pub cost_magnitude_thresholds_usd_per_day: Vec<f64>,

    /// Per-provider reasoning-effort wiring (the values sent in each provider's
    /// native `additional_params` shape).
    #[serde(default = "default_reasoning_tuning")]
    pub reasoning: ReasoningTuning,

    /// Slow-loop de-escalation thresholds — when the governed base-model loop
    /// proposes lowering (to save money) or raising (when chronically failing) a
    /// step's base. Conservative by design (see [`DeescalationTuning`]).
    #[serde(default = "default_deescalation_tuning")]
    pub deescalation: DeescalationTuning,

    /// Intent-index thresholds — the loop that learns which process (template)
    /// wins for which task-class from mission outcomes (see [`IntentTuning`]).
    #[serde(default = "default_intent_tuning")]
    pub intent: IntentTuning,
}

/// Thresholds for the governed bidirectional base-model loop
/// (`crate::deescalation`). Data, not code — tune without rebuilding.
#[derive(Debug, Clone, Deserialize)]
pub struct DeescalationTuning {
    /// Minimum runs for a `(affinity, model)` before it can drive a decision —
    /// don't move a base on a thin sample.
    #[serde(default = "default_deescalation_min_runs")]
    pub min_runs: usize,
    /// Pass-rate a candidate must clear to count as "clearing the bar" (used
    /// both to lower onto a cheaper model and to raise onto a reliable one).
    #[serde(default = "default_lower_min_pass_rate")]
    pub lower_min_pass_rate: f64,
    /// Below this base pass-rate the base is "chronically failing" → propose a
    /// raise.
    #[serde(default = "default_raise_max_pass_rate")]
    pub raise_max_pass_rate: f64,
    /// Minimum fractional cost saving to justify lowering — the conservatism
    /// guard: a marginal saving keeps the stronger model.
    #[serde(default = "default_material_savings_pct")]
    pub material_savings_pct: f64,
}

/// Thresholds for the intent-index loop (`crate::intent_index`). Data, not code:
/// tune the evidence bar without rebuilding. Conservative like the de-escalation
/// loop — don't let a thin sample drive a process choice.
#[derive(Debug, Clone, Deserialize)]
pub struct IntentTuning {
    /// Minimum *evidence* runs (terminated missions with ≥1 declared outcome) for
    /// a `(task_class, template)` before its success-rate is trusted to drive a
    /// selection. Mirrors `DeescalationTuning::min_runs`.
    #[serde(default = "default_intent_min_runs")]
    pub min_runs: usize,
}

/// How our reasoning-effort levels map to each provider's native knob — values
/// only; the JSON *shape* is fixed by each provider's API.
#[derive(Debug, Clone, Deserialize)]
pub struct ReasoningTuning {
    /// Anthropic extended-thinking token budget per level (`0` → thinking off).
    #[serde(default = "default_anthropic_budgets")]
    pub anthropic_budget_tokens: BTreeMap<String, u64>,
    /// OpenAI / OpenRouter `reasoning.effort` value per level.
    #[serde(default = "default_openai_effort")]
    pub openai_effort: BTreeMap<String, String>,
    /// Gemini `thinking_level` value per level.
    #[serde(default = "default_gemini_level")]
    pub gemini_level: BTreeMap<String, String>,
    /// Reasoning effort applied to a `kind: agent` turn when the step declares
    /// no explicit `reasoning_effort`. `low` caps per-turn reasoning so a
    /// *reasoning* model can lead a chain without spending the whole turn budget
    /// on hidden reasoning (which surfaces as empty content → an AGENT_NO_RESULT
    /// stall). Empty string opts out (send nothing → provider default). Note
    /// `medium` is itself a no-op here (≡ provider default — see
    /// `reasoning_params`), so it is NOT a useful cap.
    #[serde(default = "default_reasoning_default_effort")]
    pub default_effort: String,
}

impl ReasoningTuning {
    /// Anthropic thinking budget for `level` (falls back to `medium`, then 8192).
    pub fn anthropic_budget(&self, level: &str) -> u64 {
        self.anthropic_budget_tokens
            .get(level)
            .copied()
            .unwrap_or_else(|| {
                self.anthropic_budget_tokens
                    .get("medium")
                    .copied()
                    .unwrap_or(8_192)
            })
    }
    /// OpenAI/OpenRouter effort for `level` (falls back to `medium`).
    pub fn openai_effort(&self, level: &str) -> &str {
        self.openai_effort
            .get(level)
            .map(String::as_str)
            .unwrap_or("medium")
    }
    /// Gemini thinking level for `level` (falls back to `high`).
    pub fn gemini_level(&self, level: &str) -> &str {
        self.gemini_level
            .get(level)
            .map(String::as_str)
            .unwrap_or("high")
    }
}

fn default_affinity_weight() -> f64 {
    0.5
}
fn default_sufficient_intelligence() -> f64 {
    52.0
}
fn default_default_requests_per_day() -> usize {
    1_000
}
fn default_requests_per_day_levels() -> Vec<usize> {
    vec![100, 1_000, 10_000, 100_000]
}
fn default_cost_input_tokens() -> f64 {
    5_000.0
}
fn default_cost_output_tokens() -> f64 {
    600.0
}
fn default_blended_price_input_weight() -> f64 {
    0.3
}
fn default_price_sensitivity() -> f64 {
    0.5
}
fn default_value_marginal_band() -> f64 {
    0.15
}
fn default_reasoning_multipliers() -> BTreeMap<String, f64> {
    [
        ("none", 1.0),
        ("minimal", 1.3),
        ("low", 2.0),
        ("medium", 4.0),
        ("high", 8.0),
        ("xhigh", 15.0),
        ("max", 15.0),
    ]
    .into_iter()
    .map(|(k, v)| (k.to_string(), v))
    .collect()
}
fn default_cost_magnitude_thresholds() -> Vec<f64> {
    vec![0.10, 1.0, 10.0, 100.0, 1_000.0, 10_000.0]
}
fn default_deescalation_min_runs() -> usize {
    5
}
fn default_lower_min_pass_rate() -> f64 {
    0.9
}
fn default_raise_max_pass_rate() -> f64 {
    0.6
}
fn default_material_savings_pct() -> f64 {
    0.25
}
fn default_deescalation_tuning() -> DeescalationTuning {
    DeescalationTuning {
        min_runs: default_deescalation_min_runs(),
        lower_min_pass_rate: default_lower_min_pass_rate(),
        raise_max_pass_rate: default_raise_max_pass_rate(),
        material_savings_pct: default_material_savings_pct(),
    }
}
fn default_intent_min_runs() -> usize {
    5
}
fn default_intent_tuning() -> IntentTuning {
    IntentTuning {
        min_runs: default_intent_min_runs(),
    }
}
fn default_reasoning_tuning() -> ReasoningTuning {
    ReasoningTuning {
        anthropic_budget_tokens: default_anthropic_budgets(),
        openai_effort: default_openai_effort(),
        gemini_level: default_gemini_level(),
        default_effort: default_reasoning_default_effort(),
    }
}
/// Cap per-turn reasoning by default so reasoning models can lead a chain
/// without burning the whole turn budget. Override-aware via the tuning file.
fn default_reasoning_default_effort() -> String {
    "low".to_string()
}
fn map_str(pairs: &[(&str, &str)]) -> BTreeMap<String, String> {
    pairs
        .iter()
        .map(|(k, v)| (k.to_string(), v.to_string()))
        .collect()
}
fn default_anthropic_budgets() -> BTreeMap<String, u64> {
    [
        ("none", 0),
        ("minimal", 2_048),
        ("low", 2_048),
        ("medium", 8_192),
        ("high", 16_384),
        ("xhigh", 32_768),
        ("max", 32_768),
    ]
    .into_iter()
    .map(|(k, v)| (k.to_string(), v))
    .collect()
}
fn default_openai_effort() -> BTreeMap<String, String> {
    map_str(&[
        ("none", "none"),
        ("minimal", "minimal"),
        ("low", "low"),
        ("medium", "medium"),
        ("high", "high"),
        ("xhigh", "xhigh"),
        ("max", "xhigh"),
    ])
}
fn default_gemini_level() -> BTreeMap<String, String> {
    map_str(&[
        ("none", "none"),
        ("minimal", "low"),
        ("low", "low"),
        ("medium", "high"),
        ("high", "high"),
        ("xhigh", "high"),
        ("max", "high"),
    ])
}

const DEFAULT_TUNING: &str = include_str!("../data/tuning.json");

static TUNING: LazyLock<RecommendationTuning> = LazyLock::new(|| {
    crate::catalog::load_catalog("PRAXEC_TUNING_FILE", "tuning.json", DEFAULT_TUNING)
});

/// The active recommendation tuning (override-aware; loaded once).
pub fn tuning() -> &'static RecommendationTuning {
    &TUNING
}

/// Per-provider reasoning-effort `additional_params` for a rig call — each
/// provider's **native shape**, with the configured values (`tuning.reasoning`).
/// `medium`/empty → `None` (the provider default; never send a rejected param).
/// Shared by the cockpit chat loop and the `kind: llm` executor.
pub fn reasoning_params(vendor: &str, level: &str) -> Option<serde_json::Value> {
    use serde_json::json;
    let level = level.trim().to_lowercase();
    if level.is_empty() || level == "medium" {
        return None;
    }
    let r = &tuning().reasoning;
    match vendor {
        "openai" | "openrouter" => {
            Some(json!({ "reasoning": { "effort": r.openai_effort(&level) } }))
        }
        "anthropic" => Some(match r.anthropic_budget(&level) {
            0 => json!({ "thinking": { "type": "disabled" } }),
            budget => json!({ "thinking": { "type": "enabled", "budget_tokens": budget } }),
        }),
        "gemini" => Some(json!({ "thinking_level": r.gemini_level(&level) })),
        _ => None,
    }
}

/// The output-token multiplier for `level`, from the configured map; an unknown
/// level falls back to `medium` (rig's default effort), or 4.0 if absent.
pub fn reasoning_multiplier(level: &str) -> f64 {
    let t = tuning();
    t.reasoning_multipliers
        .get(level)
        .copied()
        .unwrap_or_else(|| {
            t.reasoning_multipliers
                .get("medium")
                .copied()
                .unwrap_or(4.0)
        })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn shipped_defaults_load() {
        let t = tuning();
        assert_eq!(t.affinity_weight, 0.5);
        assert_eq!(t.sufficient_intelligence, 52.0);
        assert_eq!(t.price_sensitivity, 0.5);
        assert_eq!(t.value_marginal_band, 0.15);
        assert_eq!(t.requests_per_day_levels, vec![100, 1_000, 10_000, 100_000]);
        assert_eq!(t.deescalation.min_runs, 5);
        assert_eq!(t.intent.min_runs, 5);
        assert_eq!(reasoning_multiplier("high"), 8.0);
        // Unknown level → medium fallback.
        assert_eq!(reasoning_multiplier("wat"), 4.0);
    }

    #[test]
    fn a_partial_override_keeps_other_defaults() {
        // Only affinity_weight set → the rest fall back to defaults.
        let t: RecommendationTuning =
            serde_json::from_str(r#"{ "affinity_weight": 0.8 }"#).unwrap();
        assert_eq!(t.affinity_weight, 0.8);
        assert_eq!(t.sufficient_intelligence, 52.0);
        // The new value-selection knobs default in too.
        assert_eq!(t.price_sensitivity, 0.5);
        assert_eq!(t.value_marginal_band, 0.15);
        assert_eq!(t.default_requests_per_day, 1_000);
        // The reasoning + cost-bucket knobs also default in.
        assert_eq!(
            t.cost_magnitude_thresholds_usd_per_day,
            vec![0.10, 1.0, 10.0, 100.0, 1_000.0, 10_000.0]
        );
        assert_eq!(t.reasoning.anthropic_budget("high"), 16_384);
        // The de-escalation + intent loops default in too.
        assert_eq!(t.deescalation.min_runs, 5);
        assert_eq!(t.intent.min_runs, 5);
    }

    #[test]
    fn reasoning_accessors_map_levels_with_fallbacks() {
        let r = default_reasoning_tuning();
        assert_eq!(r.anthropic_budget("none"), 0); // disabled
        assert_eq!(r.anthropic_budget("xhigh"), 32_768);
        assert_eq!(r.anthropic_budget("mystery"), 8_192); // → medium fallback
        assert_eq!(r.openai_effort("max"), "xhigh");
        assert_eq!(r.gemini_level("low"), "low");
        assert_eq!(r.gemini_level("mystery"), "high"); // fallback
    }

    #[test]
    fn default_reasoning_effort_is_low_and_caps_the_turn() {
        // The shipped default caps per-turn reasoning so a reasoning model can
        // lead a chain; "low" (not "medium") because medium is a no-op.
        let r = default_reasoning_tuning();
        assert_eq!(r.default_effort, "low");
        // And "low" actually emits a capping param for openrouter/openai
        // (whereas "medium" would send nothing).
        assert!(reasoning_params("openrouter", &r.default_effort).is_some());
        assert!(reasoning_params("openrouter", "medium").is_none());
    }

    #[test]
    fn default_effort_is_override_aware() {
        // Setting only default_effort keeps the other reasoning maps at defaults.
        let t: RecommendationTuning =
            serde_json::from_str(r#"{ "reasoning": { "default_effort": "minimal" } }"#).unwrap();
        assert_eq!(t.reasoning.default_effort, "minimal");
        assert_eq!(t.reasoning.openai_effort("high"), "high"); // untouched
        // Empty opts out (provider default).
        let t2: RecommendationTuning =
            serde_json::from_str(r#"{ "reasoning": { "default_effort": "" } }"#).unwrap();
        assert_eq!(t2.reasoning.default_effort, "");
    }
}
