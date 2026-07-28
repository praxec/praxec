//! The **slow loop**: governed, *bidirectional* base-model re-selection driven by
//! the audit. The fast loop (per-run) escalates a step's model when its
//! acceptance bar fails; this loop looks across runs and *proposes* a durable
//! change to a step's base model — **lower** it to save money when a cheaper
//! model is clearing the bar with real margin, or **raise** it when the current
//! base is chronically failing. It NEVER applies a change: it emits a proposal
//! (the `models.yaml` edit + the evidence) for a human-approval gate. Praxec
//! governing its own model config (the recursion).
//!
//! Three pure layers, each independently testable:
//! 1. [`observations_from_audit`] — correlate `agent.invoked` / `agent.completed`
//!    / `chain.failed` events (by `correlation_id`) into per-step outcomes.
//! 2. [`aggregate`] — roll observations up per `(affinity, model, effort)`: run
//!    count, pass-rate, mean realized cost. (Affinity is the `models.yaml` key —
//!    the unit the base actually configures; the steps it covers are evidence.)
//!    Effort is part of the identity (#12): reasoning levels aren't portable
//!    across models, so `model@medium` and `model@high` are distinct evidence.
//! 3. [`propose`] — the **conservative** decision. Lowering requires the cheaper
//!    model's pass-rate to be *at or above* the base's AND material savings;
//!    a marginal value gain is NOT enough — keep the stronger model. Raising
//!    triggers when the base's pass-rate falls below the failing bar. Thresholds
//!    come from [`tuning`](crate::tuning), never hard-coded.
//!
//! Producer ≠ evaluator: "passed" means the step cleared its *independent*
//! acceptance bar (the next transition advanced) — never a model grading itself.

use crate::audit::AuditEvent;
use serde::Serialize;
use serde_json::Value;
use std::collections::{BTreeMap, BTreeSet};

/// One agent-step outcome distilled from correlated audit events.
#[derive(Debug, Clone, PartialEq)]
pub struct StepObservation {
    /// The `models.yaml` key (affinity / ModelRef) the step ran under.
    pub affinity: String,
    /// The transition (step) name.
    pub step: String,
    /// The `provider:model` that ran.
    pub model: String,
    /// (#12) The reasoning effort the model ACTUALLY ran under — read from
    /// `agent.completed` (so it's paired with the WALKED model, which an
    /// escalated hop can make differ from the composer's intent), `None` when the
    /// run applied no effort (provider default) or failed before completing.
    /// Effort is non-portable across models, so `qwen3-coder@medium` and
    /// `@high` are DISTINCT observations — never lumped.
    pub effort: Option<String>,
    /// Cleared its independent acceptance bar (advanced) vs failed / aborted.
    pub passed: bool,
    /// Realized USD for the step (`None` on failure / uncatalogued).
    pub cost_usd: Option<f64>,
}

/// Aggregate stats for one `(affinity, model, effort)` triple. Effort is part of
/// the identity (#12): the same model at two reasoning efforts rolls up into two
/// distinct buckets, so the flywheel proposes against the exact `model@effort`
/// the base is configured to run.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct ModelStats {
    pub affinity: String,
    pub model: String,
    /// (#12) The reasoning effort these runs applied (`None` = provider default);
    /// part of the rollup key, so `model@medium` and `model@high` never merge.
    pub effort: Option<String>,
    pub runs: usize,
    pub passes: usize,
    /// `passes / runs`.
    pub pass_rate: f64,
    /// Mean realized USD over the priced runs (`None` if none were priced).
    pub mean_cost_usd: Option<f64>,
    /// Distinct steps observed under this `(affinity, model)` — evidence.
    pub steps: Vec<String>,
}

/// Which way a proposal moves the base.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Direction {
    /// Cheaper base — the cheap model clears the bar with margin + saves money.
    Lower,
    /// Stronger base — the current base is chronically failing its bar.
    Raise,
    /// (#13) FAIR-TRIAL exploration — a catalog-fit, under-cost-cap model with too
    /// little evidence to exploit yet. Pure-exploit `Lower`/`Raise` can only ever
    /// re-rank models that already have runs, so a shifting ecosystem's new/better
    /// entrants would never be sampled. This surfaces one such entrant per
    /// affinity as a governed proposal to ADD it to the chain for evidence — never
    /// auto-applied, never displacing the proven base.
    Trial,
}

/// A governed proposal to change one affinity's base model. Carries the evidence
/// so a human can judge it; applying it is a separate, gated step.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Proposal {
    pub affinity: String,
    pub direction: Direction,
    pub from_model: String,
    pub to_model: String,
    pub base_runs: usize,
    pub base_pass_rate: f64,
    pub base_mean_cost_usd: Option<f64>,
    pub candidate_runs: usize,
    pub candidate_pass_rate: f64,
    pub candidate_mean_cost_usd: Option<f64>,
    /// `(base_cost - candidate_cost) / base_cost` when both are priced.
    pub savings_pct: Option<f64>,
    pub rationale: String,
}

/// The decision thresholds — data, not code (sourced from [`tuning`](crate::tuning)).
#[derive(Debug, Clone)]
pub struct DeescalationParams {
    /// Minimum runs for a `(affinity, model)` before it can drive a decision.
    pub min_runs: usize,
    /// Pass-rate a candidate must clear to be considered "clearing the bar".
    pub lower_min_pass_rate: f64,
    /// Below this base pass-rate the base is "chronically failing" → raise.
    pub raise_max_pass_rate: f64,
    /// Minimum fractional savings to justify lowering (the conservatism guard).
    pub material_savings_pct: f64,
    /// (#13) The frontier cost cap ($/M output) a fair-trial candidate must sit
    /// UNDER. Same line the runtime cost gate enforces — a trial never nominates a
    /// premium model no human approved. Sourced from
    /// `gateway.cost.frontier_cap_usd_per_m`, defaulting to
    /// [`DEFAULT_FRONTIER_CAP_USD_PER_M`](crate::model_catalog::DEFAULT_FRONTIER_CAP_USD_PER_M).
    pub frontier_cap_usd_per_m: f64,
}

