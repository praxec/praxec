//! The injectable session-runner seam — mirrors `ProviderFactory`
//! (llm-executor) and `McpToolCaller` (executors). The `AgentExecutor`
//! depends only on these traits, so its logic (config, projection, fail-fasts)
//! is unit-tested with stubs — no subprocess, no real LLM.

use std::time::Duration;

use async_trait::async_trait;
use serde::{Deserialize, Serialize};

use praxec_core::error::ExecutorError;

use crate::config::ModelBinding;

/// The structured result an agent reports via the schema-enforced
/// `final_answer` contract. `output` is the object projected to slots through
/// the step's existing `output:` mapping.
#[derive(Debug, Clone, PartialEq, Deserialize)]
pub struct AgentResult {
    pub status: AgentStatus,
    #[serde(default)]
    pub output: serde_json::Value,
    #[serde(default)]
    pub internal_monologue: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum AgentStatus {
    Success,
    Failed,
}

/// What the runner observed for one session.
#[derive(Debug, Clone, PartialEq)]
pub enum AgentRunOutcome {
    /// The agent called `final_answer` with a conforming envelope.
    Completed(AgentResult),
    /// The run ended without a conforming `final_answer` call (FM1).
    NoResult,
    /// Wall-clock timeout (FM4).
    TimedOut,
    /// P12 R1.4 — the agent hit its suspend signal (`await_human`): the
    /// tool-loop STOPPED and its conversation was **durably persisted** to the
    /// [`ParkedSessionStore`](praxec_core::ports::ParkedSessionStore), keyed
    /// by `correlation_id`. First-class control flow — NOT an error, NOT
    /// NoResult. A later correlated reply resumes the exact frame via
    /// [`RigSessionRunner::resume`](crate::rig_runner::RigSessionRunner::resume).
    Suspended(AgentSuspension),
}

/// The park receipt carried by [`AgentRunOutcome::Suspended`]: proof the frame
/// is durably persisted, plus the context a human needs to answer.
#[derive(Debug, Clone, PartialEq)]
pub struct AgentSuspension {
    /// Routes the human's later reply back to this exact parked frame.
    pub correlation_id: String,
    /// What the agent asked the human (the `await_human` call's `prompt`).
    pub prompt: String,
}

/// Runner report: the outcome plus the captured transcript (→ Evidence) and
/// realized token usage summed across the agent's turns (→ cost telemetry).
#[derive(Debug, Clone)]
pub struct AgentRunReport {
    pub outcome: AgentRunOutcome,
    /// Captured stdout (JSON-lines) for the audit trail.
    pub transcript: String,
    /// The resolved `"provider:model"` the session ran on — carried back so the
    /// audit can price the run against the model catalog.
    pub model: String,
    /// Prompt (input) tokens summed across every turn. `0` when the provider
    /// reported no usage (degrade gracefully — never fail the run).
    pub prompt_tokens: u64,
    /// Completion (output) tokens summed across every turn. `0` when absent.
    pub completion_tokens: u64,
}

/// Correlating identity for observability events emitted DURING a run
/// (`agent.heartbeat`) — the same identity the runtime stamps on
/// `agent.invoked` / `agent.completed`, so an in-run heartbeat joins their
/// correlation in the audit stream. All-`None` (the default, and the value
/// deserialized for parked frames persisted before this existed) simply omits
/// the fields from the emitted events.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct RunIdentity {
    /// The workflow instance the agent step belongs to.
    pub workflow_id: Option<String>,
    /// The step's audit correlation id — `agent.invoked` carries the same one,
    /// so a heartbeat is joinable to its boundary events.
    pub correlation_id: Option<String>,
    /// The transition being driven.
    pub transition: Option<String>,
}

/// Serde default for [`AgentSession::tool_setup_timeout`] — the historical 60s
/// bound, applied when resuming a snapshot written before the field existed.
fn default_tool_setup_timeout() -> Duration {
    Duration::from_secs(crate::executor::DEFAULT_TOOL_SETUP_SECONDS)
}

