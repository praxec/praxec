//! The **value-prop savings report** — aggregates the realized cost telemetry
//! that the agent auto-drive path now records on each `agent.completed` audit
//! event ({model, prompt_tokens, completion_tokens, cost_usd}) into a per-run /
//! cross-run cost picture, and computes the **counterfactual**: what the same
//! realized tokens *would* have cost at the most-capable ("ceiling") catalog
//! model. The headline is "saved Z% vs ceiling" — the evidence that justifies
//! the chosen base model and that the de-escalation loop consumes.
//!
//! This is deliberately a pure function over `&[AuditEvent]` + `&[ModelEntry]`
//! (the catalog), so it unit-tests against synthetic events with no store or
//! clock. The CLI (`praxec cost report`) is a thin wrapper that loads the
//! audit sink's events and the active catalog, then renders this.
//!
//! Pricing reuses [`cost_usd_in`](crate::model_catalog::cost_usd_in): an
//! uncatalogued model degrades gracefully (its cost is omitted and flagged,
//! never a panic), in line with the runtime leaving `cost_usd: null`.

use crate::audit::AuditEvent;
use crate::model_catalog::{cost_usd_in, ModelEntry};
use chrono::{DateTime, Utc};
use serde::Serialize;
use serde_json::Value;
use std::cmp::Ordering;
use std::collections::BTreeMap;
use std::fmt::Write as _;

/// The audit event type that carries per-agent-step cost telemetry.
pub const AGENT_COMPLETED: &str = "agent.completed";

/// Scoping for the report: restrict to one workflow run and/or a time window.
#[derive(Debug, Clone, Default)]
pub struct ReportOptions {
    /// Only count agent steps from this workflow id (`None` ⇒ all runs).
    pub workflow: Option<String>,
    /// Only count agent steps at or after this instant (`None` ⇒ all time).
    pub since: Option<DateTime<Utc>>,
}

/// A cost rollup for one grouping key (a model string, or a step/transition).
#[derive(Debug, Clone, Serialize, PartialEq)]
pub struct GroupCost {
    pub key: String,
    pub runs: usize,
    pub prompt_tokens: u64,
    pub completion_tokens: u64,
    /// Summed realized USD across the priced runs in this group.
    pub cost_usd: f64,
    /// Runs in this group whose model wasn't catalogued (cost unknown).
    pub uncatalogued_runs: usize,
}

/// The counterfactual: the same realized tokens repriced at the ceiling model.
#[derive(Debug, Clone, Serialize, PartialEq)]
pub struct Counterfactual {
    /// The ceiling (most-capable catalogued) model, as `provider:model`.
    pub ceiling_model: String,
    /// What the comparable runs' tokens would cost at the ceiling model.
    pub ceiling_cost_usd: f64,
    /// Realized USD over the same comparable runs (apples to apples).
    pub actual_cost_usd: f64,
    /// `ceiling_cost_usd - actual_cost_usd` (positive ⇒ the base saved money).
    pub savings_usd: f64,
    /// `savings_usd / ceiling_cost_usd * 100` (0 when the ceiling cost is 0).
    pub savings_pct: f64,
}

/// The aggregated value-prop report.
#[derive(Debug, Clone, Default, Serialize, PartialEq)]
pub struct CostReport {
    /// Total agent steps in scope.
    pub runs: usize,
    /// Steps with a known realized cost.
    pub priced_runs: usize,
    /// Steps whose model wasn't catalogued (cost unknown, excluded from totals).
    pub uncatalogued_runs: usize,
    pub total_prompt_tokens: u64,
    pub total_completion_tokens: u64,
    /// Sum of realized USD over the priced runs.
    pub total_cost_usd: f64,
    /// Cost rolled up per model, most expensive first.
    pub by_model: Vec<GroupCost>,
    /// Cost rolled up per step/transition, most expensive first.
    pub by_step: Vec<GroupCost>,
    /// `None` when no ceiling model can be determined or no comparable runs.
    pub counterfactual: Option<Counterfactual>,
}

/// The **ceiling** model: the most-capable catalogued model (max `intelligence`,
/// ties broken to the lexically-greater `provider:model` for determinism). This
/// is the "what if we'd run everything on the strongest model" baseline.
pub fn ceiling_model(models: &[ModelEntry]) -> Option<&ModelEntry> {
    models.iter().max_by(|a, b| {
        match a
            .intelligence
            .partial_cmp(&b.intelligence)
            .unwrap_or(Ordering::Equal)
        {
            Ordering::Equal => a.model_string().cmp(&b.model_string()),
            other => other,
        }
    })
}

