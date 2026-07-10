//! Workflow test / fuzz harness — drives any praxec config with mock
//! executors and a seeded chooser, classifying each run against structural
//! invariants. See docs/architecture/workflow-test-design.md.

pub mod analysis;
pub mod assert;
pub mod chooser;
pub mod driver;
pub mod inject;
pub mod isolate;
pub mod mock;
pub mod mutate;
pub mod oracle;
pub mod perfuzz;
pub mod prng;
pub mod report;
pub mod scenario;
pub mod smartmock;
pub mod walk;

pub use analysis::plan::{OutputPlan, derive_plan};
pub use driver::{FuzzReport, fuzz_config};
pub use mutate::{MutationReport, OperatorResult, mutation_score};
pub use oracle::{RunVerdict, classify_run};
pub use perfuzz::{TransitionVerdict, fuzz_transitions};
pub use smartmock::SmartMockRegistry;

// ── Coverage report ───────────────────────────────────────────────────────────

/// Per-definition coverage result combining graph-walk and per-transition fuzz.
pub struct DefCoverage {
    pub definition_id: String,
    /// Number of edges found by the graph walk.
    pub edges: usize,
    /// States unreachable from `initialState`.
    pub orphan_states: Vec<String>,
    /// Per-transition fuzz verdicts (one per edge).
    pub verdicts: Vec<perfuzz::TransitionVerdict>,
    /// States caught in an infinite deterministic loop (no escape to a terminal
    /// or non-deterministic state via deterministic-only transitions).
    pub det_loops: Vec<String>,
}

/// Coverage report across all definitions in a config.
pub struct CoverageReport {
    pub defs: Vec<DefCoverage>,
}

impl CoverageReport {
    /// True if any definition has orphan states, any transition verdict failed,
    /// or any deterministic loops were detected.
    pub fn has_failures(&self) -> bool {
        self.defs.iter().any(|d| {
            !d.orphan_states.is_empty()
                || d.verdicts.iter().any(|v| !v.ok)
                || !d.det_loops.is_empty()
        })
    }

    /// Render a human-readable per-definition coverage summary.
    ///
    /// Example output:
    /// ```text
    /// ── my_flow — 3 edges, 0 orphan(s)
    ///   ✓ start.go
    ///   ✗ review.approve — could not satisfy guard (evidence/role?)
    /// ── other_flow — 2 edges, 1 orphan(s)
    ///   ✓ start.go
    ///   orphan states: [dead_state]
    /// ```
    pub fn render_text(&self) -> String {
        let mut out = String::new();
        for d in &self.defs {
            out.push_str(&format!(
                "── {} — {} edges, {} orphan(s)\n",
                d.definition_id,
                d.edges,
                d.orphan_states.len()
            ));
            for v in &d.verdicts {
                let mark = if v.ok { "✓" } else { "✗" };
                if v.detail.is_empty() {
                    out.push_str(&format!("  {mark} {}.{}\n", v.state, v.transition));
                } else {
                    out.push_str(&format!(
                        "  {mark} {}.{} — {}\n",
                        v.state, v.transition, v.detail
                    ));
                }
            }
            if !d.orphan_states.is_empty() {
                out.push_str(&format!("  orphan states: {:?}\n", d.orphan_states));
            }
            if !d.det_loops.is_empty() {
                out.push_str(&format!("  deterministic loop: {:?}\n", d.det_loops));
            }
        }
        out
    }
}

/// Run graph-walk + per-transition fuzz over every definition in `config_path`.
pub async fn fuzz_coverage(config_path: &std::path::Path) -> anyhow::Result<CoverageReport> {
    let (resolved, _d) = praxec_core::config::load_resolved_with_repos(config_path)?;
    let ids = praxec_core::store::ConfigDefinitionStore::from_config(&resolved).ids();
    let mut defs = Vec::new();
    for id in ids {
        let def = &resolved["workflows"][&id];
        let w = crate::walk::walk(def);
        let det_loops = crate::walk::deterministic_loops(def);
        let verdicts = crate::perfuzz::fuzz_transitions(&resolved, &id).await?;

        defs.push(DefCoverage {
            definition_id: id,
            edges: w.edges.len(),
            orphan_states: w.orphan_states,
            verdicts,
            det_loops,
        });
    }
    Ok(CoverageReport { defs })
}