impl DeescalationParams {
    /// Load the thresholds from the active tuning (override-aware). The frontier
    /// cap defaults to the catalog's shipped line; the gateway overrides it from
    /// `gateway.cost.frontier_cap_usd_per_m` when configured.
    pub fn from_tuning() -> Self {
        let d = &crate::tuning::tuning().deescalation;
        Self {
            min_runs: d.min_runs,
            lower_min_pass_rate: d.lower_min_pass_rate,
            raise_max_pass_rate: d.raise_max_pass_rate,
            material_savings_pct: d.material_savings_pct,
            frontier_cap_usd_per_m: crate::model_catalog::DEFAULT_FRONTIER_CAP_USD_PER_M,
        }
    }
}

/// (#12) Build the canonical **chain-identity** string for a model paired with
/// its applied reasoning effort: `"model@effort"`, or bare `"model"` when no
/// effort is paired. This is the unit the flywheel keys on — the SAME model at
/// two efforts is two identities — and the exact form `load_current_chains`
/// writes into each `models.yaml` chain rung, so a proposal's `from`/`to` and the
/// evidence compare on one representation.
pub fn chain_identity(model: &str, effort: Option<&str>) -> String {
    match effort.map(str::trim).filter(|e| !e.is_empty()) {
        Some(e) => format!("{model}@{e}"),
        None => model.to_string(),
    }
}

/// Normalize a chain-identity string to its canonical `(bare-model-id, effort)`
/// comparison key: split off any `@effort` suffix, strip any leading
/// `vendor:`/`provider:` prefix via [`model_catalog::bare_model_id`], and
/// lower-case the effort. THE normalization shared by the flywheel's keying and
/// the fair-trial catalog dedup (#13) — a catalog `vendor:model` and a chain
/// `provider:model` for the same underlying model therefore compare equal.
///
/// [`model_catalog::bare_model_id`]: crate::model_catalog::bare_model_id
fn identity_key(identity: &str) -> (String, String) {
    let (model, effort) = match identity.rsplit_once('@') {
        Some((m, e)) => (m, e),
        None => (identity, ""),
    };
    (
        crate::model_catalog::bare_model_id(model).to_string(),
        effort.trim().to_ascii_lowercase(),
    )
}

/// Do two chain-identity strings name the same `model@effort` after
/// normalization (see [`identity_key`])?
fn same_identity(a: &str, b: &str) -> bool {
    identity_key(a) == identity_key(b)
}

/// Correlate audit events into per-step outcomes. A correlation that carries an
/// `agent.invoked` is an agent step; it **passed** if its `agent.completed`
/// fired, **failed** if a `chain.failed` fired instead (model/affinity come from
/// `agent.invoked`, realized cost from `agent.completed`).
pub fn observations_from_audit(events: &[AuditEvent]) -> Vec<StepObservation> {
    #[derive(Default)]
    struct Acc {
        /// (step, affinity, model) from `agent.invoked`.
        invoked: Option<(String, String, String)>,
        /// (model, cost, effort) from `agent.completed`. Model+effort come from
        /// the SAME event so they're paired — the walked model and the effort it
        /// actually ran under (#12).
        completed: Option<(String, Option<f64>, Option<String>)>,
        failed: bool,
        /// (model, effort) of each attempt that FAILED the structured-output
        /// contract (AGENT_NOT_CONVERGING / NO_RESULT / RESULT_FAILED). Only the
        /// WINNER reaches `agent.completed`, so without these a model that can't
        /// emit the contract stays invisible to the flywheel and keeps getting
        /// routed contract-critical work. Each becomes a FAILED observation.
        contract_failed: Vec<(String, Option<String>)>,
    }
    let str_field = |p: &Value, k: &str| {
        p.get(k)
            .and_then(Value::as_str)
            .unwrap_or("(unknown)")
            .to_string()
    };

    let mut by_cor: BTreeMap<String, Acc> = BTreeMap::new();
    for e in events {
        match e.event_type.as_str() {
            "agent.invoked" => {
                let p = &e.payload;
                by_cor.entry(e.correlation_id.clone()).or_default().invoked = Some((
                    str_field(p, "transition"),
                    str_field(p, "affinity"),
                    str_field(p, "model"),
                ));
            }
            "agent.completed" => {
                let p = &e.payload;
                let cost = p.get("cost_usd").and_then(Value::as_f64);
                // Effort is read HERE (beside the walked model), never from
                // `agent.invoked`: on an escalated hop the completed model differs
                // from the composer's, and effort belongs to the model that ran.
                let effort = p
                    .get("reasoning_effort")
                    .and_then(Value::as_str)
                    .map(str::to_string);
                by_cor
                    .entry(e.correlation_id.clone())
                    .or_default()
                    .completed = Some((str_field(p, "model"), cost, effort));
            }
            "chain.failed" => {
                by_cor.entry(e.correlation_id.clone()).or_default().failed = true;
            }
            "agent.model_attempt" => {
                let p = &e.payload;
                // Match on the STABLE wire-code prefix of `error`, not the Debug
                // `outcome` string. A `success`/`suspended` attempt has no such
                // error, so it is naturally excluded (its spend/pass is on
                // `agent.completed`). A `BudgetExceeded` cut is NOT a contract
                // failure — the per-attempt wall, not the model — so it is excluded.
                let is_contract_failure =
                    p.get("error").and_then(Value::as_str).is_some_and(|err| {
                        err.starts_with("AGENT_NOT_CONVERGING")
                            || err.starts_with("AGENT_NO_RESULT")
                            || err.starts_with("AGENT_RESULT_FAILED")
                    });
                if is_contract_failure {
                    let effort = p
                        .get("reasoning_effort")
                        .and_then(Value::as_str)
                        .map(str::to_string);
                    by_cor
                        .entry(e.correlation_id.clone())
                        .or_default()
                        .contract_failed
                        .push((str_field(p, "model"), effort));
                }
            }
            _ => {}
        }
    }

    let mut out = Vec::new();
    for (_cor, acc) in by_cor {
        let Some((step, affinity, inv_model)) = acc.invoked else {
            continue;
        };
        // Contract-failure attempts → one FAILED observation each, keyed on the
        // attempt's OWN (model, effort) so a losing model's real pass-rate is
        // seen even when the correlation ultimately succeeded via a fallback.
        // These are terminal facts about that attempt, independent of the
        // correlation's final outcome, so they're emitted regardless.
        for (model, effort) in &acc.contract_failed {
            out.push(StepObservation {
                affinity: affinity.clone(),
                step: step.clone(),
                model: model.clone(),
                effort: effort.clone(),
                passed: false,
                cost_usd: None,
            });
        }
        // Passed iff its `agent.completed` fired; failed iff a `chain.failed`
        // fired instead; otherwise still in flight — neither, so skip. On a PASS
        // model+cost+effort all come from `agent.completed` (the actual walked
        // hop); on a FAILURE the model falls back to `agent.invoked` and there's
        // no realized cost or applied effort.
        let (passed, model, cost, effort) = match acc.completed {
            Some((model, cost, effort)) => (true, model, cost, effort),
            None if acc.failed => (false, inv_model, None, None),
            None => continue,
        };
        out.push(StepObservation {
            affinity,
            step,
            model,
            effort,
            passed,
            cost_usd: cost,
        });
    }
    out
}

