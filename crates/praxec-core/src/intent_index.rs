//! The **intent index** — the learning loop that tracks which *process*
//! (template) succeeds for which *task-class*, distilled from the
//! `outcome.recorded` audit event emitted at every mission terminal. Two pure
//! layers (mirroring [`crate::deescalation`]):
//!
//! 1. [`observations_from_audit`] — pull `outcome.recorded` events and join each
//!    mission's realized cost via [`build_cost_report`](crate::cost_report)
//!    (by `workflow_id`).
//! 2. [`aggregate`] — roll observations up per `(task_class, template_id)`: run
//!    count, success-rate, mean cost, and the *evidence* count.
//!
//! Ranking/selection (which template to pick for a class) lives in the template
//! selector (a later phase); this module is the deterministic *evidence* layer
//! the selector and a `praxec intent report` read.
//!
//! **Producer ≠ evaluator / no reward-gaming.** "Success" means the mission's
//! declared `outcomes` were all met (the deterministic done-signal computed by
//! the runtime — never a model grading itself). A mission with *zero* declared
//! outcomes is "vacuously met"; such runs are counted but are **not** success
//! evidence, so a no-outcome template can't inflate its success-rate.
//!
//! Pure over `&[AuditEvent]` + `&[ModelEntry]` — unit-tested on synthetic events
//! with no store or clock, exactly like [`cost_report`](crate::cost_report).

use crate::audit::AuditEvent;
use crate::cost_report::{ReportOptions, build_cost_report};
use crate::model_catalog::ModelEntry;
use serde::Serialize;
use serde_json::Value;
use std::collections::BTreeMap;
use std::fmt::Write as _;

/// The audit event type emitted at each mission terminal (carries the outcome
/// done-signal + the process/template identity the index learns over).
pub const OUTCOME_RECORDED: &str = "outcome.recorded";

/// One mission outcome distilled from an `outcome.recorded` event, joined to the
/// mission's realized cost.
#[derive(Debug, Clone, PartialEq)]
pub struct OutcomeObservation {
    /// The process/template the mission ran (the `outcome.recorded` `template_id`,
    /// which is the workflow `definition_id`).
    pub template_id: String,
    /// The declared task-class (`process` tag), or `None` when unclassified.
    pub task_class: Option<String>,
    /// All declared outcomes' `met` checks passed (the deterministic done-signal).
    pub met: bool,
    /// `"succeeded"` | `"failed"` — the terminal mission status (evidence).
    pub terminal_status: String,
    /// Count of declared outcomes. `0` ⇒ vacuously met ⇒ not success evidence.
    pub outcomes_total: usize,
    /// Realized USD for the whole mission (summed `agent.completed` cost, joined
    /// by `workflow_id`); `None` when no priced agent step is recorded.
    pub cost_usd: Option<f64>,
}

/// Aggregate stats for one `(task_class, template_id)` pair.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct IntentStats {
    pub task_class: String,
    pub template_id: String,
    /// All terminated missions observed for this pair.
    pub runs: usize,
    /// Missions that met their outcomes (only `evidence_runs` can count here).
    pub successes: usize,
    /// `successes / evidence_runs` (0 when there's no evidence yet).
    pub success_rate: f64,
    /// Mean realized USD over the priced missions (`None` if none priced).
    pub mean_cost_usd: Option<f64>,
    /// Missions with ≥1 declared outcome — the runs that count as success
    /// evidence. The selector trusts the rate only once this clears `min_runs`.
    pub evidence_runs: usize,
}

/// The decision thresholds — data, not code (from [`tuning`](crate::tuning)).
#[derive(Debug, Clone)]
pub struct IntentParams {
    /// Minimum evidence runs before a pair's success-rate is trusted.
    pub min_runs: usize,
}

impl IntentParams {
    /// Load the thresholds from the active tuning (override-aware).
    pub fn from_tuning() -> Self {
        Self {
            min_runs: crate::tuning::tuning().intent.min_runs,
        }
    }
}

