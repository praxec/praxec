//! ADR-0008 — a mission's **resolution status** and the derivation that
//! computes it from the instance state + its outcomes.
//!
//! A mission is one running workflow instance. Its status lives on two axes
//! folded into one honest enum: **in process** (`running` / `waiting`) and
//! **resolved** (`succeeded` / `failed`). This replaces the prior untyped
//! grab-bag (`started`, `executed`, `waiting_for_action`, `waiting_on_lock`,
//! `completed`, `failed`, `cancelled`, `timed_out`) that conflated the two and
//! collapsed every terminal to a success-shaped `"completed"`.

use serde_json::{json, Value};

/// Why a mission `failed`. Kept as a *reason* on `Failed` (not a peer status) so
/// the top-level enum stays at four and the cockpit badges four colors.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FailReason {
    /// An operator aborted the mission (an external command).
    Cancelled,
    /// The mission exceeded a deadline (`onTimeout` / executor timeout).
    TimedOut,
    /// Resolved without its outcomes met — reached a `failure` terminal, or a
    /// `success` terminal whose outcome checks did not all pass (poka-yoke), or
    /// ran out of legal moves with outcomes still unmet.
    GuardUnmet,
    /// A step errored — a deterministic chain or an executor failed (the
    /// `error` slot carries the specific code, e.g. `CHAIN_FAILED` /
    /// `EXECUTOR_FAILED`). Distinct from a *deliberate* failure terminal.
    Error,
}

impl FailReason {
    pub fn as_str(self) -> &'static str {
        match self {
            FailReason::Cancelled => "cancelled",
            FailReason::TimedOut => "timed_out",
            FailReason::GuardUnmet => "guard_unmet",
            FailReason::Error => "error",
        }
    }
}

/// A mission's resolution status (ADR-0008).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MissionStatus {
    /// In process, advancing on its own (an executor/agent owns the next move).
    Running,
    /// In process but stalled on input — a human gate, a lock, or an external
    /// answer. The cockpit's "Needs You" at the mission level.
    Waiting,
    /// Reached a `success` terminal with all outcomes met.
    Succeeded,
    /// Reached a failure resolution; see [`FailReason`].
    Failed(FailReason),
}

impl MissionStatus {
    pub fn as_str(self) -> &'static str {
        match self {
            MissionStatus::Running => "running",
            MissionStatus::Waiting => "waiting",
            MissionStatus::Succeeded => "succeeded",
            MissionStatus::Failed(_) => "failed",
        }
    }

    /// The fail reason, if any.
    pub fn reason(self) -> Option<FailReason> {
        match self {
            MissionStatus::Failed(r) => Some(r),
            _ => None,
        }
    }

    /// Resolved — the mission has reached a terminal outcome and will not advance.
    pub fn is_resolved(self) -> bool {
        matches!(self, MissionStatus::Succeeded | MissionStatus::Failed(_))
    }

    /// The `result` object slot: `{ "status": .. }` plus `"reason"` when failed.
    pub fn to_result(self) -> Value {
        let mut m = json!({ "status": self.as_str() });
        if let Some(r) = self.reason() {
            m["reason"] = json!(r.as_str());
        }
        m
    }
}

/// The `outcome:` marker on a terminal state (ADR-0008).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TerminalOutcome {
    Success,
    Failure,
}

impl TerminalOutcome {
    /// Parse the wire token; `None` for an unmarked terminal (legacy `terminal:
    /// true` with no `outcome:` — treated as success when reached).
    pub fn from_token(s: &str) -> Option<Self> {
        match s {
            "success" => Some(TerminalOutcome::Success),
            "failure" => Some(TerminalOutcome::Failure),
            _ => None,
        }
    }
}