// ── Scenario report ───────────────────────────────────────────────────────────

pub struct ScenarioReport {
    /// (test name, per-assertion outcomes)
    pub results: Vec<(String, Vec<assert::AssertOutcome>)>,
}

impl ScenarioReport {
    pub fn failed(&self) -> bool {
        self.results
            .iter()
            .any(|(_, os)| os.iter().any(|o| !o.passed))
    }

    pub fn render_text(&self) -> String {
        let mut out = String::new();
        for (name, outcomes) in &self.results {
            for o in outcomes {
                let mark = if o.passed { "✓" } else { "✗" };
                if o.passed {
                    out.push_str(&format!("{mark} {name} — {}\n", o.label));
                } else {
                    out.push_str(&format!("{mark} {name} — {}: {}\n", o.label, o.detail));
                }
            }
            if outcomes.is_empty() {
                out.push_str(&format!("• {name} — (no assertions)\n"));
            }
        }
        out
    }
}

pub async fn run_scenarios(
    config_path: &std::path::Path,
    scenarios_path: &std::path::Path,
) -> anyhow::Result<ScenarioReport> {
    let (resolved, _d) = praxec_core::config::load_resolved_with_repos(config_path)?;
    let text = std::fs::read_to_string(scenarios_path)?;
    let file = scenario::parse_scenarios(&text)?;
    let mut results = Vec::new();
    for t in &file.tests {
        let runs = driver::fuzz_definition(&resolved, &t.workflow, t.iterations, t.seed).await?;
        results.push((t.name.clone(), assert::evaluate(&t.expect, &runs)));
    }
    Ok(ScenarioReport { results })
}

// ── Unit tests ────────────────────────────────────────────────────────────────

#[cfg(test)]
mod coverage_tests {
    use super::*;
    use perfuzz::TransitionVerdict;

    fn make_report(orphan: bool, verdict_ok: bool) -> CoverageReport {
        CoverageReport {
            defs: vec![DefCoverage {
                definition_id: "test_flow".to_string(),
                edges: 2,
                orphan_states: if orphan {
                    vec!["dead_state".to_string()]
                } else {
                    vec![]
                },
                verdicts: vec![
                    TransitionVerdict {
                        state: "start".to_string(),
                        transition: "go".to_string(),
                        ok: true,
                        detail: String::new(),
                    },
                    TransitionVerdict {
                        state: "review".to_string(),
                        transition: "approve".to_string(),
                        ok: verdict_ok,
                        detail: if verdict_ok {
                            String::new()
                        } else {
                            "could not satisfy guard (evidence/role?)".to_string()
                        },
                    },
                ],
                det_loops: vec![],
            }],
        }
    }

    #[test]
    fn has_failures_true_when_orphan_and_failing_verdict() {
        let report = make_report(true, false);
        assert!(
            report.has_failures(),
            "should have failures with orphan + failing verdict"
        );
    }

    #[test]
    fn has_failures_true_when_only_orphan() {
        let report = make_report(true, true);
        assert!(
            report.has_failures(),
            "should have failures when orphan present"
        );
    }

    #[test]
    fn has_failures_true_when_only_failing_verdict() {
        let report = make_report(false, false);
        assert!(
            report.has_failures(),
            "should have failures when verdict fails"
        );
    }

    #[test]
    fn has_failures_false_when_clean() {
        let report = make_report(false, true);
        assert!(!report.has_failures(), "should have no failures when clean");
    }

    #[test]
    fn render_text_contains_orphan_and_failing_verdict() {
        let report = make_report(true, false);
        let text = report.render_text();
        assert!(
            text.contains("dead_state"),
            "render_text should contain orphan state name"
        );
        assert!(
            text.contains("✗"),
            "render_text should contain ✗ for failing verdict"
        );
        assert!(
            text.contains("could not satisfy guard"),
            "render_text should contain verdict detail"
        );
        assert!(
            text.contains("orphan states"),
            "render_text should label orphan states"
        );
    }

    #[test]
    fn render_text_passing_verdict_has_checkmark() {
        let report = make_report(false, true);
        let text = report.render_text();
        assert!(text.contains("✓"), "passing verdicts should have ✓");
        assert!(!text.contains("✗"), "no failures should mean no ✗");
    }
}