/// Roll observations up per `(affinity, model, effort)` (#12).
pub fn aggregate(observations: &[StepObservation]) -> Vec<ModelStats> {
    #[derive(Default)]
    struct Acc {
        runs: usize,
        passes: usize,
        cost_sum: f64,
        priced: usize,
        steps: BTreeSet<String>,
    }
    // Effort is part of the key: the same model at two efforts is two buckets.
    let mut map: BTreeMap<(String, String, Option<String>), Acc> = BTreeMap::new();
    for o in observations {
        let a = map
            .entry((o.affinity.clone(), o.model.clone(), o.effort.clone()))
            .or_default();
        a.runs += 1;
        if o.passed {
            a.passes += 1;
        }
        if let Some(c) = o.cost_usd {
            a.cost_sum += c;
            a.priced += 1;
        }
        a.steps.insert(o.step.clone());
    }
    map.into_iter()
        .map(|((affinity, model, effort), a)| ModelStats {
            affinity,
            model,
            effort,
            runs: a.runs,
            passes: a.passes,
            pass_rate: if a.runs > 0 {
                a.passes as f64 / a.runs as f64
            } else {
                0.0
            },
            mean_cost_usd: if a.priced > 0 {
                Some(a.cost_sum / a.priced as f64)
            } else {
                None
            },
            steps: a.steps.into_iter().collect(),
        })
        .collect()
}

/// The conservative bidirectional decision. `current_chains` maps each affinity
/// to its ordered `models.yaml` chain (base first — each rung a
/// [`chain_identity`] `model@effort` string). `catalog` is the active model
/// catalog, consulted ONLY for the fair-trial exploration (#13) — pass `&[]` to
/// disable it and get pure exploit. Returns one proposal per affinity per
/// dimension that warrants a change; affinities at a healthy, well-priced base
/// (or with a only-marginally-cheaper alternative) yield nothing.
pub fn propose(
    stats: &[ModelStats],
    current_chains: &BTreeMap<String, Vec<String>>,
    params: &DeescalationParams,
    catalog: &[crate::model_catalog::ModelEntry],
) -> Vec<Proposal> {
    let mut out = Vec::new();
    for (affinity, chain) in current_chains {
        let Some(base_model) = chain.first() else {
            continue;
        };
        // Match the base by NORMALIZED `model@effort` identity (#12), so the base
        // whose chain rung carries an effort is found against the exactly-keyed
        // stats bucket — and a vendor/provider prefix mismatch never mis-matches.
        let Some(base) = stats.iter().find(|s| {
            &s.affinity == affinity
                && same_identity(&chain_identity(&s.model, s.effort.as_deref()), base_model)
        }) else {
            continue;
        };
        // Not enough evidence on the base to move it either way.
        if base.runs < params.min_runs {
            continue;
        }
        let candidates: Vec<&ModelStats> = stats
            .iter()
            .filter(|s| {
                &s.affinity == affinity
                    && !same_identity(&chain_identity(&s.model, s.effort.as_deref()), base_model)
                    && s.runs >= params.min_runs
            })
            .collect();

        // savings fraction of the base's cost (positive ⇒ cheaper).
        let savings_of = |cand: &ModelStats| -> Option<f64> {
            match (base.mean_cost_usd, cand.mean_cost_usd) {
                (Some(b), Some(c)) if b > 0.0 => Some((b - c) / b),
                _ => None,
            }
        };

        if base.pass_rate < params.raise_max_pass_rate {
            // ── RAISE: the base is chronically failing its bar. ──────────────
            // Prefer an evidenced alternative that clears the bar (most reliable,
            // ties to cheaper); else escalate per the operator's own next rung.
            let mut best: Option<&ModelStats> = None;
            for c in candidates
                .iter()
                .copied()
                .filter(|c| c.pass_rate >= params.lower_min_pass_rate)
            {
                best = Some(match best {
                    None => c,
                    Some(b) => {
                        let c_cost = c.mean_cost_usd.unwrap_or(f64::INFINITY);
                        let b_cost = b.mean_cost_usd.unwrap_or(f64::INFINITY);
                        if c.pass_rate > b.pass_rate
                            || (c.pass_rate == b.pass_rate && c_cost < b_cost)
                        {
                            c
                        } else {
                            b
                        }
                    }
                });
            }

            if let Some(cand) = best {
                let savings = savings_of(cand);
                out.push(Proposal {
                    affinity: affinity.clone(),
                    direction: Direction::Raise,
                    from_model: base_model.clone(),
                    to_model: chain_identity(&cand.model, cand.effort.as_deref()),
                    base_runs: base.runs,
                    base_pass_rate: base.pass_rate,
                    base_mean_cost_usd: base.mean_cost_usd,
                    candidate_runs: cand.runs,
                    candidate_pass_rate: cand.pass_rate,
                    candidate_mean_cost_usd: cand.mean_cost_usd,
                    savings_pct: savings,
                    rationale: format!(
                        "base {} clears its bar only {:.0}% of {} runs (< {:.0}% failing bar); \
                         {} clears it {:.0}% of {} runs — raise the base for reliability.",
                        base_model,
                        base.pass_rate * 100.0,
                        base.runs,
                        params.raise_max_pass_rate * 100.0,
                        cand.model,
                        cand.pass_rate * 100.0,
                        cand.runs,
                    ),
                });
            } else if let Some(next) = chain.get(1) {
                // No evidenced alternative — escalate per the declared chain.
                out.push(Proposal {
                    affinity: affinity.clone(),
                    direction: Direction::Raise,
                    from_model: base_model.clone(),
                    to_model: next.clone(),
                    base_runs: base.runs,
                    base_pass_rate: base.pass_rate,
                    base_mean_cost_usd: base.mean_cost_usd,
                    candidate_runs: 0,
                    candidate_pass_rate: 0.0,
                    candidate_mean_cost_usd: None,
                    savings_pct: None,
                    rationale: format!(
                        "base {} clears its bar only {:.0}% of {} runs (< {:.0}% failing bar) \
                         and no alternative has evidence — escalate to the next chain rung {} \
                         for review.",
                        base_model,
                        base.pass_rate * 100.0,
                        base.runs,
                        params.raise_max_pass_rate * 100.0,
                        next,
                    ),
                });
            }
        } else if base.pass_rate >= params.lower_min_pass_rate {
            // ── LOWER: the base is healthy; is a cheaper model just as good? ──
            // Conservative: the candidate must clear the bar, match or beat the
            // base's pass-rate, AND save materially. A marginal saving ⇒ keep
            // the stronger model (no proposal).
            let mut qualifying: Vec<(&ModelStats, f64, f64)> = Vec::new();
            for c in &candidates {
                let (Some(cc), Some(savings)) = (c.mean_cost_usd, savings_of(c)) else {
                    continue;
                };
                if savings >= params.material_savings_pct
                    && c.pass_rate >= params.lower_min_pass_rate
                    && c.pass_rate >= base.pass_rate
                {
                    qualifying.push((c, savings, cc));
                }
            }
            // Cheapest-effective: the lowest-cost qualifier.
            if let Some((cand, savings, _)) = qualifying
                .into_iter()
                .min_by(|a, b| a.2.partial_cmp(&b.2).unwrap_or(std::cmp::Ordering::Equal))
            {
                out.push(Proposal {
                    affinity: affinity.clone(),
                    direction: Direction::Lower,
                    from_model: base_model.clone(),
                    to_model: chain_identity(&cand.model, cand.effort.as_deref()),
                    base_runs: base.runs,
                    base_pass_rate: base.pass_rate,
                    base_mean_cost_usd: base.mean_cost_usd,
                    candidate_runs: cand.runs,
                    candidate_pass_rate: cand.pass_rate,
                    candidate_mean_cost_usd: cand.mean_cost_usd,
                    savings_pct: Some(savings),
                    rationale: format!(
                        "{} clears the bar {:.0}% of {} runs (>= base {} at {:.0}%) and costs \
                         {:.0}% less — lower the base to bank the saving.",
                        cand.model,
                        cand.pass_rate * 100.0,
                        cand.runs,
                        base_model,
                        base.pass_rate * 100.0,
                        savings * 100.0,
                    ),
                });
            }
        }
        // else: ambiguous middle band (failing bar ≤ pass-rate < healthy bar) —
        // leave the base alone.

        // ── FAIR-TRIAL exploration (#13) ─────────────────────────────────────
        // Exploit (above) can only ever re-rank models that already have runs; a
        // zero-evidence catalog entrant could never surface, so a shifting
        // ecosystem's new/better models would never be sampled. Independently of
        // the exploit outcome, surface AT MOST ONE catalog-fit, under-cost-cap,
        // not-yet-charted, under-evidenced model for this affinity as a governed
        // `Trial` — the human decides whether to spend evidence on it. Reached
        // only for affinities with a well-evidenced base (the `continue`s above),
        // so we explore alternatives to something proven, never noise.
        if let Some(trial) = fair_trial(affinity, chain, base, stats, params, catalog) {
            out.push(trial);
        }
    }
    out
}