/// The transient signal a response builder passes in — *what just happened* in
/// this call — which the derivation folds together with the instance state.
/// Typed (poka-yoke) so the response call sites can't drift to ad-hoc strings.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StatusHint {
    /// A submit / chain advanced successfully (the instance may now be terminal).
    Started,
    /// A single transition executed (non-terminal continuation).
    Executed,
    /// A submit was rejected (guard / permission / not-your-move); the instance
    /// is unchanged at a non-terminal position. The error rides the `error` slot.
    Rejected,
    /// A step / executor / chain failed; the instance sits at a recoverable
    /// (non-terminal) position. The error rides the `error` slot — the mission
    /// is not itself resolved as `failed` (that's reserved for terminal/cancel/
    /// timeout), it is still in process awaiting recovery.
    Failed,
    /// The instance was cancelled by an operator.
    Cancelled,
    /// The instance hit `onTimeout`.
    TimedOut,
    /// At rest, awaiting the next principal action.
    WaitingForAction,
    /// Suspended waiting for a repo lock to free.
    WaitingOnLock,
    /// Suspended waiting for a spawned sub-workflow to terminate — `Waiting`
    /// regardless of who owns the parent state.
    WaitingOnSubworkflow,
}

/// Derive the mission status (ADR-0008) from the transient hint plus the
/// instance's resolved-ness and its outcomes.
///
/// - `is_terminal` — the current state is `terminal: true`.
/// - `terminal_outcome` — the terminal's `outcome:` marker (`None` = unmarked).
/// - `outcomes_met` — all declared outcome `check`s evaluate true (vacuously
///   true when none are declared).
/// - `awaiting_human` — the current (non-terminal) state hands the next move to
///   a human (`actor: human`), so the mission is `waiting`, not `running`.
pub fn derive_mission_status(
    hint: StatusHint,
    is_terminal: bool,
    terminal_outcome: Option<TerminalOutcome>,
    outcomes_met: bool,
    awaiting_human: bool,
) -> MissionStatus {
    match hint {
        // Cancellation / timeout / a step error resolve the mission's last
        // action as failed regardless of the (recoverable) instance position —
        // a sub-mission consumer detects failure on this signal, not by polling.
        StatusHint::Cancelled => MissionStatus::Failed(FailReason::Cancelled),
        StatusHint::TimedOut => MissionStatus::Failed(FailReason::TimedOut),
        StatusHint::Failed => MissionStatus::Failed(FailReason::Error),
        _ if is_terminal => match terminal_outcome {
            Some(TerminalOutcome::Failure) => MissionStatus::Failed(FailReason::GuardUnmet),
            // The poka-yoke: a success terminal only earns `succeeded` when its
            // outcomes actually hold — otherwise the run resolved short.
            Some(TerminalOutcome::Success) if outcomes_met => MissionStatus::Succeeded,
            Some(TerminalOutcome::Success) => MissionStatus::Failed(FailReason::GuardUnmet),
            // Unmarked terminal (legacy `terminal: true`): success.
            None => MissionStatus::Succeeded,
        },
        // Non-terminal: in process. Waiting iff stalled on a lock or a human.
        StatusHint::WaitingOnLock => MissionStatus::Waiting,
        StatusHint::WaitingOnSubworkflow => MissionStatus::Waiting,
        _ if awaiting_human => MissionStatus::Waiting,
        _ => MissionStatus::Running,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use rstest::rstest;
    use FailReason::{Cancelled, Error, GuardUnmet, TimedOut};
    use MissionStatus::{Failed, Running, Succeeded, Waiting};
    use StatusHint as H;
    use TerminalOutcome::{Failure, Success};

    // G1 — the full status-derivation truth table (testing-strategy). Each row is
    // an equivalence class or a key interaction; each compiles to its own test.
    #[rstest]
    // cancellation / timeout / step-error resolve regardless of position:
    #[case(H::Cancelled, false, None, true, false, Failed(Cancelled))]
    #[case(H::Cancelled, true, Some(Success), true, false, Failed(Cancelled))] // overrides a success terminal
    #[case(H::TimedOut, false, None, true, false, Failed(TimedOut))]
    #[case(H::TimedOut, true, Some(Success), true, false, Failed(TimedOut))]
    #[case(H::Failed, false, None, false, false, Failed(Error))]
    // terminal resolution by outcome marker:
    #[case(H::Started, true, Some(Failure), true, false, Failed(GuardUnmet))]
    #[case(H::Started, true, Some(Success), true, false, Succeeded)]
    #[case(H::Started, true, Some(Success), false, false, Failed(GuardUnmet))] // poka-yoke: success terminal, unmet outcome
    #[case(H::Started, true, None, true, false, Succeeded)] // unmarked terminal = legacy success
    // non-terminal: running vs waiting:
    #[case(H::WaitingForAction, false, None, false, false, Running)]
    #[case(H::WaitingForAction, false, None, false, true, Waiting)] // human owns the next move
    #[case(H::WaitingOnLock, false, None, false, false, Waiting)] // blocked on a lock
    #[case(H::WaitingOnSubworkflow, false, None, false, false, Waiting)] // parked on a child, even from a non-human state
    #[case(H::Executed, false, None, false, false, Running)]
    #[case(H::Rejected, false, None, false, false, Running)] // a rejected move stays in process
    fn status_derivation(
        #[case] hint: StatusHint,
        #[case] terminal: bool,
        #[case] outcome: Option<TerminalOutcome>,
        #[case] met: bool,
        #[case] human: bool,
        #[case] expected: MissionStatus,
    ) {
        assert_eq!(
            derive_mission_status(hint, terminal, outcome, met, human),
            expected
        );
    }

    #[test]
    fn success_terminal_with_outcomes_met_succeeds() {
        let s = derive_mission_status(
            StatusHint::Started,
            true,
            Some(TerminalOutcome::Success),
            true,
            false,
        );
        assert_eq!(s, MissionStatus::Succeeded);
        assert_eq!(s.to_result(), json!({ "status": "succeeded" }));
    }

    #[test]
    fn success_terminal_with_unmet_outcomes_fails_guard_unmet() {
        // The poka-yoke — reaching a success terminal does not make it true.
        let s = derive_mission_status(
            StatusHint::Started,
            true,
            Some(TerminalOutcome::Success),
            false,
            false,
        );
        assert_eq!(s, MissionStatus::Failed(FailReason::GuardUnmet));
        assert_eq!(
            s.to_result(),
            json!({ "status": "failed", "reason": "guard_unmet" })
        );
    }

    #[test]
    fn failure_terminal_fails() {
        let s = derive_mission_status(
            StatusHint::Started,
            true,
            Some(TerminalOutcome::Failure),
            true,
            false,
        );
        assert_eq!(s, MissionStatus::Failed(FailReason::GuardUnmet));
    }

    #[test]
    fn unmarked_terminal_is_legacy_success() {
        let s = derive_mission_status(StatusHint::Started, true, None, true, false);
        assert_eq!(s, MissionStatus::Succeeded);
    }

    #[test]
    fn cancellation_and_timeout_win_over_position() {
        assert_eq!(
            derive_mission_status(StatusHint::Cancelled, false, None, true, false),
            MissionStatus::Failed(FailReason::Cancelled)
        );
        assert_eq!(
            derive_mission_status(
                StatusHint::TimedOut,
                true,
                Some(TerminalOutcome::Success),
                true,
                false
            ),
            MissionStatus::Failed(FailReason::TimedOut)
        );
    }

    #[test]
    fn step_failure_resolves_as_error() {
        // A chain/executor error surfaces as failed{error} even though the
        // instance sits at a recoverable non-terminal position — so a parent
        // mission detects it instead of polling forever.
        let s = derive_mission_status(StatusHint::Failed, false, None, false, false);
        assert_eq!(s, MissionStatus::Failed(FailReason::Error));
        assert_eq!(
            s.to_result(),
            json!({ "status": "failed", "reason": "error" })
        );
    }

    #[test]
    fn non_terminal_running_vs_waiting() {
        // Agent owns the next move → running.
        assert_eq!(
            derive_mission_status(StatusHint::WaitingForAction, false, None, false, false),
            MissionStatus::Running
        );
        // Human gate → waiting.
        assert_eq!(
            derive_mission_status(StatusHint::WaitingForAction, false, None, false, true),
            MissionStatus::Waiting
        );
        // Lock → waiting.
        assert_eq!(
            derive_mission_status(StatusHint::WaitingOnLock, false, None, false, false),
            MissionStatus::Waiting
        );
    }
}