/// Build the `outcome.recorded` payload emitted at a mission terminal. The
/// schema's single source of truth — [`observations_from_audit`] reads exactly
/// these fields. `task_class` / `fail_reason` are omitted when `None`.
pub fn outcome_recorded_payload(
    template_id: &str,
    task_class: Option<&str>,
    outcomes_met: bool,
    outcomes_total: usize,
    terminal_status: &str,
    fail_reason: Option<&str>,
) -> Value {
    let mut p = serde_json::json!({
        "template_id": template_id,
        "outcomes_met": outcomes_met,
        "outcomes_total": outcomes_total,
        "terminal_status": terminal_status,
    });
    if let Some(tc) = task_class {
        p["task_class"] = Value::String(tc.to_string());
    }
    if let Some(r) = fail_reason {
        p["fail_reason"] = Value::String(r.to_string());
    }
    p
}

/// Correlate the audit into per-mission outcome observations. Each
/// `outcome.recorded` event is one terminated mission; its realized cost is
/// joined from the `agent.completed` events sharing its `workflow_id` (reusing
/// the canonical pricer in [`build_cost_report`](crate::cost_report)).
pub fn observations_from_audit(
    events: &[AuditEvent],
    models: &[ModelEntry],
) -> Vec<OutcomeObservation> {
    let str_field = |p: &Value, k: &str| p.get(k).and_then(Value::as_str).map(str::to_string);
    let mut out = Vec::new();
    for e in events {
        if e.event_type != OUTCOME_RECORDED {
            continue;
        }
        let p = &e.payload;
        // Join this mission's realized cost via the canonical pricer (filters
        // `agent.completed` by workflow_id). `None` workflow ⇒ no cost.
        let cost_usd = e.workflow_id.as_ref().and_then(|wf| {
            let report = build_cost_report(
                events,
                models,
                &ReportOptions {
                    workflow: Some(wf.clone()),
                    since: None,
                },
            );
            (report.priced_runs > 0).then_some(report.total_cost_usd)
        });
        out.push(OutcomeObservation {
            template_id: str_field(p, "template_id").unwrap_or_else(|| "(unknown)".to_string()),
            task_class: str_field(p, "task_class"),
            met: p
                .get("outcomes_met")
                .and_then(Value::as_bool)
                .unwrap_or(false),
            terminal_status: str_field(p, "terminal_status")
                .unwrap_or_else(|| "(unknown)".to_string()),
            outcomes_total: p.get("outcomes_total").and_then(Value::as_u64).unwrap_or(0) as usize,
            cost_usd,
        });
    }
    out
}

/// Roll observations up per `(task_class, template_id)`.
pub fn aggregate(observations: &[OutcomeObservation]) -> Vec<IntentStats> {
    #[derive(Default)]
    struct Acc {
        runs: usize,
        evidence_runs: usize,
        successes: usize,
        cost_sum: f64,
        priced: usize,
    }
    let mut map: BTreeMap<(String, String), Acc> = BTreeMap::new();
    for o in observations {
        let task_class = o
            .task_class
            .clone()
            .unwrap_or_else(|| "(unclassified)".to_string());
        let a = map.entry((task_class, o.template_id.clone())).or_default();
        a.runs += 1;
        // R3 — only missions with ≥1 declared outcome are success evidence; a
        // vacuously-met (0-outcome) terminal is a run but never a success.
        if o.outcomes_total > 0 {
            a.evidence_runs += 1;
            if o.met {
                a.successes += 1;
            }
        }
        if let Some(c) = o.cost_usd {
            a.cost_sum += c;
            a.priced += 1;
        }
    }
    map.into_iter()
        .map(|((task_class, template_id), a)| IntentStats {
            task_class,
            template_id,
            runs: a.runs,
            successes: a.successes,
            success_rate: if a.evidence_runs > 0 {
                a.successes as f64 / a.evidence_runs as f64
            } else {
                0.0
            },
            mean_cost_usd: if a.priced > 0 {
                Some(a.cost_sum / a.priced as f64)
            } else {
                None
            },
            evidence_runs: a.evidence_runs,
        })
        .collect()
}

