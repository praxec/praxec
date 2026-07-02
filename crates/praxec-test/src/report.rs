//! Renders a FuzzReport to text and reports whether any violation occurred.

use crate::driver::FuzzReport;
use serde_json::json;

pub fn has_violations(report: &FuzzReport) -> bool {
    report
        .results
        .iter()
        .flat_map(|d| &d.scenarios)
        .any(|s| s.verdict.is_violation())
}

/// True only for EngineError-class smoke verdicts — the flow could not execute
/// at all (engine crash / start failure). This is the smoke's *gating* signal:
/// the integration smoke exists "only to ensure the whole thing can be
/// executed", so only a hard execute failure should fail the build. `Wedge` /
/// `Livelock` from a MOCK drive of an agent-heavy flow mean "the mock could not
/// navigate it to a terminal", not a workflow defect — they are advisory.
pub fn has_engine_errors(report: &FuzzReport) -> bool {
    report
        .results
        .iter()
        .flat_map(|d| &d.scenarios)
        .any(|s| matches!(s.verdict, crate::oracle::RunVerdict::EngineError(_)))
}

/// Render the integration-smoke outcome. `EngineError` → `✗` (gates the build);
/// `Wedge` / `Livelock` → `⚠` advisory (a mock chooser cannot produce valid
/// agent outputs, so it cannot drive an agent-heavy flow to completion — use
/// `--live --model` for a real-model end-to-end check). `Pass` is silent.
pub fn render_smoke(report: &FuzzReport) -> String {
    use crate::oracle::RunVerdict;
    let mut out = String::new();
    for def in &report.results {
        for s in &def.scenarios {
            match &s.verdict {
                RunVerdict::Pass => {}
                RunVerdict::EngineError(e) => out.push_str(&format!(
                    "  ✗ smoke {} — EngineError: {e}\n",
                    def.definition_id
                )),
                other => out.push_str(&format!(
                    "  ⚠ smoke {} — {other:?}: mock could not drive to completion (advisory; try --live --model)\n",
                    def.definition_id
                )),
            }
        }
    }
    out
}

pub fn render_text(report: &FuzzReport) -> String {
    let mut out = String::new();
    for def in &report.results {
        let violations: Vec<_> = def
            .scenarios
            .iter()
            .filter(|s| s.verdict.is_violation())
            .collect();
        let mark = if violations.is_empty() { "✓" } else { "✗" };
        out.push_str(&format!(
            "{mark} {} — {} scenarios, {} violations\n",
            def.definition_id,
            def.scenarios.len(),
            violations.len()
        ));
        for v in violations {
            out.push_str(&format!(
                "    {:?}  (seed {}, final status `{}`)\n",
                v.verdict, v.seed, v.final_status
            ));
        }
    }
    out.push_str(&format!(
        "\n{} transitions covered\n",
        report.transitions_covered
    ));
    out.push_str(&format!(
        "{}/{} definitions had violations\n",
        report.definitions_with_violations,
        report.results.len()
    ));
    out
}

pub fn render_json(report: &FuzzReport) -> String {
    let definitions = report
        .results
        .iter()
        .map(|def| {
            let violations: Vec<_> = def
                .scenarios
                .iter()
                .filter(|s| s.verdict.is_violation())
                .map(|s| {
                    json!({
                        "verdict": verdict_to_string(&s.verdict),
                        "seed": s.seed,
                        "final_status": s.final_status,
                    })
                })
                .collect();
            json!({
                "id": def.definition_id,
                "scenarios": def.scenarios.len(),
                "violations": violations,
            })
        })
        .collect::<Vec<_>>();

    let root = json!({
        "definitions": definitions,
        "transitions_covered": report.transitions_covered,
        "definitions_with_violations": report.definitions_with_violations,
        "has_violations": has_violations(report),
    });

    root.to_string()
}

fn verdict_to_string(verdict: &crate::oracle::RunVerdict) -> String {
    use crate::oracle::RunVerdict;
    match verdict {
        RunVerdict::Pass => "Pass".to_string(),
        RunVerdict::Wedge => "Wedge".to_string(),
        RunVerdict::Livelock => "Livelock".to_string(),
        RunVerdict::EngineError(_) => "EngineError".to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::driver::{DefResult, FuzzReport, ScenarioResult};
    use crate::oracle::RunVerdict;

    fn report(verdict: RunVerdict) -> FuzzReport {
        let is_violation = verdict.is_violation();
        FuzzReport {
            results: vec![DefResult {
                definition_id: "demo".into(),
                scenarios: vec![ScenarioResult {
                    seed: 1,
                    verdict,
                    final_status: "running".into(),
                    states_visited: vec![],
                    outcomes_met: vec![],
                }],
            }],
            transitions_covered: 3,
            definitions_with_violations: if is_violation { 1 } else { 0 },
        }
    }

    #[test]
    fn clean_report_has_no_violations() {
        let r = report(RunVerdict::Pass);
        assert!(!has_violations(&r));
        assert!(render_text(&r).contains("✓ demo"));
    }

    #[test]
    fn wedge_report_has_violations() {
        let r = report(RunVerdict::Wedge);
        assert!(has_violations(&r));
        let text = render_text(&r);
        assert!(text.contains("✗ demo"));
        assert!(text.contains("Wedge"));
    }

    #[test]
    fn wedge_is_advisory_not_an_engine_error() {
        // A mock-drive Wedge is a violation (advisory) but must NOT gate the build.
        let r = report(RunVerdict::Wedge);
        assert!(has_violations(&r), "wedge is a violation");
        assert!(!has_engine_errors(&r), "but wedge does not gate");
        assert!(render_smoke(&r).contains("⚠ smoke demo"));
        assert!(render_smoke(&r).contains("advisory"));
    }

    #[test]
    fn engine_error_gates_the_build() {
        let r = report(RunVerdict::EngineError("boom".into()));
        assert!(has_engine_errors(&r), "engine error gates");
        assert!(render_smoke(&r).contains("✗ smoke demo"));
        assert!(render_smoke(&r).contains("boom"));
    }

    #[test]
    fn pass_is_silent_in_smoke_render() {
        assert_eq!(render_smoke(&report(RunVerdict::Pass)), "");
    }

    #[test]
    fn json_is_valid_and_flags_violation() {
        let r = report(RunVerdict::Wedge);
        let s = render_json(&r);
        let v: serde_json::Value = serde_json::from_str(&s).expect("valid json");
        assert_eq!(v["has_violations"], serde_json::json!(true));
        assert_eq!(
            v["definitions"][0]["violations"][0]["verdict"],
            serde_json::json!("Wedge")
        );
    }

    #[test]
    fn json_clean_has_no_violations() {
        let r = report(RunVerdict::Pass);
        let v: serde_json::Value = serde_json::from_str(&render_json(&r)).unwrap();
        assert_eq!(v["has_violations"], serde_json::json!(false));
        assert_eq!(
            v["definitions"][0]["violations"].as_array().unwrap().len(),
            0
        );
    }
}
