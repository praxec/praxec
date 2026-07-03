//! `AgentExecutor` — the `kind: agent` step. Blackboard-pure (returns output;
//! holds no blackboard write handle) but workspace-effectful (the underlying
//! agent edits files — see file coordination in the binary overlay).
//!
//! `execute()` is a thin, deterministic shell: parse config → assemble the
//! system prompt from in-scope skills (§33.12) + the templated `goal` → resolve
//! the model chain → walk the chain with `run_one` → map the outcome to
//! `ExecuteResult` with fail-fast on every non-escalatable path. All autonomy
//! lives behind the runner seam.

use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use uuid::Uuid;

use praxec_core::error::ExecutorError;
use praxec_core::model::{Evidence, ExecuteRequest, ExecuteResult, ExecutorTelemetry};
use praxec_core::model_resolver::FailureClass;
use praxec_core::ports::Executor;
use praxec_core::promotion::{
    PromotionOutcome, UntrustedAgentRun, UntrustedOutcome, run_untrusted_agent,
};
use praxec_core::repo_locks::RepoLocks;
use praxec_core::sandbox::{Egress, ResourceLimits, SandboxProvider};
use praxec_core::skills::{SkillAssemblyError, assemble_system_message};
use praxec_core::templating::render_template;
use serde_json::{Value, json};

use crate::config::AgentExecutorConfig;
use crate::error::{AgentErrorCode, permanent};
use crate::session::{
    AgentModelResolver, AgentRunOutcome, AgentSession, AgentSessionRunner, AgentStatus,
};

/// Per-step wall-clock timeout when the step omits `max_seconds`.
pub const DEFAULT_MAX_SECONDS: u64 = 600;
/// Hard ceiling — a step can't request an unbounded run (FM3).
pub const MAX_SECONDS_CEILING: u64 = 3600;
/// Default inter-event no-progress (stall) bound when the step omits
/// `stall_seconds`. The window of total stream silence tolerated within a turn
/// before the model is declared stalled and the chain-walk escalates. Set well
/// below `DEFAULT_MAX_SECONDS` so a model that hangs at first token surfaces in
/// ~2 min instead of burning the full 10-min wall, yet generous enough that a
/// model legitimately streaming a slow "thinking" phase keeps resetting it.
pub const DEFAULT_STALL_SECONDS: u64 = 120;

pub struct AgentExecutor {
    runner: Arc<dyn AgentSessionRunner>,
    resolver: Arc<dyn AgentModelResolver>,
    /// ADR-0007 untrusted branch — `None` until `with_untrusted_support`.
    sandbox: Option<Arc<dyn SandboxProvider>>,
    locks: Option<Arc<dyn RepoLocks>>,
}

impl AgentExecutor {
    pub fn new(runner: Arc<dyn AgentSessionRunner>, resolver: Arc<dyn AgentModelResolver>) -> Self {
        Self {
            runner,
            resolver,
            sandbox: None,
            locks: None,
        }
    }

    /// ADR-0007 — enable the `untrusted: true` branch: an untrusted agent runs
    /// confined in a disposable copy and its diff is promoted. Without this, an
    /// `untrusted: true` step fails fast rather than running unconfined.
    pub fn with_untrusted_support(
        mut self,
        sandbox: Arc<dyn SandboxProvider>,
        locks: Arc<dyn RepoLocks>,
    ) -> Self {
        self.sandbox = Some(sandbox);
        self.locks = Some(locks);
        self
    }