/// (#13) Pick the affinity's best-fit untried fair-trial candidate: the
/// highest-dimension-score catalog model that (a) fits the affinity, (b) sits
/// UNDER the frontier cost cap, (c) is not already a rung of this chain, (d) has
/// `< min_runs` evidence for this affinity, and (e) is runnable (its vendor is
/// reachable). Returns a governed `Trial` proposal, or `None` when no entrant
/// qualifies. At most one per affinity per run.
fn fair_trial(
    affinity: &str,
    chain: &[String],
    base: &ModelStats,
    stats: &[ModelStats],
    params: &DeescalationParams,
    catalog: &[crate::model_catalog::ModelEntry],
) -> Option<Proposal> {
    use crate::model_catalog::bare_model_id;
    use crate::model_resolver::Affinity;

    // A dimension we can't parse into an `Affinity` has no fit signal — skip
    // (never guess). `models.yaml` keys are affinity names, so this is the
    // normal path for the built-in dimensions and a clean no-op for a bespoke key.
    let aff: Affinity = affinity.parse().ok()?;

    // Models already charted in this chain — compared on the normalized bare id
    // (a catalog `vendor:model` must dedup against a chain `provider:model`).
    let charted: BTreeSet<String> = chain.iter().map(|c| identity_key(c).0).collect();

    // The most evidence any effort of a model has for THIS affinity — used to
    // exclude models already fairly tried (`>= min_runs`).
    let evidence_for = |bare: &str| -> usize {
        stats
            .iter()
            .filter(|s| s.affinity == affinity && bare_model_id(&s.model) == bare)
            .map(|s| s.runs)
            .max()
            .unwrap_or(0)
    };

    let mut best: Option<(&crate::model_catalog::ModelEntry, f64)> = None;
    for m in catalog {
        let bare = &m.model;
        if charted.contains(bare.as_str()) {
            continue; // already in the chain — nothing to trial
        }
        if evidence_for(bare) >= params.min_runs {
            continue; // already fairly tried — exploit handles it
        }
        // Over the cost cap — a human must approve premium spend. Read the
        // entry's OWN catalog price (the same number `is_frontier` reads for the
        // active catalog; `>= cap` is the frontier line, per WS2).
        if m.output_usd_per_million >= params.frontier_cap_usd_per_m {
            continue;
        }
        // An agent base drives tools; a non-tool-calling model can't be one.
        if !m.tools {
            continue;
        }
        // Only nominate a model we can actually run (key present / local).
        if !crate::model_catalog::vendor_available(&m.vendor) {
            continue;
        }
        // Fit for THIS affinity's dimension: the model must show real strength
        // (score > 0), not merely inherit overall intelligence — a fair trial is
        // a *targeted* bet, not a random draw.
        let score = aff.score(&m.scores, 0.0);
        if score <= 0.0 {
            continue;
        }
        let improves = match best {
            None => true,
            Some((_, bs)) => score > bs,
        };
        if improves {
            best = Some((m, score));
        }
    }
    let (cand, score) = best?;
    Some(Proposal {
        affinity: affinity.to_string(),
        direction: Direction::Trial,
        from_model: chain_identity(&base.model, base.effort.as_deref()),
        to_model: cand.model_string(),
        base_runs: base.runs,
        base_pass_rate: base.pass_rate,
        base_mean_cost_usd: base.mean_cost_usd,
        candidate_runs: 0,
        candidate_pass_rate: 0.0,
        // Untried → no realized per-run cost yet. Its catalog $/M rate rides in
        // the rationale (the fair-trial is about GETTING this evidence).
        candidate_mean_cost_usd: None,
        savings_pct: None,
        rationale: format!(
            "{} is a catalog-fit ({} score {:.0}), under-cap (< ${:.2}/M) model with no \
             evidence yet for '{}' — trial it against the proven base {} ({:.0}% over {} runs) \
             to sample a possibly-better/cheaper option. Governed: added as a chain rung only on \
             human approval, never auto-applied.",
            cand.model_string(),
            affinity,
            score,
            params.frontier_cap_usd_per_m,
            affinity,
            base.model,
            base.pass_rate * 100.0,
            base.runs,
        ),
    })
}

