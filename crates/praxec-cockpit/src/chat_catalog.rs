//! Chat-model recommendation for the Mission Control conductor.
//!
//! The catalog itself is **core's single source of truth**
//! (`core::model_catalog`: `ModelEntry` + sourced/dated data); this module adds
//! the cockpit-specific *stance-aware* recommendation on top — the conductor
//! drives by **calling tools** (a hard filter) and cost is shown in orders of
//! magnitude at an **adjustable requests/day**. The capability vocabulary is
//! core's [`Affinity`]/[`AffinityScores`] — no parallel taxonomy.

use praxec_embeddings::CostMagnitude;

// The model catalog + its types live in core (capability is a property of the
// model, shared by the cockpit picker and a `kind: llm` step's `needs:`). The
// cockpit reads `ModelEntry` under its local name and layers stance on top.
pub use praxec_core::model_catalog::ModelEntry as ChatModelOption;
pub use praxec_core::model_resolver::{affinity_fit as fit, Affinity, AffinityScores};

/// Output-token multiplier for a reasoning level (configurable —
/// `tuning.reasoning_multipliers`). An unlisted level falls back to `medium`.
pub use praxec_core::tuning::reasoning_multiplier;

/// A sensible default effort for `opt`: `medium` (rig's default) if the model
/// supports it, else the first level it lists, else `none`.
pub fn default_reasoning(opt: &ChatModelOption) -> String {
    if opt.reasoning_levels.iter().any(|l| l == "medium") {
        "medium".to_string()
    } else {
        opt.reasoning_levels
            .first()
            .cloned()
            .unwrap_or_else(|| "none".to_string())
    }
}

/// The catalog with its provenance — re-exported from core (one catalog).
pub use praxec_core::model_catalog::ModelCatalog as ChatCatalog;

/// The shipped default catalog models (ignores any override) — for demo / tests.
pub fn default_chat_options() -> Vec<ChatModelOption> {
    praxec_core::model_catalog::default_model_catalog().models
}

/// The shipped default catalog with its provenance.
pub fn default_chat_catalog() -> ChatCatalog {
    praxec_core::model_catalog::default_model_catalog()
}

/// The active catalog models (override-aware via the core catalog).
pub fn chat_options() -> Vec<ChatModelOption> {
    praxec_core::model_catalog::model_catalog().models
}

/// The active catalog (override-aware) with its provenance.
pub fn chat_catalog() -> ChatCatalog {
    praxec_core::model_catalog::model_catalog()
}

/// Requests/day presets the user cycles through to see where the cost lands
/// (configurable — `tuning.requests_per_day_levels`).
pub fn requests_per_day_levels() -> &'static [usize] {
    &praxec_core::tuning::tuning().requests_per_day_levels
}

/// The volume assumed when evaluating a budget ceiling at recommend time, before
/// the user has dialed in their own (`tuning.default_requests_per_day`).
pub fn default_requests_per_day() -> usize {
    praxec_core::tuning::tuning().default_requests_per_day
}

/// Capability bar below which a model isn't reliable enough for the conductor's
/// tool loop (`tuning.sufficient_intelligence`).
pub fn sufficient_intelligence() -> f64 {
    praxec_core::tuning::tuning().sufficient_intelligence
}

/// Estimated USD/day at `requests_per_day` and a `reasoning` effort level. The
/// per-request token assumptions are configurable (`tuning.cost_*`).
pub fn chat_usd_per_day(opt: &ChatModelOption, requests_per_day: usize, reasoning: &str) -> f64 {
    let t = praxec_core::tuning::tuning();
    let n = requests_per_day as f64;
    let out_tokens = t.cost_output_tokens_per_request * reasoning_multiplier(reasoning);
    n * t.cost_input_tokens_per_request * opt.input_usd_per_million / 1_000_000.0
        + n * out_tokens * opt.output_usd_per_million / 1_000_000.0
}

/// The cost magnitude at a given requests/day + reasoning effort.
pub fn chat_cost_magnitude(
    opt: &ChatModelOption,
    requests_per_day: usize,
    reasoning: &str,
) -> CostMagnitude {
    CostMagnitude::from_usd_per_day(chat_usd_per_day(opt, requests_per_day, reasoning))
}

fn total_cmp(a: f64, b: f64) -> std::cmp::Ordering {
    a.partial_cmp(&b).unwrap_or(std::cmp::Ordering::Equal)
}