    /// ADR-0007 — the untrusted branch: free exploration in a confined disposable
    /// copy, coordinate-at-promotion. Reuses the built `run_untrusted_agent`.
    async fn run_untrusted(
        &self,
        request: &ExecuteRequest,
    ) -> Result<ExecuteResult, ExecutorError> {
        let cfg = &request.executor_config;
        let (sandbox, locks) =
            match (&self.sandbox, &self.locks) {
                (Some(s), Some(l)) => (s, l),
                _ => return Err(ExecutorError::Permanent(
                    "UNTRUSTED_UNAVAILABLE: `kind: agent` with `untrusted: true` needs a sandbox \
                     provider + repo locks, which are not configured on this gateway."
                        .into(),
                )),
            };
        let repo = cfg.get("repo").and_then(Value::as_str).ok_or_else(|| {
            ExecutorError::Permanent(
                "untrusted agent requires `repo` (the source repo to explore a copy of)".into(),
            )
        })?;
        let command: Vec<String> = cfg
            .get("command")
            .and_then(Value::as_array)
            .map(|a| {
                a.iter()
                    .filter_map(|v| v.as_str().map(str::to_string))
                    .collect()
            })
            .filter(|c: &Vec<String>| !c.is_empty())
            .ok_or_else(|| {
                ExecutorError::Permanent(
                    "untrusted agent requires a non-empty `command` (the confined driver argv)"
                        .into(),
                )
            })?;

        let run = UntrustedAgentRun {
            command,
            env: Vec::new(),
            egress: Egress::DenyAll,
            limits: ResourceLimits::default(),
        };
        let holder = format!("agent:{}", request.workflow.id);
        let outcome = run_untrusted_agent(
            std::path::Path::new(repo),
            run,
            sandbox.as_ref(),
            locks.as_ref(),
            &holder,
        )
        .await
        .map_err(|e| ExecutorError::Permanent(format!("untrusted agent run failed: {e}")))?;

        let output = match outcome {
            UntrustedOutcome::NoChanges { sandbox } => json!({
                "outcome": "no_changes",
                "stdout": String::from_utf8_lossy(&sandbox.stdout),
            }),
            UntrustedOutcome::Promoted { promotion, sandbox } => {
                let (status, files): (&str, Vec<_>) = match promotion {
                    PromotionOutcome::Applied { files } => ("promoted", files),
                    PromotionOutcome::Conflict { files } => ("conflict", files),
                    PromotionOutcome::Locked(_) => ("locked", Vec::new()),
                };
                json!({
                    "outcome": status,
                    "files": files,
                    "stdout": String::from_utf8_lossy(&sandbox.stdout),
                })
            }
        };
        Ok(ExecuteResult {
            output,
            evidence: vec![],
            child_workflow_id: None,
            next_transition: None,
            suspend: None,
            telemetry: None,
        })
    }

    /// Run one agent session for the given `model`. Encapsulates
    /// session-building, running, telemetry, evidence, and outcome mapping.
    /// Returns `Ok(ExecuteResult)` on success or `Err(ExecutorError)` on any
    /// failure (the chain-walk loop catches and classifies these).
    async fn run_one(
        &self,
        model: &str,
        cfg: &AgentExecutorConfig,
        request: &ExecuteRequest,
        system_prompt: &Option<String>,
        user_prompt: &str,
    ) -> Result<ExecuteResult, ExecutorError> {
        let max_seconds = cfg
            .max_seconds
            .unwrap_or(DEFAULT_MAX_SECONDS)
            .min(MAX_SECONDS_CEILING);
        // Stall window: never larger than the total budget (a stall bound that
        // outlived the wall would be dead code — the total timeout would always
        // fire first).
        let stall_seconds = cfg
            .stall_seconds
            .unwrap_or(DEFAULT_STALL_SECONDS)
            .min(max_seconds);

        let session = AgentSession {
            model: model.to_string(),
            system_prompt: system_prompt.clone(),
            user_prompt: user_prompt.to_string(),
            tools: cfg
                .tools
                .iter()
                .map(|t| render_template(t, &request.workflow))
                .collect(),
            reasoning_effort: cfg.reasoning_effort.clone(),
            timeout: Duration::from_secs(max_seconds),
            stall_timeout: Duration::from_secs(stall_seconds),
            expected_output_keys: cfg.expected_output_keys.clone(),
            expected_output_types: cfg.expected_output_types.clone(),
        };

        let report = self.runner.run(session).await?;

        // Per-call cost telemetry: price the realized token usage off the
        // model catalog (degrade-to-None when uncatalogued — never fail). The
        // runtime folds this into the `agent.completed` audit event.
        let telemetry = Some(ExecutorTelemetry {
            model: report.model.clone(),
            prompt_tokens: report.prompt_tokens,
            completion_tokens: report.completion_tokens,
            cost_usd: praxec_core::model_catalog::cost_usd(
                &report.model,
                report.prompt_tokens,
                report.completion_tokens,
            ),
        });

        // The full transcript is always preserved for the async "God-view".
        let evidence = vec![Evidence {
            kind: "agent_transcript".to_string(),
            id: Uuid::new_v4().to_string(),
            uri: None,
            summary: Some(format!("agent session ({model})")),
            digest: None,
            confidence: None,
        }];

        match report.outcome {
            AgentRunOutcome::TimedOut => Err(ExecutorError::Timeout(max_seconds)),
            AgentRunOutcome::NoResult => Err(permanent(
                AgentErrorCode::NoResult,
                "agent run ended without a conforming `final_answer` call",
            )),
            AgentRunOutcome::Completed(result) => match result.status {
                AgentStatus::Failed => Err(permanent(
                    AgentErrorCode::ResultFailed,
                    format!(
                        "agent reported status=failed: {}",
                        result
                            .internal_monologue
                            .as_deref()
                            .unwrap_or("(no monologue)")
                    ),
                )),
                // FM12: a successful agent MUST return a structured object to
                // project; null/scalar/array → fail-fast, never a silently
                // unset slot. Per-key projection is the runtime mapping layer's.
                AgentStatus::Success if !result.output.is_object() => Err(permanent(
                    AgentErrorCode::OutputIncomplete,
                    "agent reported success but `output` is not a JSON object to project",
                )),
                AgentStatus::Success => Ok(ExecuteResult {
                    output: result.output,
                    evidence,
                    child_workflow_id: None,
                    next_transition: None,
                    suspend: None,
                    telemetry,
                }),
            },
        }
    }
}