/// Rewrite an affinity's chain to enact a proposal: the new base goes first.
/// **Lower** keeps the old base as a higher escalation rung; **Raise** drops the
/// failing base (escalation only ratchets up). **Trial** (#13) is additive: it
/// keeps the proven chain untouched and APPENDS the trial model as a new lowest
/// rung, so the candidate accrues evidence without displacing the base — the
/// conservative, reversible enactment for an exploration bet.
pub fn apply_to_chain(proposal: &Proposal, old_chain: &[String]) -> Vec<String> {
    if proposal.direction == Direction::Trial {
        let mut new_chain = old_chain.to_vec();
        if !new_chain
            .iter()
            .any(|m| same_identity(m, &proposal.to_model))
        {
            new_chain.push(proposal.to_model.clone());
        }
        return new_chain;
    }
    let mut new_chain = vec![proposal.to_model.clone()];
    for m in old_chain {
        if m == &proposal.to_model {
            continue;
        }
        if proposal.direction == Direction::Raise && m == &proposal.from_model {
            continue;
        }
        new_chain.push(m.clone());
    }
    new_chain
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn params() -> DeescalationParams {
        DeescalationParams {
            min_runs: 5,
            lower_min_pass_rate: 0.9,
            raise_max_pass_rate: 0.6,
            material_savings_pct: 0.25,
            frontier_cap_usd_per_m: 5.0,
        }
    }

    fn stat(
        affinity: &str,
        model: &str,
        runs: usize,
        pass_rate: f64,
        mean_cost: Option<f64>,
    ) -> ModelStats {
        stat_effort(affinity, model, None, runs, pass_rate, mean_cost)
    }

    #[allow(clippy::too_many_arguments)]
    fn stat_effort(
        affinity: &str,
        model: &str,
        effort: Option<&str>,
        runs: usize,
        pass_rate: f64,
        mean_cost: Option<f64>,
    ) -> ModelStats {
        ModelStats {
            affinity: affinity.into(),
            model: model.into(),
            effort: effort.map(str::to_string),
            runs,
            passes: (runs as f64 * pass_rate).round() as usize,
            pass_rate,
            mean_cost_usd: mean_cost,
            steps: vec!["draft".into()],
        }
    }

    fn chains(pairs: &[(&str, &[&str])]) -> BTreeMap<String, Vec<String>> {
        pairs
            .iter()
            .map(|(k, v)| (k.to_string(), v.iter().map(|s| s.to_string()).collect()))
            .collect()
    }

    // ── decision layer ──────────────────────────────────────────────────────

    #[test]
    fn lowers_when_cheaper_model_passes_consistently_and_saves_materially() {
        // base passes 0.95 @ $1.00; cheaper passes 0.95 @ $0.50 → 50% savings.
        let stats = vec![
            stat("reasoning", "v:base", 10, 0.95, Some(1.00)),
            stat("reasoning", "v:cheap", 10, 0.95, Some(0.50)),
        ];
        let ch = chains(&[("reasoning", &["v:base", "v:ceiling"])]);
        let props = propose(&stats, &ch, &params(), &[]);
        assert_eq!(props.len(), 1, "expected one proposal: {props:?}");
        let p = &props[0];
        assert_eq!(p.direction, Direction::Lower);
        assert_eq!(p.from_model, "v:base");
        assert_eq!(p.to_model, "v:cheap");
        assert!((p.savings_pct.unwrap() - 0.50).abs() < 1e-9);
    }

    #[test]
    fn does_not_lower_when_savings_are_marginal() {
        // The philosophy guardrail: cheaper + clears the bar, but only 8% cheaper
        // (< 25% material threshold) → keep the stronger model. No proposal.
        let stats = vec![
            stat("reasoning", "v:base", 10, 0.95, Some(1.00)),
            stat("reasoning", "v:cheap", 10, 0.95, Some(0.92)),
        ];
        let ch = chains(&[("reasoning", &["v:base"])]);
        assert!(propose(&stats, &ch, &params(), &[]).is_empty());
    }

    #[test]
    fn does_not_lower_when_cheaper_model_misses_the_bar() {
        // Cheaper and materially so, but its pass-rate (0.70) is below the base's
        // (0.95) and below the bar → false economy, keep the base.
        let stats = vec![
            stat("reasoning", "v:base", 10, 0.95, Some(1.00)),
            stat("reasoning", "v:cheap", 10, 0.70, Some(0.40)),
        ];
        let ch = chains(&[("reasoning", &["v:base"])]);
        assert!(propose(&stats, &ch, &params(), &[]).is_empty());
    }

    #[test]
    fn picks_the_cheapest_qualifying_candidate_when_lowering() {
        // base 0.90 @ $1.00; two cheaper qualifiers — pick the cheapest-effective.
        let stats = vec![
            stat("reasoning", "v:base", 10, 0.90, Some(1.00)),
            stat("reasoning", "v:midA", 10, 0.95, Some(0.70)),
            stat("reasoning", "v:cheapB", 10, 0.96, Some(0.50)),
        ];
        let ch = chains(&[("reasoning", &["v:base"])]);
        let props = propose(&stats, &ch, &params(), &[]);
        assert_eq!(props.len(), 1);
        assert_eq!(props[0].to_model, "v:cheapB");
    }

    #[test]
    fn raises_when_base_chronically_fails_and_a_candidate_clears_the_bar() {
        // base passes only 0.40 (< 0.60 failing bar); a stronger model clears it.
        let stats = vec![
            stat("reasoning", "v:base", 10, 0.40, Some(1.00)),
            stat("reasoning", "v:strong", 10, 0.95, Some(3.00)),
        ];
        let ch = chains(&[("reasoning", &["v:base", "v:strong"])]);
        let props = propose(&stats, &ch, &params(), &[]);
        assert_eq!(props.len(), 1);
        let p = &props[0];
        assert_eq!(p.direction, Direction::Raise);
        assert_eq!(p.from_model, "v:base");
        assert_eq!(p.to_model, "v:strong");
    }

    #[test]
    fn raises_to_next_chain_rung_when_no_evidenced_alternative() {
        // base failing, no other model observed → escalate per the operator's
        // own chain (the next rung), flagged as no-evidence for the human.
        let stats = vec![stat("reasoning", "v:base", 10, 0.40, Some(1.00))];
        let ch = chains(&[("reasoning", &["v:base", "v:ceiling"])]);
        let props = propose(&stats, &ch, &params(), &[]);
        assert_eq!(props.len(), 1);
        assert_eq!(props[0].direction, Direction::Raise);
        assert_eq!(props[0].to_model, "v:ceiling");
        assert_eq!(props[0].candidate_runs, 0);
    }

    #[test]
    fn no_change_in_the_ambiguous_middle_band() {
        // base pass-rate 0.75 — not failing (>= 0.60), not healthy (< 0.90), and
        // no materially-cheaper qualifier → leave it alone.
        let stats = vec![stat("reasoning", "v:base", 10, 0.75, Some(1.00))];
        let ch = chains(&[("reasoning", &["v:base", "v:ceiling"])]);
        assert!(propose(&stats, &ch, &params(), &[]).is_empty());
    }

    #[test]
    fn ignores_thin_samples() {
        // base failing but only 2 runs (< min_runs 5) → not enough evidence.
        let stats = vec![
            stat("reasoning", "v:base", 2, 0.0, Some(1.00)),
            stat("reasoning", "v:cheap", 2, 1.0, Some(0.40)),
        ];
        let ch = chains(&[("reasoning", &["v:base", "v:ceiling"])]);
        assert!(propose(&stats, &ch, &params(), &[]).is_empty());
    }

    // ── chain rewrite ───────────────────────────────────────────────────────

    #[test]
    fn lower_keeps_old_base_as_fallback_raise_drops_it() {
        let lower = Proposal {
            affinity: "reasoning".into(),
            direction: Direction::Lower,
            from_model: "v:base".into(),
            to_model: "v:cheap".into(),
            base_runs: 10,
            base_pass_rate: 0.95,
            base_mean_cost_usd: Some(1.0),
            candidate_runs: 10,
            candidate_pass_rate: 0.95,
            candidate_mean_cost_usd: Some(0.5),
            savings_pct: Some(0.5),
            rationale: String::new(),
        };
        // Lower: cheap first, old base retained as a higher rung.
        assert_eq!(
            apply_to_chain(&lower, &["v:base".into(), "v:ceiling".into()]),
            vec!["v:cheap", "v:base", "v:ceiling"]
        );
        let raise = Proposal {
            direction: Direction::Raise,
            to_model: "v:strong".into(),
            ..lower.clone()
        };
        // Raise: strong first, failing base dropped (never fall back to it).
        assert_eq!(
            apply_to_chain(&raise, &["v:base".into(), "v:strong".into()]),
            vec!["v:strong"]
        );
    }

    // ── aggregation ─────────────────────────────────────────────────────────

    #[test]
    fn aggregate_computes_pass_rate_and_mean_cost() {
        let obs = vec![
            StepObservation {
                affinity: "reasoning".into(),
                step: "draft".into(),
                model: "v:base".into(),
                effort: None,
                passed: true,
                cost_usd: Some(1.0),
            },
            StepObservation {
                affinity: "reasoning".into(),
                step: "review".into(),
                model: "v:base".into(),
                effort: None,
                passed: true,
                cost_usd: Some(3.0),
            },
            StepObservation {
                affinity: "reasoning".into(),
                step: "draft".into(),
                model: "v:base".into(),
                effort: None,
                passed: false,
                cost_usd: None,
            },
        ];
        let stats = aggregate(&obs);
        assert_eq!(stats.len(), 1);
        let s = &stats[0];
        assert_eq!(s.runs, 3);
        assert_eq!(s.passes, 2);
        assert!((s.pass_rate - 2.0 / 3.0).abs() < 1e-9);
        // Mean over the two priced runs: (1 + 3) / 2 = 2.0.
        assert!((s.mean_cost_usd.unwrap() - 2.0).abs() < 1e-9);
        assert_eq!(s.steps, vec!["draft", "review"]);
    }

    // ── audit correlation ───────────────────────────────────────────────────

    fn invoked(cor: &str, step: &str, affinity: &str, model: &str) -> AuditEvent {
        AuditEvent::new("agent.invoked")
            .with_correlation(cor)
            .with_payload(json!({
                "transition": step, "state": "s", "affinity": affinity,
                "model": model, "max_seconds": 60,
            }))
    }
    fn completed(cor: &str, step: &str, model: &str, cost: f64) -> AuditEvent {
        AuditEvent::new("agent.completed")
            .with_correlation(cor)
            .with_payload(json!({
                "transition": step, "duration_ms": 10, "model": model,
                "prompt_tokens": 1000, "completion_tokens": 100, "cost_usd": cost,
            }))
    }
    /// `agent.completed` carrying the applied `reasoning_effort` (#12) — the walked
    /// model + the effort it actually ran under, paired on the SAME event.
    fn completed_effort(cor: &str, step: &str, model: &str, cost: f64, effort: &str) -> AuditEvent {
        AuditEvent::new("agent.completed")
            .with_correlation(cor)
            .with_payload(json!({
                "transition": step, "duration_ms": 10, "model": model,
                "prompt_tokens": 1000, "completion_tokens": 100, "cost_usd": cost,
                "reasoning_effort": effort,
            }))
    }
    fn failed(cor: &str, step: &str) -> AuditEvent {
        AuditEvent::new("chain.failed")
            .with_correlation(cor)
            .with_payload(json!({
                "fromState": "s", "transition": step, "chainDepth": 1,
                "errorClass": "OUTPUT_TYPE_MISMATCH", "message": "bar failed",
            }))
    }

    /// A contract-FAILED `agent.model_attempt` (a non-converging model that then
    /// fell back). Carries the stable wire-code `error` prefix + the effort it ran.
    fn contract_attempt(cor: &str, model: &str, effort: &str) -> AuditEvent {
        AuditEvent::new("agent.model_attempt")
            .with_correlation(cor)
            .with_payload(json!({
                "attempt_index": 0, "model": model, "outcome": "Capability",
                "error": "AGENT_NOT_CONVERGING: emitted status:success 4x, never the contract",
                "duration_ms": 720_000, "reasoning_effort": effort,
            }))
    }

    #[test]
    fn a_contract_failed_attempt_becomes_a_negative_observation_for_that_model() {
        // The step SUCCEEDED via a fallback (deepseek completes), but glm burned
        // the contract first. The flywheel must see glm's failure, not just the win.
        let events = vec![
            invoked("cor_1", "scan", "reasoning", "glm"),
            contract_attempt("cor_1", "glm", "high"),
            completed_effort("cor_1", "scan", "deepseek", 0.11, "high"),
        ];
        let obs = observations_from_audit(&events);
        assert_eq!(
            obs.len(),
            2,
            "one failed (glm) + one passed (deepseek): {obs:?}"
        );
        let glm = obs.iter().find(|o| o.model == "glm").expect("glm observed");
        assert!(
            !glm.passed,
            "glm's contract failure is a NEGATIVE observation"
        );
        assert_eq!(glm.effort.as_deref(), Some("high"));
        assert_eq!(glm.affinity, "reasoning");
        let dsk = obs
            .iter()
            .find(|o| o.model == "deepseek")
            .expect("deepseek observed");
        assert!(dsk.passed, "the fallback that completed passed");
    }

    #[test]
    fn observations_pair_invoked_with_completed_or_failed_by_correlation() {
        let events = vec![
            // pass: invoked + completed share cor_1
            invoked("cor_1", "draft", "reasoning", "v:base"),
            completed("cor_1", "draft", "v:base", 0.29),
            // fail: invoked + chain.failed share cor_2 (no completed)
            invoked("cor_2", "review", "reasoning", "v:base"),
            failed("cor_2", "review"),
            // a non-agent event is ignored
            AuditEvent::new("workflow.started"),
        ];
        let mut obs = observations_from_audit(&events);
        obs.sort_by(|a, b| a.step.cmp(&b.step));
        assert_eq!(obs.len(), 2);

        let draft = obs.iter().find(|o| o.step == "draft").unwrap();
        assert_eq!(draft.affinity, "reasoning");
        assert_eq!(draft.model, "v:base");
        assert!(draft.passed);
        assert!((draft.cost_usd.unwrap() - 0.29).abs() < 1e-9);

        let review = obs.iter().find(|o| o.step == "review").unwrap();
        assert!(!review.passed);
        assert_eq!(review.cost_usd, None);
        assert_eq!(review.model, "v:base"); // model recovered from agent.invoked
    }

    #[test]
    fn audit_to_proposal_end_to_end() {
        // Enough correlated runs that a cheap model clears the bar and saves big.
        let mut events = Vec::new();
        for i in 0..6 {
            let c1 = format!("base_{i}");
            events.push(invoked(&c1, "draft", "reasoning", "v:base"));
            events.push(completed(&c1, "draft", "v:base", 1.00));
            let c2 = format!("cheap_{i}");
            events.push(invoked(&c2, "draft", "reasoning", "v:cheap"));
            events.push(completed(&c2, "draft", "v:cheap", 0.40));
        }
        let obs = observations_from_audit(&events);
        let stats = aggregate(&obs);
        let ch = chains(&[("reasoning", &["v:base", "v:ceiling"])]);
        let props = propose(&stats, &ch, &params(), &[]);
        assert_eq!(props.len(), 1);
        assert_eq!(props[0].direction, Direction::Lower);
        assert_eq!(props[0].to_model, "v:cheap");
    }

    // ── #12 effort-aware keying ───────────────────────────────────────────────

    #[test]
    fn same_model_at_two_efforts_yields_two_buckets() {
        // The SAME model at `medium` vs `high` must be two distinct
        // (affinity, model, effort) buckets — never lumped.
        let mut events = Vec::new();
        for i in 0..3 {
            let c = format!("med_{i}");
            events.push(invoked(&c, "draft", "reasoning", "v:base"));
            events.push(completed_effort(&c, "draft", "v:base", 1.0, "medium"));
        }
        for i in 0..3 {
            let c = format!("high_{i}");
            events.push(invoked(&c, "draft", "reasoning", "v:base"));
            events.push(completed_effort(&c, "draft", "v:base", 2.0, "high"));
        }
        let stats = aggregate(&observations_from_audit(&events));
        assert_eq!(stats.len(), 2, "two effort buckets expected: {stats:?}");
        let med = stats
            .iter()
            .find(|s| s.effort.as_deref() == Some("medium"))
            .unwrap();
        let high = stats
            .iter()
            .find(|s| s.effort.as_deref() == Some("high"))
            .unwrap();
        assert_eq!(med.runs, 3);
        assert_eq!(high.runs, 3);
        assert!((med.mean_cost_usd.unwrap() - 1.0).abs() < 1e-9);
        assert!((high.mean_cost_usd.unwrap() - 2.0).abs() < 1e-9);
    }

    #[test]
    fn escalated_pass_keys_on_walked_models_effort_not_composers() {
        // A hop that ESCALATES: `agent.invoked` records the composer's intent
        // (`v:base` @ its phase effort), but the run walked to `v:strong` and the
        // `agent.completed` records the WALKED model + the effort IT ran under
        // (`high`). The observation must key on the completed pair, never the
        // invoked one — else it mis-attributes evidence to the wrong model@effort.
        let events = vec![
            invoked("cor_esc", "draft", "reasoning", "v:base"),
            completed_effort("cor_esc", "draft", "v:strong", 3.0, "high"),
        ];
        let obs = observations_from_audit(&events);
        assert_eq!(obs.len(), 1);
        assert_eq!(obs[0].model, "v:strong", "walked model, not composer's");
        assert_eq!(obs[0].effort.as_deref(), Some("high"));
        assert!(obs[0].passed);
    }

    #[test]
    fn base_lookup_matches_on_model_and_effort_identity() {
        // Base is `v:base@high`; only the high-effort bucket clears the bar and a
        // cheaper high-effort model qualifies. The chain rung carries `@high`, so
        // the base-lookup must find the high bucket (not the medium one).
        let stats = vec![
            stat_effort("reasoning", "v:base", Some("high"), 10, 0.95, Some(1.00)),
            stat_effort("reasoning", "v:base", Some("medium"), 10, 0.40, Some(0.50)),
            stat_effort("reasoning", "v:cheap", Some("high"), 10, 0.95, Some(0.40)),
        ];
        let ch = chains(&[("reasoning", &["v:base@high", "v:ceiling"])]);
        let props = propose(&stats, &ch, &params(), &[]);
        assert_eq!(props.len(), 1, "{props:?}");
        assert_eq!(props[0].direction, Direction::Lower);
        assert_eq!(props[0].from_model, "v:base@high");
        assert_eq!(props[0].to_model, "v:cheap@high");
    }

    // ── #13 fair-trial exploration ────────────────────────────────────────────

    /// A tiny catalog: a fit, under-cap, tool-capable, locally-runnable entrant
    /// (`fit-cheap`) and an over-cap frontier model (`too-dear`). Vendor `ollama`
    /// is keyless, so `vendor_available` is true in the test env.
    fn trial_catalog() -> Vec<crate::model_catalog::ModelEntry> {
        use crate::model_resolver::AffinityScores;
        let entry = |vendor: &str, model: &str, out_usd: f64, reasoning: f64| {
            crate::model_catalog::ModelEntry {
                vendor: vendor.into(),
                model: model.into(),
                input_usd_per_million: 0.0,
                output_usd_per_million: out_usd,
                context: 0,
                intelligence: 50.0,
                speed_tps: 0.0,
                tools: true,
                reasoning_levels: vec![],
                local: true,
                scores: AffinityScores {
                    reasoning,
                    ..Default::default()
                },
            }
        };
        vec![
            entry("ollama", "fit-cheap", 1.0, 60.0), // fit + under $5/M cap
            entry("ollama", "too-dear", 9.0, 90.0),  // fit but OVER the cap
        ]
    }

    #[test]
    fn fair_trial_surfaces_a_fit_undercap_untried_entrant() {
        // Healthy, evidenced base in the ambiguous-nothing-to-exploit case; the
        // catalog holds an untried fit+under-cap model → one governed Trial.
        let stats = vec![stat("reasoning", "local:proven", 10, 0.95, Some(2.0))];
        let ch = chains(&[("reasoning", &["local:proven"])]);
        let props = propose(&stats, &ch, &params(), &trial_catalog());
        let trials: Vec<_> = props
            .iter()
            .filter(|p| p.direction == Direction::Trial)
            .collect();
        assert_eq!(trials.len(), 1, "one fair-trial expected: {props:?}");
        let t = trials[0];
        assert_eq!(t.to_model, "ollama:fit-cheap");
        // Untried → zero candidate evidence (this is what the trial gathers).
        assert_eq!(t.candidate_runs, 0);
        // The over-cap `too-dear` is NOT nominated.
        assert!(!props.iter().any(|p| p.to_model == "ollama:too-dear"));
    }

    #[test]
    fn fair_trial_skips_already_charted_or_over_cap_models() {
        // `fit-cheap` is ALREADY a chain rung, and `too-dear` is over the cap →
        // no entrant qualifies, so no Trial is surfaced.
        let stats = vec![stat("reasoning", "local:proven", 10, 0.95, Some(2.0))];
        let ch = chains(&[("reasoning", &["local:proven", "local:fit-cheap"])]);
        let props = propose(&stats, &ch, &params(), &trial_catalog());
        assert!(
            !props.iter().any(|p| p.direction == Direction::Trial),
            "no trial when the only fit model is already charted: {props:?}"
        );
    }

    #[test]
    fn fair_trial_is_governed_and_additive_never_displaces_base() {
        // A Trial's chain edit APPENDS the candidate as a new lowest rung — the
        // proven base stays first (never auto-swapped).
        let stats = vec![stat("reasoning", "local:proven", 10, 0.95, Some(2.0))];
        let ch = chains(&[("reasoning", &["local:proven"])]);
        let props = propose(&stats, &ch, &params(), &trial_catalog());
        let t = props
            .iter()
            .find(|p| p.direction == Direction::Trial)
            .unwrap();
        let old = ch.get("reasoning").unwrap();
        let new_chain = apply_to_chain(t, old);
        assert_eq!(new_chain, vec!["local:proven", "ollama:fit-cheap"]);
        // The base is untouched at the head — a Trial never displaces it.
        assert_eq!(new_chain.first().unwrap(), "local:proven");
    }

    #[test]
    fn fair_trial_disabled_with_empty_catalog() {
        // Pure-exploit when no catalog is threaded in (backward-compatible).
        let stats = vec![stat("reasoning", "local:proven", 10, 0.95, Some(2.0))];
        let ch = chains(&[("reasoning", &["local:proven"])]);
        assert!(propose(&stats, &ch, &params(), &[]).is_empty());
    }

    // ── shared model-identity normalization ───────────────────────────────────

    #[test]
    fn identity_normalizes_across_vendor_and_provider_prefixes() {
        // A catalog `vendor:model` and a chain `provider:model` for the SAME
        // underlying model-id compare equal (prefix stripped), and effort is
        // folded into the key so `model@high` != `model@medium`.
        assert!(same_identity("openai:gpt-x", "openrouter:gpt-x"));
        assert!(same_identity("v:base", "v:base"));
        assert!(!same_identity("v:base@high", "v:base@medium"));
        assert!(same_identity("v:base@high", "other-provider:base@high"));
        // identity_key strips the prefix and lower-cases the effort.
        assert_eq!(
            identity_key("provider:qwen/qwen3-coder@HIGH"),
            ("qwen/qwen3-coder".to_string(), "high".to_string())
        );
        // A vendor:model catalog entrant dedups against a provider:model chain
        // rung so it is never fair-trialed when already charted.
        let stats = vec![stat("reasoning", "prov:proven", 10, 0.95, Some(2.0))];
        // chain rung under a DIFFERENT prefix but same bare id as the catalog's
        // `local:fit-cheap`.
        let ch = chains(&[("reasoning", &["prov:proven", "other:fit-cheap"])]);
        let props = propose(&stats, &ch, &params(), &trial_catalog());
        assert!(
            !props.iter().any(|p| p.direction == Direction::Trial),
            "fit-cheap is charted under a different prefix — must dedup: {props:?}"
        );
    }
}
