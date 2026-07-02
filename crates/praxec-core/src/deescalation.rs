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
//! 2. [`aggregate`] — roll observations up per `(affinity, model)`: run count,
//!    pass-rate, mean realized cost. (Affinity is the `models.yaml` key — the
//!    unit the base actually configures; the steps it covers are evidence.)
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
    /// Cleared its independent acceptance bar (advanced) vs failed / aborted.
    pub passed: bool,
    /// Realized USD for the step (`None` on failure / uncatalogued).
    pub cost_usd: Option<f64>,
}

/// Aggregate stats for one `(affinity, model)` pair.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct ModelStats {
    pub affinity: String,
    pub model: String,
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
}

impl DeescalationParams {
    /// Load the thresholds from the active tuning (override-aware).
    pub fn from_tuning() -> Self {
        let d = &crate::tuning::tuning().deescalation;
        Self {
            min_runs: d.min_runs,
            lower_min_pass_rate: d.lower_min_pass_rate,
            raise_max_pass_rate: d.raise_max_pass_rate,
            material_savings_pct: d.material_savings_pct,
        }
    }
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
        /// (model, cost) from `agent.completed`.
        completed: Option<(String, Option<f64>)>,
        failed: bool,
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
                by_cor
                    .entry(e.correlation_id.clone())
                    .or_default()
                    .completed = Some((str_field(p, "model"), cost));
            }
            "chain.failed" => {
                by_cor.entry(e.correlation_id.clone()).or_default().failed = true;
            }
            _ => {}
        }
    }

    let mut out = Vec::new();
    for (_cor, acc) in by_cor {
        let Some((step, affinity, inv_model)) = acc.invoked else {
            continue;
        };
        // Passed iff its `agent.completed` fired; failed iff a `chain.failed`
        // fired instead; otherwise still in flight — neither, so skip.
        let (passed, model, cost) = match acc.completed {
            Some((model, cost)) => (true, model, cost),
            None if acc.failed => (false, inv_model, None),
            None => continue,
        };
        out.push(StepObservation {
            affinity,
            step,
            model,
            passed,
            cost_usd: cost,
        });
    }
    out
}

/// Roll observations up per `(affinity, model)`.
pub fn aggregate(observations: &[StepObservation]) -> Vec<ModelStats> {
    #[derive(Default)]
    struct Acc {
        runs: usize,
        passes: usize,
        cost_sum: f64,
        priced: usize,
        steps: BTreeSet<String>,
    }
    let mut map: BTreeMap<(String, String), Acc> = BTreeMap::new();
    for o in observations {
        let a = map
            .entry((o.affinity.clone(), o.model.clone()))
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
        .map(|((affinity, model), a)| ModelStats {
            affinity,
            model,
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
/// to its ordered `models.yaml` chain (base first). Returns one proposal per
/// affinity that warrants a change; affinities at a healthy, well-priced base
/// (or with a only-marginally-cheaper alternative) yield nothing.
pub fn propose(
    stats: &[ModelStats],
    current_chains: &BTreeMap<String, Vec<String>>,
    params: &DeescalationParams,
) -> Vec<Proposal> {
    let mut out = Vec::new();
    for (affinity, chain) in current_chains {
        let Some(base_model) = chain.first() else {
            continue;
        };
        let Some(base) = stats
            .iter()
            .find(|s| &s.affinity == affinity && &s.model == base_model)
        else {
            continue;
        };
        // Not enough evidence on the base to move it either way.
        if base.runs < params.min_runs {
            continue;
        }
        let candidates: Vec<&ModelStats> = stats
            .iter()
            .filter(|s| {
                &s.affinity == affinity && &s.model != base_model && s.runs >= params.min_runs
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
                    to_model: cand.model.clone(),
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
                    to_model: cand.model.clone(),
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
    }
    out
}

/// Rewrite an affinity's chain to enact a proposal: the new base goes first.
/// **Lower** keeps the old base as a higher escalation rung; **Raise** drops the
/// failing base (escalation only ratchets up).
pub fn apply_to_chain(proposal: &Proposal, old_chain: &[String]) -> Vec<String> {
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
        }
    }

    fn stat(
        affinity: &str,
        model: &str,
        runs: usize,
        pass_rate: f64,
        mean_cost: Option<f64>,
    ) -> ModelStats {
        ModelStats {
            affinity: affinity.into(),
            model: model.into(),
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
        let props = propose(&stats, &ch, &params());
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
        assert!(propose(&stats, &ch, &params()).is_empty());
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
        assert!(propose(&stats, &ch, &params()).is_empty());
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
        let props = propose(&stats, &ch, &params());
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
        let props = propose(&stats, &ch, &params());
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
        let props = propose(&stats, &ch, &params());
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
        assert!(propose(&stats, &ch, &params()).is_empty());
    }

    #[test]
    fn ignores_thin_samples() {
        // base failing but only 2 runs (< min_runs 5) → not enough evidence.
        let stats = vec![
            stat("reasoning", "v:base", 2, 0.0, Some(1.00)),
            stat("reasoning", "v:cheap", 2, 1.0, Some(0.40)),
        ];
        let ch = chains(&[("reasoning", &["v:base", "v:ceiling"])]);
        assert!(propose(&stats, &ch, &params()).is_empty());
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
                passed: true,
                cost_usd: Some(1.0),
            },
            StepObservation {
                affinity: "reasoning".into(),
                step: "review".into(),
                model: "v:base".into(),
                passed: true,
                cost_usd: Some(3.0),
            },
            StepObservation {
                affinity: "reasoning".into(),
                step: "draft".into(),
                model: "v:base".into(),
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
    fn failed(cor: &str, step: &str) -> AuditEvent {
        AuditEvent::new("chain.failed")
            .with_correlation(cor)
            .with_payload(json!({
                "fromState": "s", "transition": step, "chainDepth": 1,
                "errorClass": "OUTPUT_TYPE_MISMATCH", "message": "bar failed",
            }))
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
        let props = propose(&stats, &ch, &params());
        assert_eq!(props.len(), 1);
        assert_eq!(props[0].direction, Direction::Lower);
        assert_eq!(props[0].to_model, "v:cheap");
    }
}
