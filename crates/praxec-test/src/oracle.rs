//! Classifies one drive into a structural verdict. P0 invariants only:
//! crashes, wedges (no legal move, not resolved, not a human park), and
//! livelocks (never terminated within the step budget).

use praxec_agents::orchestrator::{DriveOutcome, MissionState};

#[derive(Debug, PartialEq, Eq)]
pub enum RunVerdict {
    /// Reached a terminal resolution, or legally parked at a human gate.
    Pass,
    /// No legal move remained while non-terminal and not a human park.
    Wedge,
    /// Never reached a terminal within the step budget.
    Livelock,
    /// The engine returned an error.
    EngineError(String),
}

impl RunVerdict {
    pub fn is_violation(&self) -> bool {
        !matches!(self, RunVerdict::Pass)
    }
}

/// A synthetic "unknown" state for the rare case the final re-query fails.
pub fn oracle_unknown_state(mission_id: &str) -> MissionState {
    MissionState {
        mission_id: mission_id.to_string(),
        status: "running".to_string(),
        reason: None,
        version: 0,
        goal: None,
        outcomes: vec![],
        legal_actions: vec![],
    }
}

pub fn classify_run(outcome: &DriveOutcome, final_state: &MissionState) -> RunVerdict {
    match outcome {
        DriveOutcome::Resolved { .. } => RunVerdict::Pass,
        DriveOutcome::Declined => RunVerdict::Pass,
        DriveOutcome::MaxSteps => RunVerdict::Livelock,
        DriveOutcome::Error(e) => RunVerdict::EngineError(e.clone()),
        DriveOutcome::GaveUp => {
            if final_state.resolved() || final_state.human_turn() {
                RunVerdict::Pass
            } else {
                RunVerdict::Wedge
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use praxec_agents::orchestrator::LegalAction;

    fn st(status: &str, actions: Vec<(&str, &str)>) -> MissionState {
        MissionState {
            mission_id: "wf".into(),
            status: status.into(),
            reason: None,
            version: 1,
            goal: None,
            outcomes: vec![],
            legal_actions: actions
                .into_iter()
                .map(|(t, a)| LegalAction {
                    transition: t.into(),
                    actor: a.into(),
                })
                .collect(),
        }
    }

    #[test]
    fn resolved_passes() {
        let v = classify_run(
            &DriveOutcome::Resolved {
                status: "succeeded".into(),
                reason: None,
            },
            &st("succeeded", vec![]),
        );
        assert_eq!(v, RunVerdict::Pass);
    }

    #[test]
    fn maxsteps_is_livelock() {
        assert_eq!(
            classify_run(&DriveOutcome::MaxSteps, &st("running", vec![])),
            RunVerdict::Livelock
        );
    }

    #[test]
    fn gaveup_nonterminal_is_wedge() {
        assert_eq!(
            classify_run(&DriveOutcome::GaveUp, &st("running", vec![])),
            RunVerdict::Wedge
        );
    }

    #[test]
    fn gaveup_at_human_gate_passes() {
        let v = classify_run(
            &DriveOutcome::GaveUp,
            &st("waiting", vec![("approve", "human")]),
        );
        assert_eq!(v, RunVerdict::Pass);
    }

    #[test]
    fn engine_error_is_violation() {
        let v = classify_run(&DriveOutcome::Error("boom".into()), &st("running", vec![]));
        assert!(v.is_violation());
    }

    #[test]
    fn gaveup_at_resolved_terminal_passes() {
        // GaveUp at a succeeded terminal is a legal stop, not a wedge.
        let v = classify_run(&DriveOutcome::GaveUp, &st("succeeded", vec![]));
        assert_eq!(v, RunVerdict::Pass);
    }
}