/// One agent step distilled from an `agent.completed` event.
struct Run {
    transition: String,
    model: Option<String>,
    prompt_tokens: u64,
    completion_tokens: u64,
    /// Realized USD: the recorded `cost_usd`, else recomputed from the catalog,
    /// else `None` (model uncatalogued ⇒ cost unknown).
    cost_usd: Option<f64>,
}

/// Mutable accumulator for one grouping key.
#[derive(Default)]
struct GroupAcc {
    runs: usize,
    prompt_tokens: u64,
    completion_tokens: u64,
    cost_usd: f64,
    uncatalogued_runs: usize,
}

impl GroupAcc {
    fn add(&mut self, r: &Run) {
        self.runs += 1;
        self.prompt_tokens += r.prompt_tokens;
        self.completion_tokens += r.completion_tokens;
        match r.cost_usd {
            Some(c) => self.cost_usd += c,
            None => self.uncatalogued_runs += 1,
        }
    }

    fn into_group(self, key: String) -> GroupCost {
        GroupCost {
            key,
            runs: self.runs,
            prompt_tokens: self.prompt_tokens,
            completion_tokens: self.completion_tokens,
            cost_usd: self.cost_usd,
            uncatalogued_runs: self.uncatalogued_runs,
        }
    }
}

/// Sort a grouping most-expensive-first, ties broken by key for determinism.
fn finalize(map: BTreeMap<String, GroupAcc>) -> Vec<GroupCost> {
    let mut v: Vec<GroupCost> = map.into_iter().map(|(k, a)| a.into_group(k)).collect();
    v.sort_by(|x, y| {
        y.cost_usd
            .partial_cmp(&x.cost_usd)
            .unwrap_or(Ordering::Equal)
            .then_with(|| x.key.cmp(&y.key))
    });
    v
}

/// Build the value-prop report from audit events + the pricing catalog.
pub fn build_cost_report(
    events: &[AuditEvent],
    models: &[ModelEntry],
    opts: &ReportOptions,
) -> CostReport {
    // 1. Distill the in-scope agent steps.
    let mut runs: Vec<Run> = Vec::new();
    for e in events {
        if e.event_type != AGENT_COMPLETED {
            continue;
        }
        if let Some(wf) = &opts.workflow {
            if e.workflow_id.as_deref() != Some(wf.as_str()) {
                continue;
            }
        }
        if let Some(since) = opts.since {
            if e.timestamp < since {
                continue;
            }
        }
        let p = &e.payload;
        let transition = p
            .get("transition")
            .and_then(Value::as_str)
            .unwrap_or("(unknown)")
            .to_string();
        let model = p.get("model").and_then(Value::as_str).map(str::to_string);
        let prompt_tokens = p.get("prompt_tokens").and_then(Value::as_u64).unwrap_or(0);
        let completion_tokens = p
            .get("completion_tokens")
            .and_then(Value::as_u64)
            .unwrap_or(0);
        // Prefer the realized cost the runtime recorded; if absent, reprice from
        // the catalog; if the model is uncatalogued, the cost stays unknown.
        let cost_usd = p.get("cost_usd").and_then(Value::as_f64).or_else(|| {
            model
                .as_deref()
                .and_then(|m| cost_usd_in(models, m, prompt_tokens, completion_tokens))
        });
        runs.push(Run {
            transition,
            model,
            prompt_tokens,
            completion_tokens,
            cost_usd,
        });
    }

    // 2. Aggregate totals + per-model + per-step.
    let mut report = CostReport {
        runs: runs.len(),
        ..Default::default()
    };
    let mut by_model: BTreeMap<String, GroupAcc> = BTreeMap::new();
    let mut by_step: BTreeMap<String, GroupAcc> = BTreeMap::new();
    for r in &runs {
        report.total_prompt_tokens += r.prompt_tokens;
        report.total_completion_tokens += r.completion_tokens;
        match r.cost_usd {
            Some(c) => {
                report.priced_runs += 1;
                report.total_cost_usd += c;
            }
            None => report.uncatalogued_runs += 1,
        }
        let model_key = r.model.clone().unwrap_or_else(|| "(unknown)".into());
        by_model.entry(model_key).or_default().add(r);
        by_step.entry(r.transition.clone()).or_default().add(r);
    }
    report.by_model = finalize(by_model);
    report.by_step = finalize(by_step);

    // 3. Counterfactual: reprice each comparable run (known realized cost) at the
    // ceiling model, apples-to-apples over the same set.
    if let Some(ceiling) = ceiling_model(models) {
        let ceiling_str = ceiling.model_string();
        let mut actual = 0.0_f64;
        let mut ceiling_cost = 0.0_f64;
        let mut comparable = false;
        for r in &runs {
            if let Some(ac) = r.cost_usd {
                if let Some(cc) =
                    cost_usd_in(models, &ceiling_str, r.prompt_tokens, r.completion_tokens)
                {
                    actual += ac;
                    ceiling_cost += cc;
                    comparable = true;
                }
            }
        }
        if comparable {
            let savings_usd = ceiling_cost - actual;
            let savings_pct = if ceiling_cost != 0.0 {
                savings_usd / ceiling_cost * 100.0
            } else {
                0.0
            };
            report.counterfactual = Some(Counterfactual {
                ceiling_model: ceiling_str,
                ceiling_cost_usd: ceiling_cost,
                actual_cost_usd: actual,
                savings_usd,
                savings_pct,
            });
        }
    }

    report
}