/// Human-readable rendering, sorted by task-class then success-rate (desc).
pub fn render_human(stats: &[IntentStats], params: &IntentParams) -> String {
    if stats.is_empty() {
        return "No `outcome.recorded` events found — drive a mission to terminal \
                to populate the intent index.\n"
            .to_string();
    }
    let mut rows: Vec<&IntentStats> = stats.iter().collect();
    rows.sort_by(|a, b| {
        a.task_class.cmp(&b.task_class).then(
            b.success_rate
                .partial_cmp(&a.success_rate)
                .unwrap_or(std::cmp::Ordering::Equal),
        )
    });
    let mut s = String::new();
    let _ = writeln!(
        s,
        "Intent index — success / cost per (task_class, template) over {} group(s):\n",
        stats.len()
    );
    for st in rows {
        let cost = st
            .mean_cost_usd
            .map(|c| format!("${c:.4}"))
            .unwrap_or_else(|| "—".to_string());
        let thin = if st.evidence_runs < params.min_runs {
            format!("  [thin sample — < {} evidence runs]", params.min_runs)
        } else {
            String::new()
        };
        let _ = writeln!(s, "  [{}] {}", st.task_class, st.template_id);
        let _ = writeln!(
            s,
            "    {:.0}% success over {} evidence run(s) ({} total), mean {}{}",
            st.success_rate * 100.0,
            st.evidence_runs,
            st.runs,
            cost,
            thin
        );
    }
    s
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn outcome(
        wf: &str,
        template: &str,
        task_class: Option<&str>,
        met: bool,
        status: &str,
        outcomes_total: usize,
    ) -> AuditEvent {
        let mut payload = json!({
            "template_id": template,
            "outcomes_met": met,
            "terminal_status": status,
            "outcomes_total": outcomes_total,
        });
        if let Some(tc) = task_class {
            payload["task_class"] = json!(tc);
        }
        AuditEvent::new(OUTCOME_RECORDED)
            .with_workflow(wf)
            .with_payload(payload)
    }

    fn completed(wf: &str, cost: f64) -> AuditEvent {
        AuditEvent::new(crate::cost_report::AGENT_COMPLETED)
            .with_workflow(wf)
            .with_payload(json!({
                "model": "openrouter:z-ai/glm-5.2",
                "prompt_tokens": 1000,
                "completion_tokens": 200,
                "cost_usd": cost,
            }))
    }

    #[test]
    fn aggregate_rolls_up_success_rate_and_mean_cost() {
        // engineering/flow.X: 3 runs, 2 met → 66.7%; costs 0.01 + 0.03 (one run
        // unpriced) → mean of the two priced = 0.02.
        let obs = vec![
            OutcomeObservation {
                template_id: "flow.x".into(),
                task_class: Some("engineering".into()),
                met: true,
                terminal_status: "succeeded".into(),
                outcomes_total: 2,
                cost_usd: Some(0.01),
            },
            OutcomeObservation {
                template_id: "flow.x".into(),
                task_class: Some("engineering".into()),
                met: true,
                terminal_status: "succeeded".into(),
                outcomes_total: 2,
                cost_usd: Some(0.03),
            },
            OutcomeObservation {
                template_id: "flow.x".into(),
                task_class: Some("engineering".into()),
                met: false,
                terminal_status: "failed".into(),
                outcomes_total: 2,
                cost_usd: None,
            },
        ];
        let stats = aggregate(&obs);
        assert_eq!(stats.len(), 1);
        let s = &stats[0];
        assert_eq!(s.task_class, "engineering");
        assert_eq!(s.template_id, "flow.x");
        assert_eq!(s.runs, 3);
        assert_eq!(s.evidence_runs, 3);
        assert_eq!(s.successes, 2);
        assert!((s.success_rate - 2.0 / 3.0).abs() < 1e-9);
        assert!((s.mean_cost_usd.unwrap() - 0.02).abs() < 1e-9);
    }

    #[test]
    fn zero_outcome_runs_are_counted_but_not_success_evidence() {
        // A template that declares NO outcomes "succeeds" vacuously — it must not
        // be able to inflate success_rate (R3 reward-gaming guard).
        let obs = vec![
            OutcomeObservation {
                template_id: "flow.gamey".into(),
                task_class: Some("engineering".into()),
                met: true,
                terminal_status: "succeeded".into(),
                outcomes_total: 0,
                cost_usd: Some(0.001),
            },
            OutcomeObservation {
                template_id: "flow.gamey".into(),
                task_class: Some("engineering".into()),
                met: true,
                terminal_status: "succeeded".into(),
                outcomes_total: 0,
                cost_usd: Some(0.001),
            },
        ];
        let stats = aggregate(&obs);
        assert_eq!(stats.len(), 1);
        let s = &stats[0];
        assert_eq!(s.runs, 2);
        assert_eq!(s.evidence_runs, 0, "0-outcome runs are not evidence");
        assert_eq!(s.successes, 0);
        assert_eq!(s.success_rate, 0.0, "no evidence ⇒ rate 0, never 100%");
    }

    #[test]
    fn unclassified_observations_bucket_under_a_stable_key() {
        let obs = vec![OutcomeObservation {
            template_id: "flow.y".into(),
            task_class: None,
            met: true,
            terminal_status: "succeeded".into(),
            outcomes_total: 1,
            cost_usd: None,
        }];
        let stats = aggregate(&obs);
        assert_eq!(stats.len(), 1);
        assert_eq!(stats[0].task_class, "(unclassified)");
        assert_eq!(stats[0].success_rate, 1.0);
    }

    #[test]
    fn empty_events_yield_empty_no_panic() {
        let obs = observations_from_audit(&[], &[]);
        assert!(obs.is_empty());
        let stats = aggregate(&obs);
        assert!(stats.is_empty());
        let out = render_human(&stats, &IntentParams { min_runs: 5 });
        assert!(out.to_lowercase().contains("no "));
    }

    #[test]
    fn observations_pull_outcome_events_and_join_cost_by_workflow() {
        // Two missions; mission wf1 has two priced agent steps (0.01 + 0.02),
        // wf2 has none. cost_usd is recorded on the events, so an empty catalog
        // still prices them.
        let events = vec![
            outcome("wf1", "flow.x", Some("engineering"), true, "succeeded", 2),
            completed("wf1", 0.01),
            completed("wf1", 0.02),
            outcome("wf2", "flow.x", Some("engineering"), false, "failed", 2),
            // A non-outcome, non-cost event must be ignored.
            AuditEvent::new("agent.invoked").with_workflow("wf1"),
        ];
        let obs = observations_from_audit(&events, &[]);
        assert_eq!(obs.len(), 2, "one observation per outcome.recorded");
        let wf1 = obs.iter().find(|o| o.met).expect("wf1 met");
        assert_eq!(wf1.template_id, "flow.x");
        assert_eq!(wf1.task_class.as_deref(), Some("engineering"));
        assert_eq!(wf1.outcomes_total, 2);
        assert!(
            (wf1.cost_usd.unwrap() - 0.03).abs() < 1e-9,
            "summed mission cost"
        );
        let wf2 = obs.iter().find(|o| !o.met).expect("wf2 failed");
        assert_eq!(wf2.cost_usd, None, "no priced step ⇒ no cost");
    }

    #[test]
    fn payload_round_trips_through_observations() {
        // The emit-side payload builder and the read-side parser must agree on
        // the schema — build a payload, wrap it as the terminal event, and read
        // it straight back into an observation.
        let payload =
            outcome_recorded_payload("flow.x", Some("engineering"), true, 2, "succeeded", None);
        let evt = AuditEvent::new(OUTCOME_RECORDED)
            .with_workflow("wf1")
            .with_payload(payload);
        let obs = observations_from_audit(&[evt], &[]);
        assert_eq!(obs.len(), 1);
        let o = &obs[0];
        assert_eq!(o.template_id, "flow.x");
        assert_eq!(o.task_class.as_deref(), Some("engineering"));
        assert!(o.met);
        assert_eq!(o.outcomes_total, 2);
        assert_eq!(o.terminal_status, "succeeded");
    }

    #[test]
    fn payload_omits_optional_fields_when_none() {
        let payload =
            outcome_recorded_payload("flow.y", None, false, 0, "failed", Some("guard_unmet"));
        assert_eq!(payload["template_id"], "flow.y");
        assert_eq!(payload["outcomes_met"], false);
        assert_eq!(payload["outcomes_total"], 0);
        assert_eq!(payload["terminal_status"], "failed");
        assert_eq!(payload["fail_reason"], "guard_unmet");
        assert!(
            payload.get("task_class").is_none(),
            "unclassified ⇒ no task_class key"
        );
    }

    #[test]
    fn render_flags_thin_samples_below_min_runs() {
        let stats = vec![IntentStats {
            task_class: "research".into(),
            template_id: "flow.sci".into(),
            runs: 2,
            successes: 2,
            success_rate: 1.0,
            mean_cost_usd: Some(0.05),
            evidence_runs: 2,
        }];
        let out = render_human(&stats, &IntentParams { min_runs: 5 });
        assert!(out.contains("flow.sci"));
        assert!(out.contains("research"));
        assert!(
            out.to_lowercase().contains("thin"),
            "a below-min_runs sample is flagged as thin: {out}"
        );
    }
}