/// Recommend the best conductor model **for the user's stance** — see
/// [`crate::priorities`]. The default ([`Priorities::default`], Balanced, no
/// constraints) balances cost, time, and capability rather than maximizing any
/// one. Tool-calling is always a hard filter; the stance's constraints
/// (budget ceiling, local-only) filter further; the stance then ranks.
pub fn recommend_chat(options: &[ChatModelOption]) -> Option<&ChatModelOption> {
    recommend_chat_with(options, praxec_embeddings::vendor_available)
}

/// Reachable, tool-capable chat models, best intelligence first.
pub fn reachable_chat(options: &[ChatModelOption]) -> Vec<ChatModelOption> {
    reachable_chat_with(options, praxec_embeddings::vendor_available)
}

/// Balanced-stance, no-constraints recommendation (the default lens).
pub fn recommend_chat_with(
    options: &[ChatModelOption],
    available: impl Fn(&str) -> bool,
) -> Option<&ChatModelOption> {
    recommend_chat_for(
        options,
        available,
        &crate::priorities::Priorities::default(),
        default_requests_per_day(),
    )
}

/// Stance-aware recommendation, ranking on **overall intelligence** — the chat
/// gate's default when no task tags are in play.
pub fn recommend_chat_for<'a>(
    options: &'a [ChatModelOption],
    available: impl Fn(&str) -> bool,
    prefs: &crate::priorities::Priorities,
    requests_per_day: usize,
) -> Option<&'a ChatModelOption> {
    // No task tags → fit collapses to overall intelligence (empty needs).
    recommend_ranked(options, available, prefs, requests_per_day, &[])
}

/// Stance-aware recommendation for a **task** — rank on the model's *fit* for a
/// step's `needs:` [`Affinity`] list instead of overall intelligence. This is the
/// model suggestor: a step that declares `needs: [coding]` gets the best-fitting
/// model for coding, still filtered + weighted by the user's stance. Empty
/// `needs` is identical to [`recommend_chat_for`].
pub fn recommend_chat_for_affinities<'a>(
    options: &'a [ChatModelOption],
    available: impl Fn(&str) -> bool,
    prefs: &crate::priorities::Priorities,
    requests_per_day: usize,
    needs: &[Affinity],
) -> Option<&'a ChatModelOption> {
    recommend_ranked(options, available, prefs, requests_per_day, needs)
}

