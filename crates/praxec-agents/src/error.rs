//! Typed wire codes for the agent executor's failure classes.
//!
//! Kept in this crate (not `praxec-core`) so core stays free of agentic
//! logic. Surfaced as `ExecutorError::Permanent("<CODE>: <context>")`, matching
//! the existing coded-Permanent convention (e.g. `INVALID_PARALLEL_CONFIG`,
//! `WORKFLOW_DEPTH_EXCEEDED`). Wire codes are operator-facing and MUST stay
//! stable across releases.

use praxec_core::error::ExecutorError;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AgentErrorCode {
    /// Config failed to deserialize (unknown field, missing/empty `goal`, …).
    ConfigParse,
    /// Neither or both of `agent`/`affinity` set (must be exactly one).
    InvalidModelBinding,
    /// A `tools` entry names the praxec-self connection (re-entrancy, FM5).
    ForbiddenSelfTool,
    /// Subprocess exited without producing any result envelope (FM1).
    NoResult,
    /// The agent's final message was present but not a valid result envelope (FM1/FM2).
    MalformedResult,
    /// `final_answer` reported a non-`success` status (FM1).
    ResultFailed,
    /// `success` result's `output` is not a structured object (FM12).
    OutputIncomplete,
    /// A skill declared in scope is absent from the snapshot `_skillsLibrary`.
    SkillSubjectUnknown,
    /// A scoped skill's body is missing/empty.
    SkillBodyMissing,
    /// Could not acquire the `owned_files` lock within the bound (FM13).
    FileLockTimeout,
    /// The `aether` binary is absent / not runnable (FM7).
    BinaryMissing,
    /// Subprocess emitted an unrecognized stdout event shape (FM7).
    EventShapeDrift,
    /// Subprocess exited non-zero / was signal-killed (panic/OOM/crash) — its
    /// output must NOT be reported as a success (FM7, H7).
    ProcessFailed,
    /// A provider stream surfaced an `Error` event (rate-limit/503/auth) —
    /// propagated rather than buried in the transcript (AGENTS-03).
    ProviderError,
}

impl AgentErrorCode {
    pub fn as_wire_code(self) -> &'static str {
        match self {
            AgentErrorCode::ConfigParse => "AGENT_CONFIG_PARSE_ERROR",
            AgentErrorCode::InvalidModelBinding => "AGENT_INVALID_MODEL_BINDING",
            AgentErrorCode::ForbiddenSelfTool => "AGENT_FORBIDDEN_SELF_TOOL",
            AgentErrorCode::NoResult => "AGENT_NO_RESULT",
            AgentErrorCode::MalformedResult => "AGENT_MALFORMED_RESULT",
            AgentErrorCode::ResultFailed => "AGENT_RESULT_FAILED",
            AgentErrorCode::OutputIncomplete => "AGENT_OUTPUT_INCOMPLETE",
            AgentErrorCode::SkillSubjectUnknown => "AGENT_SKILL_SUBJECT_UNKNOWN",
            AgentErrorCode::SkillBodyMissing => "AGENT_SKILL_BODY_MISSING",
            AgentErrorCode::FileLockTimeout => "AGENT_FILE_LOCK_TIMEOUT",
            AgentErrorCode::BinaryMissing => "AGENT_BINARY_MISSING",
            AgentErrorCode::EventShapeDrift => "AGENT_EVENT_SHAPE_DRIFT",
            AgentErrorCode::ProcessFailed => "AGENT_PROCESS_FAILED",
            AgentErrorCode::ProviderError => "AGENT_PROVIDER_ERROR",
        }
    }
}

/// Build a `Permanent` ExecutorError carrying the wire code + actionable context.
pub fn permanent(code: AgentErrorCode, ctx: impl std::fmt::Display) -> ExecutorError {
    ExecutorError::Permanent(format!("{}: {ctx}", code.as_wire_code()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn wire_codes_are_stable() {
        assert_eq!(AgentErrorCode::NoResult.as_wire_code(), "AGENT_NO_RESULT");
        assert_eq!(
            AgentErrorCode::FileLockTimeout.as_wire_code(),
            "AGENT_FILE_LOCK_TIMEOUT"
        );
        assert_eq!(
            AgentErrorCode::OutputIncomplete.as_wire_code(),
            "AGENT_OUTPUT_INCOMPLETE"
        );
        assert_eq!(
            AgentErrorCode::ProcessFailed.as_wire_code(),
            "AGENT_PROCESS_FAILED"
        );
        assert_eq!(
            AgentErrorCode::ProviderError.as_wire_code(),
            "AGENT_PROVIDER_ERROR"
        );
    }

    #[test]
    fn permanent_carries_code_and_context() {
        let e = permanent(
            AgentErrorCode::NoResult,
            "agent exited without final_answer",
        );
        match e {
            ExecutorError::Permanent(msg) => {
                assert!(msg.starts_with("AGENT_NO_RESULT: "), "got: {msg}");
                assert!(msg.contains("without final_answer"));
            }
            other => panic!("expected Permanent, got {other:?}"),
        }
    }
}