/// Everything needed to run one isolated agent session.
///
/// Serde derives exist for exactly one reason: P12 R1.4 persists the session
/// alongside its parked conversation so a durable resume can rebuild the run
/// (model, prompts, tool connections, contract) after a power cycle.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AgentSession {
    /// Resolved `"provider:model"` string.
    pub model: String,
    /// In-scope skill bodies (§33.12) — the agent's instructions.
    pub system_prompt: Option<String>,
    /// The rendered `goal` — the task to perform.
    pub user_prompt: String,
    /// MCP connection names to expose (never praxec-self).
    pub tools: Vec<String>,
    pub reasoning_effort: Option<String>,
    pub timeout: Duration,
    /// Inter-event no-progress (stall) bound: the maximum span of total silence
    /// — no stream event of any kind (thinking/text/tool-call/usage) — tolerated
    /// while draining one turn before the model is declared stalled. Any event
    /// resets the window, so a streaming-but-slow model is preserved while a
    /// model that hangs at first token is caught in seconds rather than burning
    /// the whole `timeout`. A stall escalates the chain-walk to the next model
    /// (it does NOT re-run the same hung model). See the runner's drain loop.
    pub stall_timeout: Duration,
    /// Wall-clock bound on pre-turn tool setup — the MCP `host.tools()`
    /// discovery/connection call that lists every declared connection's tools
    /// before the first model turn. A hung or slow tool server is bounded here
    /// (surfacing a loud `Timeout`) rather than stalling the run. Resolved from
    /// the step's `tool_setup_seconds` override or the 60s default, clamped to
    /// `timeout`. See [`crate::executor::resolve_tool_setup_timeout`].
    ///
    /// `#[serde(default)]`: a session snapshot persisted (P12 R1.4) BEFORE this
    /// field existed resumes with the historical 60s bound — the exact behavior
    /// it ran under — rather than failing to deserialize.
    #[serde(default = "default_tool_setup_timeout")]
    pub tool_setup_timeout: Duration,
    /// The top-level keys the agent's `output` object must contain — the
    /// "criteria" the runner uses to (a) validate a salvaged JSON text answer
    /// before accepting it, and (b) phrase precise in-session feedback when the
    /// model answers non-conformingly. Empty ⇒ no specific keys required (the
    /// answer need only be a JSON object).
    pub expected_output_keys: Vec<String>,
    /// Declared JSON type per output key (from the transition
    /// `inputSchema.properties[key].type`). The runner enforces these at the
    /// `final_answer` boundary and re-prompts on a mismatch, so a wrong-type
    /// answer is corrected in-session rather than discarded post-run. Empty /
    /// missing entries are not type-checked.
    pub expected_output_types: std::collections::BTreeMap<String, String>,
    /// P12 R1.4 — opt-in suspend capability. When `true` the runner offers the
    /// reserved `await_human` tool; calling it parks the session durably
    /// ([`AgentRunOutcome::Suspended`]). **Default `false`** (and serde
    /// defaults it for records parked before the field existed): a session
    /// that doesn't opt in can NEVER suspend — the tool isn't offered, and a
    /// hallucinated call routes to the normal unknown-tool error result.
    #[serde(default)]
    pub await_enabled: bool,
    /// Correlating identity for the in-run `agent.heartbeat` audit events.
    /// Defaulted (all-`None`) for callers that run sessions outside a governed
    /// step (e.g. the orchestrator's decision calls) and for parked frames
    /// persisted before the field existed.
    #[serde(default)]
    pub identity: RunIdentity,
}

/// Runs ONE autonomous agent session and reports the outcome. The production
/// impl is the in-process [`RigSessionRunner`](crate::rig_runner::RigSessionRunner);
/// tests inject a stub.
#[async_trait]
pub trait AgentSessionRunner: Send + Sync {
    async fn run(&self, session: AgentSession) -> Result<AgentRunReport, ExecutorError>;

    /// P12 R1.4 — resume a durably parked session (one that previously ended
    /// [`AgentRunOutcome::Suspended`]) by injecting the human `reply` as the
    /// awaited `await_human` call's tool result and re-entering the tool loop
    /// from the parked turn. The production impl is
    /// [`RigSessionRunner::resume`](crate::rig_runner::RigSessionRunner::resume).
    ///
    /// Default: typed fail-fast. A runner without park support can never have
    /// produced a `Suspended` outcome, so a resume against it is a wiring bug
    /// — surface it loudly rather than silently starting a fresh session.
    async fn resume(
        &self,
        correlation_id: &str,
        _reply: &str,
    ) -> Result<AgentRunReport, ExecutorError> {
        Err(ExecutorError::Permanent(format!(
            "AGENT_AWAIT_UNSUPPORTED: this session runner cannot resume parked session \
             '{correlation_id}' (no ParkedSessionStore-backed resume support)"
        )))
    }
}

/// Resolves a config `ModelBinding` (agent name / affinity) to a
/// `"provider:model"` string. The binary wires an models.yaml-backed impl
/// (the same resolution the llm executor uses); tests inject a stub.
#[async_trait]
pub trait AgentModelResolver: Send + Sync {
    async fn resolve(&self, binding: &ModelBinding) -> Result<String, ExecutorError>;

    /// Returns the ordered model-id chain to try, cheapest-effective first.
    ///
    /// The default wraps [`resolve`](Self::resolve) as a single-element chain,
    /// so existing test doubles and the `RejectingAgentModelResolver` need no
    /// changes. A models.yaml-backed implementation overrides this with the
    /// full walk over every binding in the affinity list, enabling the
    /// executor to escalate through the chain on failure.
    async fn resolve_chain(&self, binding: &ModelBinding) -> Result<Vec<String>, ExecutorError> {
        Ok(vec![self.resolve(binding).await?])
    }
}

// ── test doubles (available to inline unit tests and, via `test-util`, to
//    integration tests / example drivers) ──────────────────────────────────

#[cfg(any(test, feature = "test-util"))]
pub mod testing {
    use std::sync::Mutex;