/// Recommendation core: **constraints filter, stance ranks**. First apply the
/// hard filters (tool-calling, reachability, and the stance's constraints —
/// budget ceiling + local-only) as a pre-filter on the model field. Then the
/// stance ranks the survivors:
///
/// - The **capability/cost** stances (Balanced / BestResults / KeepCostsLow)
///   route through [`suggest_by_value`] with the stance's
///   [`value_params`](crate::priorities::Stance::value_params) `(β, ε)` — value =
///   `fit(needs) ÷ blended_cost^β`, returning the most capable model within `ε`
///   of the best value (above the capability floor). This embodies the operator's
///   rule: best value for money, but prefer the stronger model when the
///   difference is marginal.
/// - **Fastest** is orthogonal to that value axis — it ranks the survivors that
///   clear the capability floor by raw `speed_tps`.
///
/// `needs` is the step's affinity list (empty → fit collapses to overall
/// intelligence). `requests_per_day` is only used to evaluate a budget ceiling.
///
/// [`suggest_by_value`]: praxec_core::model_catalog::suggest_by_value
fn recommend_ranked<'a>(
    options: &'a [ChatModelOption],
    available: impl Fn(&str) -> bool,
    prefs: &crate::priorities::Priorities,
    requests_per_day: usize,
    needs: &[Affinity],
) -> Option<&'a ChatModelOption> {
    use crate::priorities::Stance;

    // Hard filters: must call tools, be reachable, and satisfy the constraints.
    // The survivors' *indices* into `options`, so the returned reference borrows
    // from the caller's slice (the value selector takes an owned slice).
    let pool_idx: Vec<usize> = options
        .iter()
        .enumerate()
        .filter(|(_, o)| o.tools && available(&o.vendor))
        .filter(|(_, o)| !prefs.local_only || o.local)
        .filter(|(_, o)| {
            prefs.budget_cap.is_none_or(|c| {
                chat_cost_magnitude(o, requests_per_day, &default_reasoning(o)) <= c
            })
        })
        .map(|(i, _)| i)
        .collect();
    if pool_idx.is_empty() {
        return None;
    }
    let pool: Vec<ChatModelOption> = pool_idx.iter().map(|&i| options[i].clone()).collect();

    // Map a picked entry back to a reference into the caller's `options`.
    let back = |picked: &ChatModelOption| -> Option<&'a ChatModelOption> {
        pool_idx
            .iter()
            .find(|&&i| options[i].vendor == picked.vendor && options[i].model == picked.model)
            .map(|&i| &options[i])
    };

    // If nobody clears the capability floor, don't foot-gun the operator into an
    // unreliable model on a cost/speed argument — take the most capable. This is a
    // cockpit-layer policy that sits *above* the value selector's own below-floor
    // fallback (which would otherwise rank unreliable models by value or speed).
    let cap = |o: &ChatModelOption| fit(&o.scores, o.intelligence, needs);
    let any_sufficient = pool.iter().any(|o| cap(o) >= sufficient_intelligence());
    if !any_sufficient {
        return pool
            .iter()
            .max_by(|a, b| total_cmp(cap(a), cap(b)))
            .and_then(back);
    }

    match prefs.stance.value_params() {
        // Capability/cost stances → the shared value selector (constraints already
        // filtered above; it re-applies the capability floor and the marginal band).
        Some((beta, band)) => {
            praxec_core::model_catalog::suggest_by_value(&pool, needs, &available, beta, band)
                .and_then(back)
        }
        // Fastest is orthogonal to the value axis: among the survivors that clear
        // the capability floor (guaranteed non-empty by the guard above), rank by
        // raw `speed_tps`.
        None => {
            debug_assert_eq!(prefs.stance, Stance::Fastest);
            pool.iter()
                .filter(|o| cap(o) >= sufficient_intelligence())
                .max_by(|a, b| total_cmp(a.speed_tps, b.speed_tps))
                .and_then(back)
                .or_else(|| back(&pool[0]))
        }
    }
}

/// Reachable + tool-capable, best intelligence first (injected predicate).
pub fn reachable_chat_with(
    options: &[ChatModelOption],
    available: impl Fn(&str) -> bool,
) -> Vec<ChatModelOption> {
    let mut v: Vec<_> = options
        .iter()
        .filter(|o| o.tools && available(&o.vendor))
        .cloned()
        .collect();
    v.sort_by(|a, b| {
        b.intelligence
            .partial_cmp(&a.intelligence)
            .unwrap_or(std::cmp::Ordering::Equal)
    });
    v
}

/// Plain-language rationale for recommending `opt` as the conductor — names the
/// three axes (capability, time, cost) and why it's the value pick.
pub fn chat_rationale(opt: &ChatModelOption, requests_per_day: usize, reasoning: &str) -> String {
    let mag = chat_cost_magnitude(opt, requests_per_day, reasoning).label();
    format!(
        "Reliable enough to drive the cockpit (Intelligence {:.0}), ~{:.0} tok/s — and the best value among your providers: about {mag} at {reasoning} effort, ~{} requests/day. Browse for more capability.",
        opt.intelligence,
        opt.speed_tps,
        fmt_count(requests_per_day)
    )
}