/// Render the report as a human-readable block (the non-`--json` CLI form).
pub fn render_human(r: &CostReport) -> String {
    let mut s = String::new();
    let _ = writeln!(
        s,
        "Cost report — {} agent step(s), {} priced",
        r.runs, r.priced_runs
    );
    let _ = writeln!(s, "  total realized cost: ${:.4}", r.total_cost_usd);
    let _ = writeln!(
        s,
        "  tokens: {} prompt / {} completion",
        r.total_prompt_tokens, r.total_completion_tokens
    );
    if r.uncatalogued_runs > 0 {
        let _ = writeln!(
            s,
            "  note: {} step(s) ran an uncatalogued model — cost excluded",
            r.uncatalogued_runs
        );
    }
    if let Some(cf) = &r.counterfactual {
        let _ = writeln!(
            s,
            "  counterfactual @ ceiling {}: ${:.4} → saved {:.1}% (${:.4})",
            cf.ceiling_model, cf.ceiling_cost_usd, cf.savings_pct, cf.savings_usd
        );
    }
    let _ = writeln!(s, "By model:");
    for g in &r.by_model {
        let _ = writeln!(
            s,
            "  {:<28} {:>3} run(s)  ${:.4}",
            g.key, g.runs, g.cost_usd
        );
    }
    let _ = writeln!(s, "By step:");
    for g in &r.by_step {
        let _ = writeln!(
            s,
            "  {:<28} {:>3} run(s)  ${:.4}",
            g.key, g.runs, g.cost_usd
        );
    }
    s
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model_resolver::AffinityScores;
    use serde_json::{json, Value};

    /// A catalogued model with explicit prices so the arithmetic is checkable.
    fn model(name: &str, intelligence: f64, input: f64, output: f64) -> ModelEntry {
        ModelEntry {
            vendor: "v".into(),
            model: name.into(),
            input_usd_per_million: input,
            output_usd_per_million: output,
            context: 0,
            intelligence,
            speed_tps: 50.0,
            tools: true,
            reasoning_levels: vec![],
            local: false,
            scores: AffinityScores::default(),
        }
    }

    /// An `agent.completed` event with telemetry. `cost` of `None` ⇒ the runtime
    /// left `cost_usd: null` (uncatalogued at run time).
    fn completed(
        wf: &str,
        transition: &str,
        model_str: &str,
        prompt: u64,
        completion: u64,
        cost: Option<f64>,
    ) -> AuditEvent {
        let cost_val = match cost {
            Some(c) => json!(c),
            None => Value::Null,
        };
        AuditEvent::new(AGENT_COMPLETED)
            .with_workflow(wf)
            .with_payload(json!({
                "transition": transition,
                "model": model_str,
                "prompt_tokens": prompt,
                "completion_tokens": completion,
                "cost_usd": cost_val,
            }))
    }

    #[test]
    fn aggregates_total_and_by_model_and_by_step() {
        // Two models, three steps. v:base priced 1/3 per-million; v:ceiling 10/30.
        let models = vec![
            model("base", 56.0, 1.0, 3.0),
            model("ceiling", 80.0, 10.0, 30.0),
        ];
        // run A: base on "draft"   — 1M prompt, 1M completion → 1 + 3 = 4.00
        // run B: base on "review"  — 0.5M prompt, 0.2M comp   → 0.5 + 0.6 = 1.10
        // run C: ceiling on "draft"— 1M prompt, 1M completion → 10 + 30 = 40.00
        let events = vec![
            completed("wf1", "draft", "v:base", 1_000_000, 1_000_000, Some(4.00)),
            completed("wf1", "review", "v:base", 500_000, 200_000, Some(1.10)),
            completed(
                "wf1",
                "draft",
                "v:ceiling",
                1_000_000,
                1_000_000,
                Some(40.00),
            ),
        ];

        let r = build_cost_report(&events, &models, &ReportOptions::default());

        assert_eq!(r.runs, 3);
        assert_eq!(r.priced_runs, 3);
        assert_eq!(r.uncatalogued_runs, 0);
        assert!(
            (r.total_cost_usd - 45.10).abs() < 1e-9,
            "total {}",
            r.total_cost_usd
        );
        assert_eq!(r.total_prompt_tokens, 2_500_000);
        assert_eq!(r.total_completion_tokens, 2_200_000);

        // by_model: v:ceiling (40.00) before v:base (5.10), most expensive first.
        assert_eq!(r.by_model.len(), 2);
        assert_eq!(r.by_model[0].key, "v:ceiling");
        assert!((r.by_model[0].cost_usd - 40.00).abs() < 1e-9);
        assert_eq!(r.by_model[0].runs, 1);
        assert_eq!(r.by_model[1].key, "v:base");
        assert!((r.by_model[1].cost_usd - 5.10).abs() < 1e-9);
        assert_eq!(r.by_model[1].runs, 2);

        // by_step: draft (4 + 40 = 44.00) before review (1.10).
        assert_eq!(r.by_step.len(), 2);
        assert_eq!(r.by_step[0].key, "draft");
        assert!((r.by_step[0].cost_usd - 44.00).abs() < 1e-9);
        assert_eq!(r.by_step[0].runs, 2);
        assert_eq!(r.by_step[1].key, "review");
        assert!((r.by_step[1].cost_usd - 1.10).abs() < 1e-9);
    }

    #[test]
    fn counterfactual_saves_vs_ceiling() {
        // base = 1/3 per-million; ceiling = 10/30 per-million.
        let models = vec![
            model("base", 56.0, 1.0, 3.0),
            model("ceiling", 80.0, 10.0, 30.0),
        ];
        // Two base runs, each 1M/1M → realized 4.00 each, actual 8.00.
        // Ceiling for the same tokens: 10 + 30 = 40 each → 80.00.
        // Saved (80 - 8) / 80 = 90%.
        let events = vec![
            completed("wf1", "draft", "v:base", 1_000_000, 1_000_000, Some(4.00)),
            completed("wf1", "review", "v:base", 1_000_000, 1_000_000, Some(4.00)),
        ];

        let cf = build_cost_report(&events, &models, &ReportOptions::default())
            .counterfactual
            .expect("counterfactual present");
        assert_eq!(cf.ceiling_model, "v:ceiling");
        assert!(
            (cf.actual_cost_usd - 8.00).abs() < 1e-9,
            "actual {}",
            cf.actual_cost_usd
        );
        assert!(
            (cf.ceiling_cost_usd - 80.00).abs() < 1e-9,
            "ceiling {}",
            cf.ceiling_cost_usd
        );
        assert!((cf.savings_usd - 72.00).abs() < 1e-9);
        assert!(
            (cf.savings_pct - 90.0).abs() < 1e-9,
            "pct {}",
            cf.savings_pct
        );
    }

    #[test]
    fn uncatalogued_model_is_flagged_not_crashed() {
        let models = vec![
            model("base", 56.0, 1.0, 3.0),
            model("ceiling", 80.0, 10.0, 30.0),
        ];
        // One priced base run; one step on an uncatalogued model with null cost.
        let events = vec![
            completed("wf1", "draft", "v:base", 1_000_000, 1_000_000, Some(4.00)),
            completed("wf1", "draft", "v:mystery", 1_000_000, 1_000_000, None),
        ];

        let r = build_cost_report(&events, &models, &ReportOptions::default());
        assert_eq!(r.runs, 2);
        assert_eq!(r.priced_runs, 1);
        assert_eq!(r.uncatalogued_runs, 1);
        // Only the priced run contributes to the total — never a NaN/panic.
        assert!((r.total_cost_usd - 4.00).abs() < 1e-9);
        // The uncatalogued model still appears, flagged, with zero known cost.
        let mystery = r.by_model.iter().find(|g| g.key == "v:mystery").unwrap();
        assert_eq!(mystery.runs, 1);
        assert_eq!(mystery.uncatalogued_runs, 1);
        assert!((mystery.cost_usd - 0.0).abs() < 1e-9);
        // Counterfactual compares only the priced run: actual 4 vs ceiling 40.
        let cf = r.counterfactual.expect("counterfactual present");
        assert!((cf.actual_cost_usd - 4.00).abs() < 1e-9);
        assert!((cf.ceiling_cost_usd - 40.00).abs() < 1e-9);
        assert!((cf.savings_pct - 90.0).abs() < 1e-9);
    }

    #[test]
    fn recomputes_cost_when_runtime_left_it_null_but_model_is_catalogued() {
        // The harness logs predating telemetry leave cost_usd null; if the model
        // is catalogued we can still reprice the realized tokens.
        let models = vec![
            model("base", 56.0, 1.0, 3.0),
            model("ceiling", 80.0, 10.0, 30.0),
        ];
        let events = vec![completed(
            "wf1", "draft", "v:base", 1_000_000, 1_000_000, None,
        )];
        let r = build_cost_report(&events, &models, &ReportOptions::default());
        assert_eq!(r.priced_runs, 1);
        assert_eq!(r.uncatalogued_runs, 0);
        assert!(
            (r.total_cost_usd - 4.00).abs() < 1e-9,
            "total {}",
            r.total_cost_usd
        );
    }

    #[test]
    fn filters_by_workflow_and_since() {
        let models = vec![model("base", 56.0, 1.0, 3.0)];
        let mk = |wf: &str, when: &str| {
            let mut e = completed(wf, "draft", "v:base", 1_000_000, 1_000_000, Some(4.00));
            e.timestamp = when.parse::<DateTime<Utc>>().unwrap();
            e
        };
        let events = vec![
            mk("wf1", "2026-06-20T00:00:00Z"),
            mk("wf2", "2026-06-21T00:00:00Z"),
            mk("wf1", "2026-06-22T00:00:00Z"),
        ];

        // Scope to wf1 → 2 runs.
        let by_wf = build_cost_report(
            &events,
            &models,
            &ReportOptions {
                workflow: Some("wf1".into()),
                since: None,
            },
        );
        assert_eq!(by_wf.runs, 2);
        assert!((by_wf.total_cost_usd - 8.00).abs() < 1e-9);

        // Scope to since 06-21 → 2 runs (wf2 06-21 + wf1 06-22).
        let by_since = build_cost_report(
            &events,
            &models,
            &ReportOptions {
                workflow: None,
                since: Some("2026-06-21T00:00:00Z".parse().unwrap()),
            },
        );
        assert_eq!(by_since.runs, 2);
    }

    #[test]
    fn ignores_non_agent_completed_events() {
        let models = vec![model("base", 56.0, 1.0, 3.0)];
        let events = vec![
            AuditEvent::new("workflow.started"),
            AuditEvent::new("chain.failed"),
            completed("wf1", "draft", "v:base", 1_000_000, 1_000_000, Some(4.00)),
        ];
        let r = build_cost_report(&events, &models, &ReportOptions::default());
        assert_eq!(r.runs, 1);
    }

    #[test]
    fn ceiling_model_picks_max_intelligence_with_lexical_tiebreak() {
        let models = vec![
            model("low", 56.0, 1.0, 3.0),
            model("m-high", 80.0, 5.0, 5.0),
            model("z-high", 80.0, 9.0, 9.0),
        ];
        // Tie at 80 → lexically-greater model_string wins ("v:z-high").
        assert_eq!(ceiling_model(&models).unwrap().model, "z-high");
        assert!(ceiling_model(&[]).is_none());
    }

    #[test]
    fn render_human_surfaces_the_headline_savings() {
        let models = vec![
            model("base", 56.0, 1.0, 3.0),
            model("ceiling", 80.0, 10.0, 30.0),
        ];
        let events = vec![
            completed("wf1", "draft", "v:base", 1_000_000, 1_000_000, Some(4.00)),
            completed("wf1", "review", "v:base", 1_000_000, 1_000_000, Some(4.00)),
        ];
        let text = render_human(&build_cost_report(
            &events,
            &models,
            &ReportOptions::default(),
        ));
        assert!(text.contains("saved 90.0%"), "missing headline:\n{text}");
        assert!(text.contains("v:ceiling"));
        assert!(text.contains("v:base"));
        assert!(text.contains("draft"));
    }

    #[test]
    fn empty_events_yield_empty_report_no_panic() {
        let models = vec![model("base", 56.0, 1.0, 3.0)];
        let r = build_cost_report(&[], &models, &ReportOptions::default());
        assert_eq!(r.runs, 0);
        assert_eq!(r.priced_runs, 0);
        assert!((r.total_cost_usd - 0.0).abs() < f64::EPSILON);
        assert!(r.counterfactual.is_none());
    }
}
