//! Evaluates an `Expect` against the runs of one workflow.

use crate::driver::ScenarioResult;
use crate::scenario::Expect;

#[derive(Debug, PartialEq)]
pub struct AssertOutcome {
    pub label: String,
    pub passed: bool,
    pub detail: String,
}

/// Evaluate every clause in `expect` against the `runs` of one workflow.
pub fn evaluate(expect: &Expect, runs: &[ScenarioResult]) -> Vec<AssertOutcome> {
    let mut outcomes = Vec::new();

    // Evaluate reaches: passed if at least one run visited the state
    for s in &expect.reaches {
        let passed = runs.iter().any(|r| r.states_visited.iter().any(|v| v == s));
        let detail = if passed {
            format!("found {s} in at least one run")
        } else {
            format!("no run visited {s}")
        };
        outcomes.push(AssertOutcome {
            label: format!("reaches:{s}"),
            passed,
            detail,
        });
    }

    // Evaluate never_reaches: passed if no run visited the state
    for s in &expect.never_reaches {
        let passed = !runs.iter().any(|r| r.states_visited.iter().any(|v| v == s));
        let detail = if passed {
            format!("no run visited {s}")
        } else {
            format!("{s} was reached")
        };
        outcomes.push(AssertOutcome {
            label: format!("never_reaches:{s}"),
            passed,
            detail,
        });
    }

    // Evaluate final_state: passed if at least one run ended in this state
    for s in &expect.final_state {
        let passed = runs
            .iter()
            .any(|r| r.states_visited.last().map(|l| l == s).unwrap_or(false));
        let detail = if passed {
            format!("found final state {s}")
        } else {
            format!("no run ended in {s}")
        };
        outcomes.push(AssertOutcome {
            label: format!("final_state:{s}"),
            passed,
            detail,
        });
    }

    // Evaluate outcome_met: passed if at least one run met the outcome
    for s in &expect.outcome_met {
        let passed = runs.iter().any(|r| r.outcomes_met.iter().any(|o| o == s));
        let detail = if passed {
            format!("outcome {s} was met")
        } else {
            format!("outcome {s} never met")
        };
        outcomes.push(AssertOutcome {
            label: format!("outcome_met:{s}"),
            passed,
            detail,
        });
    }

    outcomes
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::oracle::RunVerdict;

    fn run(states: &[&str], outcomes: &[&str]) -> ScenarioResult {
        ScenarioResult {
            seed: 0,
            verdict: RunVerdict::Pass,
            final_status: "succeeded".into(),
            states_visited: states.iter().map(|s| s.to_string()).collect(),
            outcomes_met: outcomes.iter().map(|s| s.to_string()).collect(),
        }
    }

    #[test]
    fn reaches_and_never_reaches() {
        let runs = vec![run(&["start", "checked"], &[]), run(&["start"], &[])];
        let e = Expect {
            reaches: vec!["checked".into()],
            never_reaches: vec!["shipped".into()],
            final_state: vec![],
            outcome_met: vec![],
        };
        let out = evaluate(&e, &runs);
        assert!(out.iter().all(|a| a.passed), "{out:?}");
    }

    #[test]
    fn never_reaches_fails_when_reached() {
        let runs = vec![run(&["start", "shipped"], &[])];
        let e = Expect {
            never_reaches: vec!["shipped".into()],
            ..Default::default()
        };
        let out = evaluate(&e, &runs);
        assert!(out
            .iter()
            .any(|a| !a.passed && a.label.contains("never_reaches")));
    }

    #[test]
    fn final_state_checks_last_visited() {
        let runs = vec![run(&["start", "done"], &[])];
        let e = Expect {
            final_state: vec!["done".into()],
            ..Default::default()
        };
        assert!(evaluate(&e, &runs).iter().all(|a| a.passed));
        let e2 = Expect {
            final_state: vec!["other".into()],
            ..Default::default()
        };
        assert!(evaluate(&e2, &runs).iter().any(|a| !a.passed));
    }

    #[test]
    fn outcome_met_checks_any_run() {
        let runs = vec![run(&["start"], &["approved"])];
        let e = Expect {
            outcome_met: vec!["approved".into()],
            ..Default::default()
        };
        assert!(evaluate(&e, &runs).iter().all(|a| a.passed));
    }
}