    use super::*;

    /// A settable [`AgentSessionRunner`] that returns a canned report and
    /// records the sessions it was handed (for assertions).
    pub struct MockSessionRunner {
        report: AgentRunReport,
        seen: Mutex<Vec<AgentSession>>,
        resumes: Mutex<Vec<(String, String)>>,
        resume_report: Option<AgentRunReport>,
    }

    impl MockSessionRunner {
        pub fn completed(result: AgentResult) -> Self {
            Self::with_outcome(AgentRunOutcome::Completed(result))
        }
        pub fn no_result() -> Self {
            Self::with_outcome(AgentRunOutcome::NoResult)
        }
        pub fn timed_out() -> Self {
            Self::with_outcome(AgentRunOutcome::TimedOut)
        }
        pub fn suspended(correlation_id: &str, prompt: &str) -> Self {
            Self::with_outcome(AgentRunOutcome::Suspended(AgentSuspension {
                correlation_id: correlation_id.into(),
                prompt: prompt.into(),
            }))
        }
        fn with_outcome(outcome: AgentRunOutcome) -> Self {
            Self {
                report: AgentRunReport {
                    outcome,
                    transcript: "{\"kind\":\"text\",\"message\":\"mock\"}".into(),
                    model: "mock:model".into(),
                    prompt_tokens: 0,
                    completion_tokens: 0,
                },
                seen: Mutex::new(Vec::new()),
                resumes: Mutex::new(Vec::new()),
                resume_report: None,
            }
        }
        /// Canned outcome for `resume` calls (leaves `run`'s report untouched).
        pub fn with_resume_outcome(mut self, outcome: AgentRunOutcome) -> Self {
            self.resume_report = Some(AgentRunReport {
                outcome,
                transcript: "{\"kind\":\"text\",\"message\":\"mock-resume\"}".into(),
                model: "mock:model".into(),
                prompt_tokens: 0,
                completion_tokens: 0,
            });
            self
        }
        pub fn sessions(&self) -> Vec<AgentSession> {
            self.seen.lock().unwrap().clone()
        }
        /// The `(correlation_id, reply)` pairs `resume` was called with.
        pub fn resumes(&self) -> Vec<(String, String)> {
            self.resumes.lock().unwrap().clone()
        }
    }

    #[async_trait]
    impl AgentSessionRunner for MockSessionRunner {
        async fn run(&self, session: AgentSession) -> Result<AgentRunReport, ExecutorError> {
            self.seen.lock().unwrap().push(session);
            Ok(self.report.clone())
        }

        async fn resume(
            &self,
            correlation_id: &str,
            reply: &str,
        ) -> Result<AgentRunReport, ExecutorError> {
            self.resumes
                .lock()
                .unwrap()
                .push((correlation_id.to_string(), reply.to_string()));
            match &self.resume_report {
                Some(report) => Ok(report.clone()),
                None => Err(ExecutorError::Permanent(
                    "MOCK_RESUME_UNCONFIGURED: set with_resume_outcome".into(),
                )),
            }
        }
    }

    /// A resolver that returns a fixed `"provider:model"`.
    pub struct MockModelResolver(pub String);

    #[async_trait]
    impl AgentModelResolver for MockModelResolver {
        async fn resolve(&self, _binding: &ModelBinding) -> Result<String, ExecutorError> {
            Ok(self.0.clone())
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn agent_result_parses_envelope() {
        let r: AgentResult = serde_json::from_value(json!({
            "status": "success",
            "output": { "fix": "patched" },
            "internal_monologue": "thought about it"
        }))
        .unwrap();
        assert_eq!(r.status, AgentStatus::Success);
        assert_eq!(r.output, json!({ "fix": "patched" }));
    }

    #[test]
    fn agent_result_defaults_missing_output_and_monologue() {
        let r: AgentResult = serde_json::from_value(json!({ "status": "failed" })).unwrap();
        assert_eq!(r.status, AgentStatus::Failed);
        assert_eq!(r.output, serde_json::Value::Null);
        assert!(r.internal_monologue.is_none());
    }

    #[test]
    fn session_snapshot_without_tool_setup_timeout_resumes_at_default() {
        // A session persisted (P12 R1.4) before `tool_setup_timeout` existed must
        // still deserialize on resume — falling back to the historical 60s bound
        // it ran under, not a serde error.
        let s: AgentSession = serde_json::from_value(json!({
            "model": "anthropic:claude-sonnet-4-6",
            "system_prompt": null,
            "user_prompt": "do the thing",
            "tools": ["conn"],
            "reasoning_effort": null,
            "timeout": { "secs": 600, "nanos": 0 },
            "stall_timeout": { "secs": 120, "nanos": 0 },
            "expected_output_keys": [],
            "expected_output_types": {},
            "await_enabled": false,
            "identity": {}
        }))
        .expect("legacy snapshot without tool_setup_timeout must deserialize");
        assert_eq!(s.tool_setup_timeout, Duration::from_secs(60));
    }
}