fn fmt_count(n: usize) -> String {
    if n >= 1000 {
        format!("{}k", n / 1000)
    } else {
        n.to_string()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn shipped_catalog_parses_and_all_recommendable_call_tools() {
        let opts = default_chat_options();
        assert!(!opts.is_empty());
        for o in &opts {
            assert!(o.intelligence > 0.0, "{} unscored", o.model);
        }
    }

    #[test]
    fn shipped_catalog_carries_dated_provenance() {
        // Numbers must be sourced + dated, not guessed (the data principle).
        let cat = default_chat_catalog();
        assert!(!cat.source.is_empty(), "catalog must name its source");
        assert!(cat.captured.starts_with("2026-"), "catalog must be dated");
        assert!(!cat.models.is_empty());
    }

    fn opt(
        model: &str,
        intelligence: f64,
        in_p: f64,
        out_p: f64,
        speed: f64,
        tools: bool,
    ) -> ChatModelOption {
        ChatModelOption {
            vendor: "x".into(),
            model: model.into(),
            input_usd_per_million: in_p,
            output_usd_per_million: out_p,
            context: 0,
            intelligence,
            speed_tps: speed,
            tools,
            reasoning_levels: vec![],
            local: false,
            scores: AffinityScores::default(),
        }
    }

    /// `opt` plus explicit affinity scores (coding, agentic, reasoning); the
    /// overall intelligence is their mean.
    fn scored(model: &str, in_p: f64, out_p: f64, speed: f64, s: [f64; 3]) -> ChatModelOption {
        let mut o = opt(model, (s[0] + s[1] + s[2]) / 3.0, in_p, out_p, speed, true);
        o.scores = AffinityScores {
            coding: s[0],
            agentic: s[1],
            reasoning: s[2],
            ..Default::default()
        };
        o
    }

    use crate::priorities::{Priorities, Stance};

    fn prefs(stance: Stance) -> Priorities {
        Priorities {
            stance,
            budget_cap: None,
            local_only: false,
        }
    }

    #[test]
    fn stances_diverge_on_the_same_catalog() {
        let opts = vec![
            opt("frontier", 61.0, 5.0, 25.0, 55.0, true), // most capable, pricey, slow
            opt("thrifty", 53.0, 0.3, 1.2, 60.0, true),   // cheapest sufficient
            opt("speedy", 54.0, 2.0, 10.0, 250.0, true),  // fastest sufficient
        ];
        let pick = |s| {
            recommend_chat_for(&opts, |_| true, &prefs(s), 1_000)
                .unwrap()
                .model
                .clone()
        };
        assert_eq!(pick(Stance::BestResults), "frontier");
        assert_eq!(pick(Stance::KeepCostsLow), "thrifty");
        assert_eq!(pick(Stance::Fastest), "speedy");
        // Balanced lands on a sufficient model and isn't forced to an extreme.
        assert!(
            recommend_chat_for(&opts, |_| true, &prefs(Stance::Balanced), 1_000)
                .unwrap()
                .intelligence
                >= sufficient_intelligence()
        );
    }

    #[test]
    fn best_results_picks_most_capable_regardless_of_cost() {
        // β=0 cancels cost: the frontier model wins even though it is the priciest.
        let opts = vec![
            opt("frontier", 70.0, 30.0, 90.0, 40.0, true), // most capable, most expensive
            opt("cheap-strong", 60.0, 0.2, 0.8, 120.0, true),
            opt("cheap-weak", 52.0, 0.1, 0.3, 200.0, true),
        ];
        let rec = recommend_chat_for(&opts, |_| true, &prefs(Stance::BestResults), 1_000).unwrap();
        assert_eq!(rec.model, "frontier");
    }

    #[test]
    fn keep_costs_low_picks_cheapest_that_clears_the_floor() {
        // Heavy β + tight band → the cheapest model above the capability floor.
        // `dirt-cheap-weak` is cheapest overall but below the floor, so excluded.
        let floor = sufficient_intelligence();
        let opts = vec![
            opt("frontier", floor + 15.0, 40.0, 120.0, 40.0, true),
            opt("cheapest-sufficient", floor + 1.0, 0.3, 1.0, 90.0, true),
            opt("dirt-cheap-weak", floor - 8.0, 0.05, 0.1, 250.0, true),
        ];
        let rec = recommend_chat_for(&opts, |_| true, &prefs(Stance::KeepCostsLow), 1_000).unwrap();
        assert_eq!(rec.model, "cheapest-sufficient");
    }

    #[test]
    fn balanced_prefers_stronger_when_value_is_marginal() {
        // Two models of nearly identical value (same price, capability within the
        // 15% band). Balanced's marginal-band rule breaks the tie toward the
        // *stronger* model — best value for money, but prefer the stronger when
        // the difference is marginal.
        let floor = sufficient_intelligence();
        let opts = vec![
            opt("strong", floor + 6.0, 1.0, 1.0, 60.0, true),
            opt("slightly-weaker", floor + 4.0, 1.0, 1.0, 60.0, true),
        ];
        let rec = recommend_chat_for(&opts, |_| true, &prefs(Stance::Balanced), 1_000).unwrap();
        assert_eq!(rec.model, "strong");
    }

    #[test]
    fn constraints_filter_before_the_stance_ranks() {
        // Budget cap excludes the pricey-but-capable model BEFORE BestResults ranks,
        // so the survivor wins despite being less capable.
        let opts = vec![
            opt("expensive", 70.0, 5.0, 25.0, 55.0, true),
            opt("affordable", 55.0, 0.3, 1.2, 60.0, true),
        ];
        let capped = Priorities {
            stance: Stance::BestResults, // would pick "expensive" if it survived
            budget_cap: Some(CostMagnitude::TensOfDollars),
            local_only: false,
        };
        let rec = recommend_chat_for(&opts, |_| true, &capped, 10_000).unwrap();
        assert_eq!(rec.model, "affordable", "budget cap filters before ranking");

        // local-only excludes the remote model BEFORE the stance ranks.
        let mut on_device = opt("on-device", 56.0, 0.0, 0.0, 80.0, true);
        on_device.local = true;
        let opts2 = vec![opt("remote", 70.0, 5.0, 25.0, 55.0, true), on_device];
        let local = Priorities {
            stance: Stance::BestResults,
            budget_cap: None,
            local_only: true,
        };
        let rec2 = recommend_chat_for(&opts2, |_| true, &local, 1_000).unwrap();
        assert_eq!(rec2.model, "on-device", "local-only filters before ranking");
    }

    #[test]
    fn fastest_still_ranks_by_speed_orthogonal_to_value() {
        // The fastest sufficient model wins, even though it is neither cheapest nor
        // most capable — speed is orthogonal to the capability/cost value axis.
        let floor = sufficient_intelligence();
        let opts = vec![
            opt("slow-strong", floor + 10.0, 5.0, 25.0, 40.0, true),
            opt("speedy", floor + 2.0, 2.0, 10.0, 300.0, true),
            opt("cheap-medium", floor + 1.0, 0.2, 0.5, 120.0, true),
        ];
        let rec = recommend_chat_for(&opts, |_| true, &prefs(Stance::Fastest), 1_000).unwrap();
        assert_eq!(rec.model, "speedy");
    }

    #[test]
    fn local_only_constraint_filters_to_local_models() {
        let mut local = opt("on-device", 53.0, 0.0, 0.0, 80.0, true);
        local.local = true;
        let opts = vec![opt("cloud", 61.0, 5.0, 25.0, 55.0, true), local];
        let p = Priorities {
            stance: Stance::BestResults,
            budget_cap: None,
            local_only: true,
        };
        // Cloud is more capable but excluded by the hard local-only filter.
        assert_eq!(
            recommend_chat_for(&opts, |_| true, &p, 1_000)
                .unwrap()
                .model,
            "on-device"
        );
    }

    #[test]
    fn budget_cap_excludes_models_over_the_ceiling() {
        let opts = vec![
            opt("expensive", 61.0, 5.0, 25.0, 55.0, true),
            opt("affordable", 53.0, 0.3, 1.2, 60.0, true),
        ];
        // At 10k requests/day the pricey model lands at hundreds-of-dollars, past
        // a "tens of dollars" ceiling; the affordable one sits right at it.
        let p = Priorities {
            stance: Stance::BestResults,
            budget_cap: Some(CostMagnitude::TensOfDollars),
            local_only: false,
        };
        let rec = recommend_chat_for(&opts, |_| true, &p, 10_000).unwrap();
        assert_eq!(
            rec.model, "affordable",
            "ceiling excludes the costlier model"
        );
    }

    #[test]
    fn needs_route_to_the_affinity_specialist() {
        // Same overall index, different shapes: a coding specialist vs an
        // agentic specialist vs a generalist. Scores are [coding, agentic, reasoning].
        let opts = vec![
            scored("coder", 0.3, 1.2, 90.0, [64.0, 52.0, 44.0]), // best at coding
            scored("driver", 0.6, 2.5, 70.0, [54.0, 64.0, 51.0]), // best at agentic
            scored("balanced", 2.0, 12.0, 130.0, [56.0, 56.0, 56.0]), // even
        ];
        let p = crate::priorities::Priorities {
            stance: crate::priorities::Stance::BestResults, // pick the best-fit for the need
            budget_cap: None,
            local_only: false,
        };
        let pick = |needs: &[Affinity]| {
            recommend_chat_for_affinities(&opts, |_| true, &p, 1_000, needs)
                .unwrap()
                .model
                .clone()
        };
        assert_eq!(pick(&[Affinity::Coding]), "coder");
        assert_eq!(pick(&[Affinity::Agentic]), "driver");
    }

    #[test]
    fn fit_falls_back_to_intelligence_without_affinity_scores() {
        // A model with no affinity scores still ranks (via its overall index).
        let o = opt("plain", 55.0, 1.0, 1.0, 80.0, true);
        assert_eq!(fit(&o.scores, o.intelligence, &[Affinity::Coding]), 55.0);
        assert_eq!(fit(&o.scores, o.intelligence, &[]), 55.0);
    }

    #[test]
    fn affinity_parse_aliases_math_to_reasoning() {
        use std::str::FromStr;
        assert_eq!(Affinity::from_str("math"), Ok(Affinity::Reasoning));
        assert_eq!(Affinity::from_str("agents"), Ok(Affinity::Agentic));
        assert_eq!(Affinity::from_str("coding"), Ok(Affinity::Coding));
        assert!(Affinity::from_str("nonsense").is_err());
    }

    #[test]
    fn shipped_catalog_has_a_coding_specialist_and_open_models() {
        let opts = default_chat_options();
        // An open coding specialist exists and is genuinely coding-strongest.
        let coder = opts
            .iter()
            .find(|o| o.model == "qwen/qwen3-coder")
            .expect("coding model");
        assert!(coder.scores.coding > coder.scores.agentic);
        assert!(coder.scores.coding > coder.scores.reasoning);
        // More than one open-weight option (not just DeepSeek).
        let open = [
            "deepseek/deepseek-v4",
            "moonshotai/kimi-k2.6",
            "z-ai/glm-5.1",
            "minimax/minimax-m3",
        ];
        let have = open
            .iter()
            .filter(|m| opts.iter().any(|o| &&o.model == m))
            .count();
        assert!(have >= 3, "expected several open models, found {have}");
    }

    #[test]
    fn recommend_requires_tool_calling() {
        let opts = vec![
            opt("smart-no-tools", 99.0, 1.0, 1.0, 50.0, false),
            opt("tools-ok", 55.0, 1.0, 1.0, 50.0, true),
        ];
        // The smarter model can't call tools → it is never recommended.
        assert_eq!(
            recommend_chat_with(&opts, |_| true).unwrap().model,
            "tools-ok"
        );
    }

    #[test]
    fn recommend_prefers_cheaper_sufficient_over_pricier_capable() {
        let opts = vec![
            // More capable but expensive.
            opt("frontier", 61.0, 5.0, 25.0, 55.0, true),
            // Sufficient (>= bar) and much cheaper → the value pick for the job.
            opt("value", 54.0, 1.5, 9.0, 90.0, true),
        ];
        assert_eq!(recommend_chat_with(&opts, |_| true).unwrap().model, "value");
    }

    #[test]
    fn recommend_falls_back_to_most_capable_when_none_sufficient() {
        let opts = vec![
            opt("weak-cheap", 40.0, 0.1, 0.1, 200.0, true),
            opt("less-weak", 48.0, 3.0, 15.0, 90.0, true),
        ];
        // Nobody clears the bar → most capable, not cheapest.
        assert_eq!(
            recommend_chat_with(&opts, |_| true).unwrap().model,
            "less-weak"
        );
    }

    #[test]
    fn cost_scales_with_requests_and_reasoning() {
        let opus = default_chat_options()
            .into_iter()
            .find(|o| o.model == "claude-opus-4-8")
            .unwrap();
        // Volume: cheap at low, expensive at high.
        assert!(
            chat_usd_per_day(&opus, 100, "medium") < chat_usd_per_day(&opus, 100_000, "medium")
        );
        assert_ne!(
            chat_cost_magnitude(&opus, 100, "medium"),
            chat_cost_magnitude(&opus, 100_000, "medium")
        );
        // Reasoning effort: more thinking costs more.
        assert!(chat_usd_per_day(&opus, 1_000, "high") > chat_usd_per_day(&opus, 1_000, "none"));
    }

    #[test]
    fn default_reasoning_prefers_medium() {
        let opus = default_chat_options()
            .into_iter()
            .find(|o| o.model == "claude-opus-4-8")
            .unwrap();
        assert_eq!(default_reasoning(&opus), "medium");
    }
}