#[async_trait]
impl Executor for AgentExecutor {
    async fn execute(&self, request: ExecuteRequest) -> Result<ExecuteResult, ExecutorError> {
        // ADR-0007 — an `untrusted: true` step explores confined + promotes its
        // diff; everything else is the governed in-process session below.
        if request
            .executor_config
            .get("untrusted")
            .and_then(Value::as_bool)
            .unwrap_or(false)
        {
            return self.run_untrusted(&request).await;
        }

        let cfg = AgentExecutorConfig::from_value(request.executor_config.clone())?;

        // System prompt = in-scope skills (the agent's instructions, §33.12).
        let system_prompt = assemble_system_message(
            &request.workflow.definition,
            &request.workflow.state,
            request.transition.as_deref(),
        )
        .map_err(|e| match e {
            SkillAssemblyError::SubjectUnknown(s) => permanent(
                AgentErrorCode::SkillSubjectUnknown,
                format!(
                    "skill '{s}' is declared in scope but absent from the snapshot `_skillsLibrary`"
                ),
            ),
            SkillAssemblyError::BodyMissing(s) => permanent(
                AgentErrorCode::SkillBodyMissing,
                format!("skill '{s}' has no body in the snapshot `_skillsLibrary`"),
            ),
        })?;

        // User prompt = the templated goal, rendered against the blackboard.
        let user_prompt = render_template(&cfg.goal, &request.workflow);

        // Resolve the full ordered model chain (cheapest-effective first).
        let chain = self.resolver.resolve_chain(&cfg.model_binding()).await?;

        let mut escalations: Vec<Evidence> = Vec::new();

        for (idx, model) in chain.iter().enumerate() {
            match self
                .run_one(model, &cfg, &request, &system_prompt, &user_prompt)
                .await
            {
                Ok(result) => {
                    let mut result = result;
                    for e in escalations.drain(..).rev() {
                        result.evidence.insert(0, e);
                    }
                    return Ok(result);
                }
                Err(e) => {
                    let class = FailureClass::from_executor_error(&e);
                    let is_last = idx + 1 == chain.len();
                    if class.is_infrastructure() && !is_last {
                        tracing::warn!(
                            failed_model = %model,
                            next_model = %chain[idx + 1],
                            error = %e,
                            "agent model failed with escalatable class; escalating to next model in chain"
                        );
                        escalations.push(Evidence {
                            kind: "agent.model_escalation".to_string(),
                            id: Uuid::new_v4().to_string(),
                            uri: None,
                            summary: Some(format!(
                                "escalated from {} to {} ({:?})",
                                model,
                                chain[idx + 1],
                                class
                            )),
                            digest: None,
                            confidence: None,
                        });
                        continue;
                    } else {
                        return Err(e);
                    }
                }
            }
        }

        // Defensive fallback — resolve_chain guarantees non-empty, so this is
        // unreachable in practice but satisfies the type-checker.
        Err(ExecutorError::Permanent(
            "AGENT_INVALID_MODEL_BINDING: empty model chain".into(),
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::ModelBinding;
    use crate::session::testing::{MockModelResolver, MockSessionRunner};
    use crate::session::{AgentResult, AgentRunReport, AgentStatus};
    use praxec_core::model::WorkflowInstance;
    use serde_json::json;

    fn instance(definition: serde_json::Value) -> WorkflowInstance {
        WorkflowInstance {
            id: "wf_agent".into(),
            definition_id: "demo".into(),
            definition_version: "1.0.0".into(),
            definition,
            state: "working".into(),
            version: 0,
            input: json!({}),
            context: json!({ "ticket": "T-7" }),
            started_at: chrono::Utc::now(),
            trace_id: None,
            run_id: None,
            cancelled_at: None,
            cancelled_reason: None,
            depth: 0,
            parent: None,
        }
    }

    fn request(
        executor_config: serde_json::Value,
        definition: serde_json::Value,
    ) -> ExecuteRequest {
        ExecuteRequest {
            workflow: instance(definition),
            transition: None,
            arguments: json!({}),
            executor_config,
            idempotency_key: None,
            correlation_id: None,
        }
    }

    fn bare_def() -> serde_json::Value {
        json!({ "initialState": "working", "states": { "working": {} } })
    }

    fn exec_with(runner: MockSessionRunner) -> AgentExecutor {
        AgentExecutor::new(
            Arc::new(runner),
            Arc::new(MockModelResolver("anthropic:claude-sonnet-4-6".into())),
        )
    }

    #[tokio::test]
    async fn success_projects_output_and_resolves_model() {
        let runner = MockSessionRunner::completed(AgentResult {
            status: AgentStatus::Success,
            output: json!({ "verdict": "pass" }),
            internal_monologue: Some("looked fine".into()),
        });
        let exec = exec_with(runner);
        let res = exec
            .execute(request(
                json!({ "kind": "agent", "affinity": "coding", "goal": "review {{ $.context.ticket }}" }),
                bare_def(),
            ))
            .await
            .expect("success");
        assert_eq!(res.output, json!({ "verdict": "pass" }));
        assert_eq!(res.evidence.len(), 1);
    }

    #[tokio::test]
    async fn tools_are_templated_against_the_blackboard() {
        // A coding step declares `file:{{repo_path}}` so the agent gets file
        // tools rooted at the workflow's repo — the root must be rendered, not
        // passed through literally.
        let runner = Arc::new(MockSessionRunner::completed(AgentResult {
            status: AgentStatus::Success,
            output: json!({}),
            internal_monologue: None,
        }));
        let exec = AgentExecutor::new(
            runner.clone(),
            Arc::new(MockModelResolver("anthropic:x".into())),
        );
        let mut req = request(
            json!({
                "affinity": "coding",
                "goal": "build",
                "tools": ["file:{{ $.workflow.input.repo_path }}", "engine"]
            }),
            bare_def(),
        );
        req.workflow.input = json!({ "repo_path": "/home/me/markdown-mcp" });

        exec.execute(req).await.expect("success");

        assert_eq!(
            runner.sessions()[0].tools,
            vec![
                "file:/home/me/markdown-mcp".to_string(),
                "engine".to_string()
            ],
            "tool connection strings must be rendered against the blackboard"
        );
    }

    #[tokio::test]
    async fn goal_is_templated_against_the_blackboard() {
        let runner = MockSessionRunner::completed(AgentResult {
            status: AgentStatus::Success,
            output: json!({}),
            internal_monologue: None,
        });
        let runner = Arc::new(runner);
        let exec = AgentExecutor::new(
            runner.clone(),
            Arc::new(MockModelResolver("anthropic:x".into())),
        );
        exec.execute(request(
            json!({ "affinity": "coding", "goal": "fix ticket {{ $.context.ticket }}" }),
            bare_def(),
        ))
        .await
        .expect("success");
        let user = runner.sessions()[0].user_prompt.clone();
        assert_eq!(
            user, "fix ticket T-7",
            "goal must be rendered against context"
        );
    }

    #[tokio::test]
    async fn skills_become_the_system_prompt() {
        let runner = Arc::new(MockSessionRunner::completed(AgentResult {
            status: AgentStatus::Success,
            output: json!({}),
            internal_monologue: None,
        }));
        let exec = AgentExecutor::new(
            runner.clone(),
            Arc::new(MockModelResolver("anthropic:x".into())),
        );
        let def = json!({
            "initialState": "working",
            "skills": ["review.tone"],
            "states": { "working": {} },
            "_skillsLibrary": {
                "review.tone": { "verb": "review", "lifecycle": "stable", "body": "BE-TERSE" }
            }
        });
        exec.execute(request(json!({ "affinity": "coding", "goal": "go" }), def))
            .await
            .expect("success");
        let sys = runner.sessions()[0]
            .system_prompt
            .clone()
            .expect("system prompt");
        assert!(
            sys.contains("BE-TERSE"),
            "skill body must be the system prompt: {sys}"
        );
    }

    #[tokio::test]
    async fn no_result_fails_loud() {
        let exec = exec_with(MockSessionRunner::no_result());
        let err = exec
            .execute(request(
                json!({ "affinity": "coding", "goal": "g" }),
                bare_def(),
            ))
            .await
            .expect_err("no final_answer → error");
        assert!(format!("{err:?}").contains("AGENT_NO_RESULT"));
    }

    #[tokio::test]
    async fn status_failed_fails_loud() {
        let exec = exec_with(MockSessionRunner::completed(AgentResult {
            status: AgentStatus::Failed,
            output: json!({}),
            internal_monologue: Some("couldn't do it".into()),
        }));
        let err = exec
            .execute(request(
                json!({ "affinity": "coding", "goal": "g" }),
                bare_def(),
            ))
            .await
            .expect_err("status=failed → error");
        assert!(format!("{err:?}").contains("AGENT_RESULT_FAILED"));
    }

    #[tokio::test]
    async fn success_with_non_object_output_fails_loud() {
        let exec = exec_with(MockSessionRunner::completed(AgentResult {
            status: AgentStatus::Success,
            output: json!("just a string"),
            internal_monologue: None,
        }));
        let err = exec
            .execute(request(
                json!({ "affinity": "coding", "goal": "g" }),
                bare_def(),
            ))
            .await
            .expect_err("non-object output → error");
        assert!(format!("{err:?}").contains("AGENT_OUTPUT_INCOMPLETE"));
    }

    #[tokio::test]
    async fn timeout_maps_to_executor_timeout() {
        let exec = exec_with(MockSessionRunner::timed_out());
        let err = exec
            .execute(request(
                json!({ "affinity": "coding", "goal": "g" }),
                bare_def(),
            ))
            .await
            .expect_err("timeout");
        assert!(matches!(err, ExecutorError::Timeout(_)), "got {err:?}");
    }

    #[tokio::test]
    async fn bad_config_rejected_before_running() {
        let runner = Arc::new(MockSessionRunner::no_result());
        let exec = AgentExecutor::new(runner.clone(), Arc::new(MockModelResolver("x:y".into())));
        let err = exec
            .execute(request(
                json!({ "agent": "a", "affinity": "reasoning", "goal": "g" }),
                bare_def(),
            ))
            .await
            .expect_err("both bindings → error");
        assert!(format!("{err:?}").contains("AGENT_INVALID_MODEL_BINDING"));
        assert!(
            runner.sessions().is_empty(),
            "runner must not be invoked on bad config"
        );
    }

    // S5 (testing-strategy) — model-resolution *failure*. A syntactically valid
    // binding the resolver can't resolve (unknown model / no provider) must fail
    // the execute and short-circuit before the session runs.
    struct FailingResolver;
    #[async_trait]
    impl AgentModelResolver for FailingResolver {
        async fn resolve(&self, _binding: &ModelBinding) -> Result<String, ExecutorError> {
            Err(ExecutorError::Permanent(
                "MODEL_UNRESOLVED: no provider for the binding".into(),
            ))
        }
    }

    #[tokio::test]
    async fn a_resolver_failure_propagates_as_the_execute_error() {
        let runner = Arc::new(MockSessionRunner::no_result());
        let exec = AgentExecutor::new(runner, Arc::new(FailingResolver));
        let err = exec
            .execute(request(
                json!({ "kind": "agent", "affinity": "coding", "goal": "g" }),
                bare_def(),
            ))
            .await
            .expect_err("an unresolvable model fails the execute");
        assert!(format!("{err:?}").contains("MODEL_UNRESOLVED"));
    }

    #[tokio::test]
    async fn a_resolver_failure_skips_the_session() {
        // Fail-fast ordering: resolution happens before the runner is touched.
        let runner = Arc::new(MockSessionRunner::no_result());
        let exec = AgentExecutor::new(runner.clone(), Arc::new(FailingResolver));
        let _ = exec
            .execute(request(
                json!({ "kind": "agent", "affinity": "coding", "goal": "g" }),
                bare_def(),
            ))
            .await;
        assert!(
            runner.sessions().is_empty(),
            "resolution must fail before the session runs"
        );
    }

    // ── Chain-walk escalation tests ──────────────────────────────────────

    /// A resolver that returns a two-model chain: weak first, strong second.
    struct TwoModelResolver;
    #[async_trait]
    impl AgentModelResolver for TwoModelResolver {
        async fn resolve(&self, _binding: &ModelBinding) -> Result<String, ExecutorError> {
            Ok("openrouter:weak".into())
        }
        async fn resolve_chain(
            &self,
            _binding: &ModelBinding,
        ) -> Result<Vec<String>, ExecutorError> {
            Ok(vec![
                "openrouter:weak".to_string(),
                "openrouter:strong".to_string(),
            ])
        }
    }

    /// A runner that fails with NoResult (→ Capability, escalatable) on the
    /// weak model and succeeds with a valid output on the strong model.
    struct EscalatingRunner {
        seen: std::sync::Mutex<Vec<AgentSession>>,
    }
    impl EscalatingRunner {
        fn new() -> Self {
            Self {
                seen: std::sync::Mutex::new(Vec::new()),
            }
        }
        fn sessions(&self) -> Vec<AgentSession> {
            self.seen.lock().unwrap().clone()
        }
    }
    #[async_trait]
    impl AgentSessionRunner for EscalatingRunner {
        async fn run(&self, session: AgentSession) -> Result<AgentRunReport, ExecutorError> {
            self.seen.lock().unwrap().push(session.clone());
            if session.model == "openrouter:weak" {
                // NoResult → Capability → escalatable
                Ok(AgentRunReport {
                    outcome: AgentRunOutcome::NoResult,
                    transcript: String::new(),
                    model: session.model.clone(),
                    prompt_tokens: 0,
                    completion_tokens: 0,
                })
            } else {
                // Strong model succeeds
                Ok(AgentRunReport {
                    outcome: AgentRunOutcome::Completed(AgentResult {
                        status: AgentStatus::Success,
                        output: json!({ "result": "done" }),
                        internal_monologue: None,
                    }),
                    transcript: String::new(),
                    model: session.model.clone(),
                    prompt_tokens: 10,
                    completion_tokens: 5,
                })
            }
        }
    }

    #[tokio::test]
    async fn escalation_succeeds_when_weak_model_returns_no_result() {
        // Weak model returns NoResult (→ Capability → is_infrastructure() == true),
        // so the chain-walk should escalate to the strong model and return Ok.
        let runner = Arc::new(EscalatingRunner::new());
        let exec = AgentExecutor::new(runner.clone(), Arc::new(TwoModelResolver));

        let result = exec
            .execute(request(
                json!({ "affinity": "coding", "goal": "do something" }),
                bare_def(),
            ))
            .await
            .expect("escalation should succeed on the strong model");

        assert_eq!(result.output, json!({ "result": "done" }));

        // Verify both models were tried in order
        let sessions = runner.sessions();
        assert_eq!(sessions.len(), 2, "both models should have been attempted");
        assert_eq!(sessions[0].model, "openrouter:weak");
        assert_eq!(sessions[1].model, "openrouter:strong");

        // Evidence must include the escalation hop BEFORE the transcript evidence.
        assert_eq!(
            result.evidence.len(),
            2,
            "expected 2 evidence entries: escalation + transcript"
        );
        let esc = &result.evidence[0];
        assert_eq!(esc.kind, "agent.model_escalation");
        let summary = esc
            .summary
            .as_ref()
            .expect("escalation evidence must have a summary");
        assert!(
            summary.contains("openrouter:weak"),
            "summary must mention the failed model: {summary}"
        );
        assert!(
            summary.contains("openrouter:strong"),
            "summary must mention the next model: {summary}"
        );
        // The transcript evidence should still be present (at index 1).
        assert_eq!(result.evidence[1].kind, "agent_transcript");
    }

    /// A runner that always returns a non-escalatable content error regardless
    /// of which model is used — should NOT escalate and must surface the error.
    struct ContentErrorRunner;
    #[async_trait]
    impl AgentSessionRunner for ContentErrorRunner {
        async fn run(&self, _session: AgentSession) -> Result<AgentRunReport, ExecutorError> {
            // Returns Err directly — maps to ContentOther (not infrastructure)
            Err(ExecutorError::Permanent("some author bug".into()))
        }
    }

    #[tokio::test]
    async fn non_escalatable_error_surfaces_immediately_without_escalating() {
        // A ContentOther error (e.g. author bug) on the first model of a
        // two-model chain must NOT escalate; it must surface immediately.
        let runner = Arc::new(ContentErrorRunner);
        let exec = AgentExecutor::new(runner, Arc::new(TwoModelResolver));

        let err = exec
            .execute(request(
                json!({ "affinity": "coding", "goal": "do something" }),
                bare_def(),
            ))
            .await
            .expect_err("content error should surface, not escalate");

        // The exact error from the runner is propagated unchanged
        assert!(
            format!("{err:?}").contains("some author bug"),
            "original error should be surfaced: {err:?}"
        );
    }

    // ── ADR-0007 — the untrusted branch ────────────────────────────────────

    use praxec_core::repo_locks::RepoLockSpace;
    use praxec_core::sandbox::{Preflight, SandboxOutput, SandboxSpec};
    use std::path::Path;

    /// A SandboxProvider that edits its workspace — stands in for the confined
    /// agent exploring the disposable copy.
    struct EditingProvider;
    #[async_trait]
    impl SandboxProvider for EditingProvider {
        fn preflight(&self) -> Preflight {
            Preflight {
                usable: true,
                detail: "editing".into(),
                install_hint: None,
            }
        }
        async fn run(&self, spec: &SandboxSpec) -> anyhow::Result<SandboxOutput> {
            let ws = spec.workspace.clone().expect("workspace");
            std::fs::write(ws.join("a.txt"), "a0\nagent-edited\n").unwrap();
            Ok(SandboxOutput {
                code: Some(0),
                success: true,
                stdout: b"explored".to_vec(),
                stderr: vec![],
            })
        }
    }

    fn setup_repo(d: &Path) {
        use std::process::Command;
        let g = |args: &[&str]| {
            assert!(
                Command::new("git")
                    .arg("-C")
                    .arg(d)
                    .args(args)
                    .output()
                    .unwrap()
                    .status
                    .success()
            );
        };
        Command::new("git")
            .args(["init", "-q", "-b", "main"])
            .arg(d)
            .output()
            .unwrap();
        std::fs::write(d.join("a.txt"), "a0\n").unwrap();
        g(&["-c", "user.email=t@t", "-c", "user.name=t", "add", "."]);
        g(&[
            "-c",
            "user.email=t@t",
            "-c",
            "user.name=t",
            "commit",
            "-qm",
            "C0",
        ]);
    }

    #[tokio::test]
    async fn untrusted_agent_explores_confined_and_promotes() {
        let repo = tempfile::tempdir().unwrap();
        setup_repo(repo.path());
        let exec = exec_with(MockSessionRunner::no_result())
            .with_untrusted_support(Arc::new(EditingProvider), Arc::new(RepoLockSpace::new()));

        let cfg = json!({
            "untrusted": true,
            "repo": repo.path().to_string_lossy(),
            "command": ["true"],
        });
        let result = exec
            .execute(request(cfg, bare_def()))
            .await
            .expect("untrusted run");
        assert_eq!(result.output["outcome"], "promoted");
        assert!(
            std::fs::read_to_string(repo.path().join("a.txt"))
                .unwrap()
                .contains("agent-edited")
        );
    }

    #[tokio::test]
    async fn untrusted_without_sandbox_support_fails_fast() {
        // No with_untrusted_support → an untrusted step must refuse, not run.
        let exec = exec_with(MockSessionRunner::no_result());
        let err = exec
            .execute(request(
                json!({ "untrusted": true, "repo": "/x", "command": ["true"] }),
                bare_def(),
            ))
            .await
            .expect_err("untrusted without a sandbox must fail fast");
        assert!(format!("{err:?}").contains("UNTRUSTED_UNAVAILABLE"));
    }
}
