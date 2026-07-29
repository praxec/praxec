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
use std::time::{Duration, Instant};

use async_trait::async_trait;
use uuid::Uuid;

use praxec_core::audit::{AuditEvent, AuditSink};
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

use crate::breaker::BreakerRegistry;
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
/// (CR#1) Default wall-clock ceiling on a single step's ENTIRE model chain-walk
/// when the step omits `step_budget_seconds`.
///
/// The chain-walk escalates on every infrastructure-class failure — including
/// `AGENT_NO_RESULT` — and each model it tries gets its own full
/// [`DEFAULT_MAX_SECONDS`] wall. A chain of reasoning models that all fail to
/// sign off therefore burned N×600s of *silent churn* with no forward progress
/// and no state transition (the observed fix-loop stall). This budget bounds the
/// whole walk: each attempt's wall is clamped to the budget remaining, and when
/// it runs out the walk stops with a terminal `AGENT_STEP_BUDGET_EXHAUSTED`
/// rather than starting another full-wall attempt.
///
/// Sized to fit one full-wall attempt plus a meaningful escalation (not N of
/// them): generous enough that a single legitimately-slow model is never
/// kneecapped, tight enough that a wedged step surfaces to a human in minutes
/// instead of tens of minutes.
pub const DEFAULT_STEP_BUDGET_SECONDS: u64 = 900;
/// Default tool-setup (MCP `host.tools()`) timeout, in seconds, when a step omits
/// `tool_setup_seconds`. Bounds the pre-turn tool discovery/connection phase so a
/// hung or slow tool server surfaces as a loud `Timeout` rather than a silent
/// stall. The historical hardcoded value; now a default rather than a `const`
/// ceiling so an expensive step can raise it per-call.
pub const DEFAULT_TOOL_SETUP_SECONDS: u64 = 60;
/// Floor below which the remaining step budget is too small to be worth another
/// model attempt — starting a run with a couple of seconds left would just
/// manufacture a timeout. Below this, the walk stops and surfaces.
const MIN_ATTEMPT_SECONDS: u64 = 15;

/// The audit `event_type` emitted when ONE model attempt of a step's
/// chain-walk ends — success, suspend, or classified failure. Payload:
/// `{ attempt_index, model, outcome, error, duration_ms, transition }`,
/// stamped with the step's workflow id + correlation so an operator tailing
/// the audit stream can tell a provider outage from a hang-prone lead model
/// per attempt, not just per step (v0.0.28 dogfood defect 2).
pub const AGENT_MODEL_ATTEMPT_EVENT: &str = "agent.model_attempt";

/// Resolve the effective tool-setup timeout: the step's `tool_setup_seconds`
/// override (call-level) or [`DEFAULT_TOOL_SETUP_SECONDS`], clamped to the step's
/// resolved `max_seconds` (a setup bound longer than the whole wall is dead code
/// — the wall would always fire first). Pure + total so it is directly unit-tested.
pub(crate) fn resolve_tool_setup_timeout(cfg_seconds: Option<u64>, max_seconds: u64) -> Duration {
    Duration::from_secs(
        cfg_seconds
            .unwrap_or(DEFAULT_TOOL_SETUP_SECONDS)
            .min(max_seconds),
    )
}

/// Resolve one attempt's effective `(max_seconds, stall_seconds)` windows: the
/// step's configured (or default) wall clamped to the ceiling AND to what's
/// left of the step budget (`wall_cap`), then the stall window clamped to that
/// wall (a stall bound outliving the wall would be dead code). Extracted from
/// [`AgentExecutor::run_one`] so the chain-walk loop records the SAME clamped
/// windows in its attempt log that the attempt actually ran under.
pub(crate) fn effective_attempt_windows(
    cfg: &AgentExecutorConfig,
    wall_cap: Duration,
) -> (u64, u64) {
    let max_seconds = cfg
        .max_seconds
        .unwrap_or(DEFAULT_MAX_SECONDS)
        .min(MAX_SECONDS_CEILING)
        .min(wall_cap.as_secs());
    // Stall window: never larger than the total budget (a stall bound that
    // outlived the wall would be dead code — the total timeout would always
    // fire first).
    let stall_seconds = cfg
        .stall_seconds
        .unwrap_or(DEFAULT_STALL_SECONDS)
        .min(max_seconds);
    (max_seconds, stall_seconds)
}

/// One finished model attempt of a step's chain-walk — the evidence unit the
/// terminal `AGENT_CHAIN_EXHAUSTED` / `AGENT_STEP_BUDGET_EXHAUSTED` errors
/// summarize so an exhausted step never surfaces as just the last attempt's
/// clamped timeout (v0.0.28 dogfood defect 1).
struct AttemptRecord {
    model: String,
    /// The attempt's [`FailureClass`] (Debug form, e.g. `NetworkTimeout`,
    /// `RateLimit429`, `Capability`) — derived from the typed classifier,
    /// never hand-written.
    outcome: String,
    /// The attempt's error Display (e.g. `timeout after 74000 ms`).
    error: String,
    duration: Duration,
    /// The CLAMPED wall this attempt actually ran under.
    wall_seconds: u64,
    /// The CLAMPED stall window this attempt actually ran under.
    stall_seconds: u64,
}

/// Render the walk's attempt log for a terminal error message:
/// `[1/2] model → Class after Ns (wall Ns, stall Ns): error; [2/2] …`.
fn walk_summary(attempts: &[AttemptRecord]) -> String {
    let n = attempts.len();
    attempts
        .iter()
        .enumerate()
        .map(|(i, a)| {
            format!(
                "[{}/{}] {} → {} after {}s (wall {}s, stall {}s): {}",
                i + 1,
                n,
                a.model,
                a.outcome,
                a.duration.as_secs(),
                a.wall_seconds,
                a.stall_seconds,
                a.error,
            )
        })
        .collect::<Vec<_>>()
        .join("; ")
}

/// A short, self-announcing tail of an agent transcript, for surfacing PARTIAL
/// work on a budget outcome so the flow sees what WAS produced rather than a
/// hollow error. The full transcript is always retained in the God-view audit
/// record; this is the inline breadcrumb. Char-boundary safe.
fn partial_tail(transcript: &str) -> String {
    const MAX: usize = 600;
    let t = transcript.trim();
    if t.is_empty() {
        return "(no output produced)".to_string();
    }
    if t.len() <= MAX {
        return t.to_string();
    }
    let cut = t.len() - MAX;
    let start = (cut..t.len())
        .find(|&i| t.is_char_boundary(i))
        .unwrap_or(t.len());
    format!("…{}", &t[start..])
}

/// The durable `_agent_await` wait marker, read off the workflow context when
/// it belongs to the transition being dispatched (transition-identity guarded,
/// mirroring the runtime's `agent_await_for`). `Some(_)` means this dispatch
/// is a RESUME of a parked frame, not a fresh run.
struct AgentAwaitMarker {
    correlation_id: String,
    prompt: String,
}

fn agent_await_marker(request: &ExecuteRequest) -> Option<AgentAwaitMarker> {
    let wait = request.workflow.context.get("_agent_await")?;
    let marker_transition = wait.get("transition").and_then(Value::as_str)?;
    if request.transition.as_deref() != Some(marker_transition) {
        return None;
    }
    Some(AgentAwaitMarker {
        correlation_id: wait
            .get("correlation_id")
            .and_then(Value::as_str)?
            .to_string(),
        prompt: wait
            .get("prompt")
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_string(),
    })
}

pub struct AgentExecutor {
    runner: Arc<dyn AgentSessionRunner>,
    resolver: Arc<dyn AgentModelResolver>,
    /// P12 — per-model cooldown circuit-breaker. Cross-call memory of which
    /// models have failed repeatedly, so the per-invocation chain-walk skips a
    /// known-bad model for a cooldown window instead of re-probing (and
    /// re-timing-out on) it on every agent call.
    breaker: BreakerRegistry,
    /// ADR-0007 untrusted branch — `None` until `with_untrusted_support`.
    sandbox: Option<Arc<dyn SandboxProvider>>,
    locks: Option<Arc<dyn RepoLocks>>,
    /// Per-attempt observability — `None` until `with_audit_sink`. When wired,
    /// every model attempt of the chain-walk lands one
    /// [`AGENT_MODEL_ATTEMPT_EVENT`] in the audit stream (emit-only; a sink
    /// failure never affects the run — mirrors the runner's heartbeat).
    audit: Option<Arc<dyn AuditSink>>,
}

impl AgentExecutor {
    pub fn new(runner: Arc<dyn AgentSessionRunner>, resolver: Arc<dyn AgentModelResolver>) -> Self {
        Self {
            runner,
            resolver,
            breaker: BreakerRegistry::default(),
            sandbox: None,
            locks: None,
            audit: None,
        }
    }

    /// Observability (v0.0.28 dogfood defect 2) — wire the gateway audit sink
    /// so every model attempt of a chain-walk emits one
    /// [`AGENT_MODEL_ATTEMPT_EVENT`] under the step's workflow id +
    /// correlation. Without it the audit stream only carries the step's
    /// boundaries and an operator cannot tell a provider outage from a
    /// hang-prone lead model.
    pub fn with_audit_sink(mut self, sink: Arc<dyn AuditSink>) -> Self {
        self.audit = Some(sink);
        self
    }

    /// Record one attempt's end in the audit stream. Infallible by design:
    /// pure observability, so a missing sink or a sink error is dropped rather
    /// than failing the run it narrates (mirrors the runner's heartbeat).
    #[allow(clippy::too_many_arguments)]
    async fn emit_model_attempt(
        &self,
        request: &ExecuteRequest,
        attempt_index: usize,
        model: &str,
        outcome: &str,
        error: Option<&str>,
        duration: Duration,
        // Tokens the attempt spent, when a runner report was produced (Some even
        // on a FAILED attempt — a non-converging model still burned tokens). None
        // when no report reached us (pre-run fail-fast / runner error) — recorded
        // as `null`, legibly unpriced, never fabricated zeros. Lets the cost
        // report count real spend on failed attempts (not just the winner).
        prompt_tokens: Option<u64>,
        completion_tokens: Option<u64>,
        // The reasoning effort THIS attempt ran under (paired with the model),
        // so the de-escalation flywheel can key a failed-attempt observation on
        // `(affinity, model, effort)` — the same triple it aggregates the winner
        // on. Without it a losing model is invisible to the flywheel.
        reasoning_effort: Option<&str>,
    ) {
        let Some(sink) = &self.audit else {
            return;
        };
        // Price the attempt from the catalog; `null` when uncatalogued or unknown.
        let cost_usd = match (prompt_tokens, completion_tokens) {
            (Some(p), Some(c)) => praxec_core::model_catalog::cost_usd(model, p, c),
            _ => None,
        };
        let mut event = AuditEvent::new(AGENT_MODEL_ATTEMPT_EVENT)
            .with_workflow(request.workflow.id.clone())
            .with_payload(json!({
                "attempt_index": attempt_index,
                "model": model,
                "outcome": outcome,
                "error": error,
                "duration_ms": duration.as_millis() as u64,
                "transition": request.transition,
                "prompt_tokens": prompt_tokens,
                "completion_tokens": completion_tokens,
                "cost_usd": cost_usd,
                "reasoning_effort": reasoning_effort,
            }));
        if let Some(c) = &request.correlation_id {
            event = event.with_correlation(c.clone());
        }
        let _ = sink.record(event).await;
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
        // (CR#1) What's LEFT of the step's chain-walk budget. Clamps this
        // attempt's wall so the sum across the whole walk can never exceed the
        // budget — without it, each escalation would silently start a fresh
        // full-length wall and the "budget" would bound nothing.
        //
        // (WS1-B) The per-hop reasoning effort rides on `cfg.reasoning_effort` —
        // the caller passes a per-attempt `cfg` whose `reasoning_effort` is the
        // model's paired effort (already fail-fast-validated) — so this fn keeps
        // its signature and the session build below is unchanged.
        wall_cap: Duration,
        // Tokens spent (when a report was produced, even on failure) + the mapped
        // result. Returning the tokens lets the walk loop emit a FAILED attempt's
        // real spend on `agent.model_attempt`, not just the winner's.
    ) -> (Option<(u64, u64)>, Result<ExecuteResult, ExecutorError>) {
        let (max_seconds, stall_seconds) = effective_attempt_windows(cfg, wall_cap);
        // Tool-setup bound: the step's call-level override or the 60s default,
        // clamped to this attempt's wall.
        let tool_setup_timeout = resolve_tool_setup_timeout(cfg.tool_setup_seconds, max_seconds);

        // Render the leaf's tool connections against the blackboard + the
        // run-ambient root. A `file:<root>` whose root does NOT render absolute
        // is an unresolved/mis-threaded template — e.g. a pack still on the
        // legacy `file:{{ $.workflow.input.repo_path }}` with `repo_path` unset,
        // rendering to a bare `file:`. Fail FAST at dispatch, before spending a
        // second of the step budget, instead of handing the agent an empty
        // toolbelt and letting it burn the whole budget producing nothing (the
        // v0.0.20 defect). This is the DETECTION half of the poka-yoke; the
        // PREVENTION half is the mandatory `$.run.repo_root`, which always
        // resolves to an absolute canonical dir, so a migrated leaf never trips
        // this.
        let tools: Vec<String> = cfg
            .tools
            .iter()
            .map(|t| render_template(t, &request.workflow))
            .collect();
        if let Some(bad) = tools.iter().find(|t| {
            // Both a read-write `file:` and a read-only `file-ro:` root must be
            // absolute — an unresolved template on either is a mis-threaded leaf.
            let root = t
                .strip_prefix(crate::file_tools::FILE_RO_CONNECTION_PREFIX)
                .or_else(|| t.strip_prefix(crate::file_tools::FILE_CONNECTION_PREFIX));
            root.is_some_and(|r| !std::path::Path::new(r).is_absolute())
        }) {
            return (
                None,
                Err(ExecutorError::Permanent(format!(
                    "FILE_TOOL_ROOT_UNRESOLVED: agent-leaf tool `{bad}` did not resolve to an \
                     absolute repo root (its `$.run.repo_root` / legacy \
                     `$.workflow.input.repo_path` template rendered empty or relative). A \
                     file-touching leaf requires a real root; refusing to dispatch with no \
                     filesystem rather than exhausting the step budget."
                ))),
            );
        }
        let session = AgentSession {
            model: model.to_string(),
            system_prompt: system_prompt.clone(),
            user_prompt: user_prompt.to_string(),
            tools,
            // WS1-B — `cfg.reasoning_effort` is the per-attempt effort the
            // caller resolved for THIS hop (model-paired ?? phase); the global
            // default is applied downstream by `reasoning_for` when it's None.
            reasoning_effort: cfg.reasoning_effort.clone(),
            timeout: Duration::from_secs(max_seconds),
            stall_timeout: Duration::from_secs(stall_seconds),
            tool_setup_timeout,
            expected_output_keys: cfg.expected_output_keys.clone(),
            expected_output_types: cfg.expected_output_types.clone(),
            await_enabled: cfg.await_enabled,
            // Coding-evidence forcing function: a coding leaf's `final_answer`
            // must be backed by real `write_file`/`edit_file` calls (see the
            // runner's write tally). Non-coding leaves leave this false.
            requires_file_write: cfg.requires_file_write,
            // The identity the runtime stamps on `agent.invoked` — carried so
            // the runner's in-run `agent.heartbeat` events join the same
            // workflow + correlation in the audit stream.
            identity: crate::session::RunIdentity {
                workflow_id: Some(request.workflow.id.clone()),
                correlation_id: request.correlation_id.clone(),
                transition: request.transition.clone(),
            },
        };

        let report = match self.runner.run(session).await {
            Ok(r) => r,
            Err(e) => return (None, Err(e)),
        };
        // Tokens survive the map — result_from_report consumes `report` and drops
        // them on its failure branches, so capture them first.
        let tokens = Some((report.prompt_tokens, report.completion_tokens));
        (tokens, Self::result_from_report(report, max_seconds))
    }

    /// Map one runner report onto the executor result contract — shared by the
    /// fresh-run path ([`Self::run_one`]) and the correlated-resume path, so a
    /// resumed session honors the exact same fail-fasts (FM1/FM12) and
    /// telemetry/evidence shape a fresh one does.
    fn result_from_report(
        report: crate::session::AgentRunReport,
        max_seconds: u64,
    ) -> Result<ExecuteResult, ExecutorError> {
        let model = report.model.clone();

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
            // The applied reasoning effort is resolved per-hop in the chain-walk,
            // not here (this maps a runner report). The walk stamps it onto the
            // returned telemetry on the success branch, where the hop's
            // `applied_effort` is in scope.
            effort: None,
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
            // WS3: the whole-run wall (`max_seconds`) elapsing is a BUDGET
            // outcome, not a dead-air stall — the model may have been actively
            // streaming when the ceiling hit. Surface it honestly
            // (AGENT_BUDGET_EXCEEDED → FailureClass::BudgetExceeded, still
            // escalatable so the walk is unchanged) and carry the partial work
            // rather than discarding it into a bare timeout that reads as a stall.
            AgentRunOutcome::TimedOut => Err(permanent(
                AgentErrorCode::BudgetExceeded,
                format!(
                    "agent spent its full {max_seconds}s time budget without finalizing — a \
                     spend/time ceiling, NOT a dead-air stall (the model may have been \
                     streaming). Partial work: {}",
                    partial_tail(&report.transcript)
                ),
            )),
            // WS3: split the "ended without a conforming final_answer" outcome.
            // A run that produced output (streamed text / tool calls) but never
            // finalized is NOT_CONVERGING (did work, didn't converge) — honestly
            // distinct from a genuinely empty NoResult, and carries the partial
            // work rather than discarding it. Both classify as Capability
            // (escalate to a stronger model), so the walk is unchanged.
            // Coding-evidence forcing function: a success with zero file writes,
            // after the runner's bounded in-context correction. Escalatable
            // (Capability) like NoResult/NotConverging so the chain-walk hands the
            // work to the next model — the automated "swap the lead coder". The
            // burned tokens rode in on the report (this is an OUTCOME, not a
            // runner Err), so the wasted spend is attributed to this model.
            AgentRunOutcome::NoFileWrites => Err(permanent(
                AgentErrorCode::NoFileWrites,
                format!(
                    "agent reported success on a coding deliverable but made no successful \
                     write_file/edit_file call across the run, even after in-context correction \
                     — escalating to the next model. Partial work: {}",
                    partial_tail(&report.transcript)
                ),
            )),
            AgentRunOutcome::NoResult => {
                if report.transcript.trim().is_empty() {
                    Err(permanent(
                        AgentErrorCode::NoResult,
                        "agent run ended without producing any output or a conforming \
                         `final_answer` call",
                    ))
                } else {
                    Err(permanent(
                        AgentErrorCode::NotConverging,
                        format!(
                            "agent produced output across its turns but never converged on a \
                             conforming `final_answer` — a not-converging run, not a dead one. \
                             Partial work: {}",
                            partial_tail(&report.transcript)
                        ),
                    ))
                }
            }
            // P12 R1.4 — the agent parked on a human gate. FIRST-CLASS
            // control flow, not a failure: the conversation is already
            // durably persisted under `correlation_id`, and the runtime maps
            // this suspend to the same durable waiting representation a
            // `kind: workflow` park uses (an `_agent_await` context marker +
            // a `waiting` mission status). A human resumes by re-submitting
            // the transition with `arguments.reply`. Returning `Ok` here also
            // ends the chain-walk (a suspend must never escalate to the next
            // model — that would run a duplicate agent while the parked frame
            // awaits its human).
            AgentRunOutcome::Suspended(s) => Ok(ExecuteResult {
                output: json!({}),
                evidence,
                child_workflow_id: None,
                next_transition: None,
                suspend: Some(praxec_core::model::StepSuspend::AgentAwait(
                    praxec_core::model::AgentAwaitSuspend {
                        correlation_id: s.correlation_id,
                        prompt: s.prompt,
                    },
                )),
                telemetry,
            }),
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

        // P12 R1.4 — the resume path: when THIS transition previously parked
        // on `await_human`, the workflow context carries a durable
        // `_agent_await` marker (written by the runtime's
        // `suspend_on_agent_await`, transition-identity guarded). A re-submit
        // of the transition is then a RESUME of the parked frame, not a fresh
        // run: the human's `arguments.reply` is routed to the runner's
        // correlated resume, which re-enters the exact parked tool-loop turn.
        // A re-fire without a reply fails typed (never a silent duplicate
        // agent run while the parked frame still awaits its human).
        if let Some(wait) = agent_await_marker(&request) {
            let correlation_id = wait.correlation_id;
            let reply = match request.arguments.get("reply").and_then(Value::as_str) {
                Some(r) if !r.trim().is_empty() => r,
                _ => {
                    return Err(permanent(
                        AgentErrorCode::AwaitReplyRequired,
                        format!(
                            "this transition is parked on an agent `await_human` gate \
                             (correlation_id={correlation_id}, prompt={:?}); re-submit it \
                             with a non-empty `arguments.reply` to resume the parked session",
                            wait.prompt
                        ),
                    ));
                }
            };
            let max_seconds = cfg
                .max_seconds
                .unwrap_or(DEFAULT_MAX_SECONDS)
                .min(MAX_SECONDS_CEILING);
            let report = self.runner.resume(&correlation_id, reply).await?;
            return Self::result_from_report(report, max_seconds);
        }

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
        // Entry gate (Plan A, shadow mode) — the tracked render also reports
        // any `$.`-paths that stubbed. In shadow mode dispatch still proceeds
        // regardless; enforcement (blocking on unresolved paths) is Task 4.
        let (user_prompt, unresolved) =
            praxec_core::templating::render_template_tracked(&cfg.goal, &request.workflow);
        if !unresolved.is_empty() {
            if let Some(sink) = &self.audit {
                let mut event = AuditEvent::new("agent.input_unresolved")
                    .with_workflow(request.workflow.id.clone())
                    .with_payload(json!({
                        "transition": request.transition,
                        "unresolved": unresolved,
                        "enforced": false,
                    }));
                if let Some(c) = &request.correlation_id {
                    event = event.with_correlation(c.clone());
                }
                let _ = sink.record(event).await;
            }
        }

        // Resolve the full ordered model chain (cheapest-effective first). Each
        // hop carries its model-paired reasoning effort (WS1-B).
        let chain = self.resolver.resolve_chain(&cfg.model_binding()).await?;
        // Re-associate each model with its paired effort so it survives the
        // breaker's reorder below (which works on bare model strings). Effort is
        // per-model, so keying by model is exact.
        let effort_by_model: std::collections::HashMap<String, Option<String>> = chain
            .iter()
            .map(|h| (h.model.clone(), h.effort.clone()))
            .collect();
        let chain_models: Vec<String> = chain.iter().map(|h| h.model.clone()).collect();

        // P12 — consult the per-model breaker: skip models whose breaker is
        // open (they failed ≥ threshold times recently), keeping chain order.
        // The walk starts below a known-bad primary instead of re-probing it
        // every call. If EVERYTHING is open, `plan` degrades to the
        // least-recently-failed model — never an empty walk.
        let planned = self.breaker.plan(&chain_models, Instant::now());

        let mut escalations: Vec<Evidence> = Vec::new();

        // (CR#1) The step's chain-walk budget. `walk_start` is the ONE clock the
        // whole escalation shares, so N models can't each claim a fresh wall.
        // Tokio's clock (not `std`'s) so the bound is exercisable under virtual
        // time — a wall-clock budget you can only test by actually waiting 15
        // minutes is a wall-clock budget nobody tests.
        let step_budget = Duration::from_secs(
            cfg.step_budget_seconds
                .unwrap_or(DEFAULT_STEP_BUDGET_SECONDS),
        );
        let walk_start = tokio::time::Instant::now();

        // (v0.0.28 defect 1) The walk's attempt log — one record per finished
        // attempt, so an exhausted chain surfaces WHAT WAS TRIED (model,
        // outcome class, duration, clamped windows), never just the last
        // attempt's clamped timeout.
        let mut attempts: Vec<AttemptRecord> = Vec::new();
        // The step's CONFIGURED windows (pre-clamp) — carried on the terminal
        // error so an operator can see "configured 600s wall, last attempt
        // clamped to 74s" at a glance.
        let configured_wall = cfg
            .max_seconds
            .unwrap_or(DEFAULT_MAX_SECONDS)
            .min(MAX_SECONDS_CEILING);
        let configured_stall = cfg.stall_seconds.unwrap_or(DEFAULT_STALL_SECONDS);

        for (idx, model) in planned.iter().enumerate() {
            // How much wall is left for this attempt? Checked BEFORE the run so
            // a spent budget stops the walk instead of starting an attempt that
            // could only time out.
            let remaining = step_budget.saturating_sub(walk_start.elapsed());
            // The budget governs ESCALATION, never the first attempt: model 0
            // always gets to run. Otherwise a budget configured below
            // `MIN_ATTEMPT_SECONDS` would silently no-op every agent step —
            // a knob that turns the feature off by accident is a trap, not a
            // bound. (An explicitly tiny budget still clamps attempt 0's wall
            // below; that's the operator's stated intent, honestly applied.)
            if idx > 0 && remaining.as_secs() < MIN_ATTEMPT_SECONDS {
                // Budget spent mid-walk. STOP escalating and surface. This is
                // the hard bound that turns silent multi-model churn into a
                // fast, legible hand-off: `AGENT_STEP_BUDGET_EXHAUSTED`
                // classifies as ContentOther (NOT Capability), so it does not
                // re-escalate — it routes to the flow, and on to a human.
                tracing::warn!(
                    models_tried = idx,
                    budget_seconds = step_budget.as_secs(),
                    "agent step budget exhausted mid-chain-walk; surfacing instead of escalating"
                );
                return Err(permanent(
                    AgentErrorCode::StepBudgetExhausted,
                    format!(
                        "the {}s step budget was spent after {} model attempt(s) \
                         ({}) without a result; stopping the chain-walk rather than \
                         starting another full-length attempt — this step needs a human. \
                         Walk: {}",
                        step_budget.as_secs(),
                        idx,
                        planned[..idx].join(" → "),
                        walk_summary(&attempts),
                    ),
                ));
            }
            // WS2 — cost gate. Before running this model, refuse a FRONTIER
            // (over-cap $/M) model unless a human allowlisted it. Auto-drive can
            // never unilaterally spend on a premium model — a config error, not a
            // transient, so it STOPS the walk (does not escalate). The preflight
            // `FRONTIER_LEAD` warning surfaces it statically; this is the runtime
            // enforcement. Only active when the composer injected a cap.
            if let Some(cap) = cfg.frontier_cap_usd_per_m {
                if praxec_core::model_catalog::is_frontier(model, cap)
                    && !cfg.approve_frontier.iter().any(|m| m == model)
                {
                    let usd =
                        praxec_core::model_catalog::output_usd_per_million(model).unwrap_or(0.0);
                    return Err(permanent(
                        AgentErrorCode::InvalidModelBinding,
                        format!(
                            "FRONTIER_NOT_APPROVED: model `{model}` costs ${usd:.2}/M output \
                             (≥ ${cap:.2} cap) and is not in gateway.cost.approve_frontier. \
                             Auto-drive will not spend on a premium model without a human's \
                             approval — add it to the allowlist to permit it, or bind a commodity \
                             model."
                        ),
                    ));
                }
            }
            // WS1-B — the reasoning effort APPLIED to this model, resolved
            // INDEPENDENTLY for this hop (never carried from a sibling model —
            // levels aren't portable across models): the model's own paired
            // effort wins, else the phase (`cfg`) effort, else the global
            // default. Then FAIL-FAST if the model can't do it — a level a model
            // doesn't advertise is a config error, not a transient, so it STOPS
            // the walk (does not silently downgrade, does not escalate). The
            // preflight validator catches this statically; this is the runtime
            // backstop.
            let hop_effort = effort_by_model.get(model).cloned().flatten();
            let applied_effort = hop_effort
                .clone()
                .or_else(|| cfg.reasoning_effort.clone())
                .or_else(|| {
                    let d = praxec_core::tuning::tuning()
                        .reasoning
                        .default_effort
                        .trim()
                        .to_string();
                    (!d.is_empty()).then_some(d)
                });
            if let Some(level) = &applied_effort {
                if !praxec_core::model_catalog::effort_supported(model, level) {
                    return Err(permanent(
                        AgentErrorCode::InvalidModelBinding,
                        format!(
                            "REASONING_EFFORT_UNSUPPORTED: model `{model}` is asked for reasoning \
                             effort `{level}` which it does not advertise. Pair this model with a \
                             supported effort in models.yaml (`effort:` on the binding), or change \
                             the phase/global effort — a reasoning level is not portable across \
                             models, so praxec fails fast rather than silently downgrading."
                        ),
                    ));
                }
            }
            // The per-attempt config carries THIS hop's effort: the model's
            // paired effort wins over the phase effort. `reasoning_for` applies
            // the global default downstream when both are absent. Threading it
            // via `cfg` (rather than a new `run_one` arg) keeps that signature
            // small.
            let attempt_cfg = match &hop_effort {
                Some(_) => AgentExecutorConfig {
                    reasoning_effort: hop_effort.clone(),
                    ..cfg.clone()
                },
                None => cfg.clone(),
            };

            // The clamped windows THIS attempt runs under — recorded alongside
            // the outcome so the terminal summary shows configured vs clamped.
            let (attempt_wall, attempt_stall) = effective_attempt_windows(&attempt_cfg, remaining);
            let attempt_start = tokio::time::Instant::now();
            let (attempt_tokens, attempt_outcome) = self
                .run_one(
                    model,
                    &attempt_cfg,
                    &request,
                    &system_prompt,
                    &user_prompt,
                    remaining,
                )
                .await;
            let (p_tokens, c_tokens) = match attempt_tokens {
                Some((p, c)) => (Some(p), Some(c)),
                None => (None, None),
            };
            match attempt_outcome {
                Ok(result) => {
                    self.breaker.on_success(model);
                    let outcome = if result.suspend.is_some() {
                        "suspended"
                    } else {
                        "success"
                    };
                    self.emit_model_attempt(
                        &request,
                        idx,
                        model,
                        outcome,
                        None,
                        attempt_start.elapsed(),
                        p_tokens,
                        c_tokens,
                        applied_effort.as_deref(),
                    )
                    .await;
                    let mut result = result;
                    // WS1-B (#12) — stamp THIS hop's applied reasoning effort onto
                    // the telemetry, PAIRED with the model that actually ran. On an
                    // escalated hop the walked `model` differs from the composer's
                    // intent, and effort is non-portable across models, so the
                    // de-escalation flywheel must key off the effort the winning
                    // model actually ran under — recorded here, on `agent.completed`.
                    if let Some(t) = result.telemetry.as_mut() {
                        t.effort = applied_effort.clone();
                    }
                    for e in escalations.drain(..).rev() {
                        result.evidence.insert(0, e);
                    }
                    return Ok(result);
                }
                Err(e) => {
                    let class = FailureClass::from_executor_error(&e);
                    let attempt_duration = attempt_start.elapsed();
                    self.emit_model_attempt(
                        &request,
                        idx,
                        model,
                        &format!("{class:?}"),
                        Some(&e.to_string()),
                        attempt_duration,
                        p_tokens,
                        c_tokens,
                        applied_effort.as_deref(),
                    )
                    .await;
                    attempts.push(AttemptRecord {
                        model: model.clone(),
                        outcome: format!("{class:?}"),
                        error: e.to_string(),
                        duration: attempt_duration,
                        wall_seconds: attempt_wall,
                        stall_seconds: attempt_stall,
                    });
                    // Escalatable classes (incl. Timeout / AGENT_NO_RESULT)
                    // are model-health signals — feed the breaker. Content /
                    // author errors are not the model's fault and don't count.
                    if class.is_infrastructure() {
                        self.breaker.on_failure(model, Instant::now());
                    }
                    let is_last = idx + 1 == planned.len();
                    // (v0.0.28 defect 1) The chain EXHAUSTED: the last
                    // candidate of a MULTI-model walk failed with an
                    // escalatable class and there is no next model. Surface
                    // the typed AGENT_CHAIN_EXHAUSTED with the full walk
                    // summary — never the last attempt's error alone, whose
                    // clamped "timeout after Nms" reads as a single-model
                    // timeout and hides everything that was tried. A
                    // SINGLE-model chain keeps the attempt's own error
                    // (genuine single-attempt semantics — there was no walk
                    // to summarize); a non-escalatable class still surfaces
                    // unchanged below (author/content errors are not
                    // exhaustion).
                    if class.is_infrastructure() && is_last && idx > 0 {
                        return Err(permanent(
                            AgentErrorCode::ChainExhausted,
                            format!(
                                "the full model chain was exhausted after {} attempt(s) \
                                 consuming {}s of the {}s step budget (configured wall \
                                 {}s, stall {}s) without a result — every candidate \
                                 failed with an escalatable class; this step needs a \
                                 human. Walk: {}",
                                attempts.len(),
                                walk_start.elapsed().as_secs(),
                                step_budget.as_secs(),
                                configured_wall,
                                configured_stall,
                                walk_summary(&attempts),
                            ),
                        ));
                    }
                    if class.is_infrastructure() && !is_last {
                        tracing::warn!(
                            failed_model = %model,
                            next_model = %planned[idx + 1],
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
                                planned[idx + 1],
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

    #[test]
    fn tool_setup_timeout_defaults_when_unset() {
        // No `tool_setup_seconds` on the step → the 60s default, unaffected by a
        // generous wall.
        assert_eq!(
            resolve_tool_setup_timeout(None, 600),
            Duration::from_secs(DEFAULT_TOOL_SETUP_SECONDS)
        );
    }

    #[test]
    fn tool_setup_timeout_honors_call_level_override() {
        // A step that knows its tool server is slow raises the bound per-call.
        assert_eq!(
            resolve_tool_setup_timeout(Some(180), 600),
            Duration::from_secs(180)
        );
    }

    #[test]
    fn tool_setup_timeout_clamped_to_wall() {
        // A setup bound longer than the whole wall is dead code (the wall fires
        // first) → clamp to `max_seconds`, both for an explicit override and the
        // default on a tiny wall.
        assert_eq!(
            resolve_tool_setup_timeout(Some(600), 30),
            Duration::from_secs(30)
        );
        assert_eq!(
            resolve_tool_setup_timeout(None, 10),
            Duration::from_secs(10)
        );
    }

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
            run_env: praxec_core::RunEnv::for_test(),
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
    async fn entry_gate_shadow_emits_anomaly_but_does_not_block() {
        let audit = praxec_core::audit::MemoryAuditSink::new();
        let exec = AgentExecutor::new(
            Arc::new(MockSessionRunner::completed(AgentResult {
                status: AgentStatus::Success,
                output: json!({ "verdict": "pass" }),
                internal_monologue: None,
            })),
            Arc::new(MockModelResolver("anthropic:claude-sonnet-4-6".into())),
        )
        .with_audit_sink(Arc::new(audit.clone()));
        // goal references a context key that is NOT present → renders "(missing: unset)"
        let res = exec
            .execute(request(
                json!({ "affinity": "coding", "goal": "do {{ $.context.missing }}" }),
                bare_def(),
            ))
            .await;
        // shadow mode: the run still succeeds (does NOT block)
        assert!(res.is_ok(), "shadow mode must not block: {res:?}");
        // but an anomaly was recorded, naming the unresolved path
        let anomalies: Vec<_> = audit
            .snapshot()
            .into_iter()
            .filter(|e| e.event_type == "agent.input_unresolved")
            .collect();
        assert_eq!(anomalies.len(), 1);
        assert!(
            anomalies[0].payload["unresolved"]
                .to_string()
                .contains("$.context.missing")
        );
        assert_eq!(anomalies[0].payload["enforced"], json!(false));
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
    async fn unresolved_file_root_fails_fast_at_dispatch() {
        // A leaf declares a `file:` tool whose root template does NOT resolve —
        // here the legacy `$.workflow.input.repo_path` with no `repo_path` set,
        // rendering to a non-absolute stub. The executor must REFUSE to dispatch
        // (FILE_TOOL_ROOT_UNRESOLVED) rather than hand the agent an empty
        // toolbelt and let it burn the whole step budget (the v0.0.20 defect).
        // The runner must never be invoked.
        let runner = Arc::new(MockSessionRunner::completed(AgentResult {
            status: AgentStatus::Success,
            output: json!({}),
            internal_monologue: None,
        }));
        let exec = AgentExecutor::new(
            runner.clone(),
            Arc::new(MockModelResolver("anthropic:x".into())),
        );
        // `instance()` has empty `input`, so `$.workflow.input.repo_path` does
        // not resolve to an absolute root.
        let req = request(
            json!({
                "affinity": "coding",
                "goal": "build",
                "tools": ["file:{{ $.workflow.input.repo_path }}", "engine"]
            }),
            bare_def(),
        );
        let err = exec
            .execute(req)
            .await
            .expect_err("must fail fast at dispatch");
        match err {
            ExecutorError::Permanent(m) => {
                assert!(m.contains("FILE_TOOL_ROOT_UNRESOLVED"), "{m}")
            }
            other => panic!("expected Permanent, got {other:?}"),
        }
        assert!(
            runner.sessions().is_empty(),
            "the runner must NOT be invoked when the file root is unresolved"
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

    /// WS1-B — a hop whose PAIRED effort the model can't do must fail fast
    /// (permanent `REASONING_EFFORT_UNSUPPORTED`) BEFORE the model runs — a
    /// config error is not a transient, so it neither escalates nor silently
    /// downgrades.
    #[tokio::test]
    async fn a_hop_effort_the_model_cannot_do_fails_fast_without_running_it() {
        struct EffortMismatchResolver;
        #[async_trait::async_trait]
        impl AgentModelResolver for EffortMismatchResolver {
            async fn resolve(&self, _b: &ModelBinding) -> Result<String, ExecutorError> {
                Ok("openrouter:qwen/qwen3-coder".into())
            }
            async fn resolve_chain(
                &self,
                _b: &ModelBinding,
            ) -> Result<Vec<crate::session::ResolvedHop>, ExecutorError> {
                // qwen3-coder advertises [none,low,medium]; pair it with `high`.
                Ok(vec![crate::session::ResolvedHop {
                    model: "openrouter:qwen/qwen3-coder".into(),
                    effort: Some("high".into()),
                }])
            }
        }
        let runner = Arc::new(MockSessionRunner::completed(AgentResult {
            status: AgentStatus::Success,
            output: json!({}),
            internal_monologue: None,
        }));
        let exec = AgentExecutor::new(runner.clone(), Arc::new(EffortMismatchResolver));
        let err = exec
            .execute(request(
                json!({ "affinity": "coding", "goal": "go" }),
                bare_def(),
            ))
            .await
            .expect_err("an unsupported paired effort must fail");
        assert!(
            format!("{err:?}").contains("REASONING_EFFORT_UNSUPPORTED"),
            "expected a fail-fast effort error, got {err:?}"
        );
        assert!(
            runner.sessions().is_empty(),
            "the model must NOT be run when its paired effort is unsupported"
        );
    }

    /// WS2 — the cost gate must refuse an over-cap frontier model that no human
    /// allowlisted, BEFORE running it (a config error, not a transient → stops
    /// the walk).
    #[tokio::test]
    async fn the_cost_gate_refuses_an_unapproved_frontier_model_without_running_it() {
        struct FrontierResolver;
        #[async_trait::async_trait]
        impl AgentModelResolver for FrontierResolver {
            async fn resolve(&self, _b: &ModelBinding) -> Result<String, ExecutorError> {
                Ok("anthropic:claude-opus-4-8".into())
            }
            async fn resolve_chain(
                &self,
                _b: &ModelBinding,
            ) -> Result<Vec<crate::session::ResolvedHop>, ExecutorError> {
                // claude-opus-4-8 is $25/M output — over the $5 cap.
                Ok(vec![crate::session::ResolvedHop {
                    model: "anthropic:claude-opus-4-8".into(),
                    effort: None,
                }])
            }
        }
        let runner = Arc::new(MockSessionRunner::completed(AgentResult {
            status: AgentStatus::Success,
            output: json!({}),
            internal_monologue: None,
        }));
        let exec = AgentExecutor::new(runner.clone(), Arc::new(FrontierResolver));
        let err = exec
            .execute(request(
                json!({ "affinity": "coding", "goal": "go", "frontier_cap_usd_per_m": 5.0 }),
                bare_def(),
            ))
            .await
            .expect_err("an unapproved frontier model must be refused");
        assert!(
            format!("{err:?}").contains("FRONTIER_NOT_APPROVED"),
            "expected a cost-gate refusal, got {err:?}"
        );
        assert!(
            runner.sessions().is_empty(),
            "the frontier model must NOT be run when it is not approved"
        );
    }

    /// WS2 — an allowlisted frontier model is permitted (a human opted it in).
    #[tokio::test]
    async fn the_cost_gate_permits_an_allowlisted_frontier_model() {
        struct FrontierResolver;
        #[async_trait::async_trait]
        impl AgentModelResolver for FrontierResolver {
            async fn resolve(&self, _b: &ModelBinding) -> Result<String, ExecutorError> {
                Ok("anthropic:claude-opus-4-8".into())
            }
            async fn resolve_chain(
                &self,
                _b: &ModelBinding,
            ) -> Result<Vec<crate::session::ResolvedHop>, ExecutorError> {
                Ok(vec![crate::session::ResolvedHop {
                    model: "anthropic:claude-opus-4-8".into(),
                    effort: None,
                }])
            }
        }
        let runner = Arc::new(MockSessionRunner::completed(AgentResult {
            status: AgentStatus::Success,
            output: json!({}),
            internal_monologue: None,
        }));
        let exec = AgentExecutor::new(runner.clone(), Arc::new(FrontierResolver));
        exec.execute(request(
            json!({
                "affinity": "coding", "goal": "go",
                "frontier_cap_usd_per_m": 5.0,
                "approve_frontier": ["anthropic:claude-opus-4-8"]
            }),
            bare_def(),
        ))
        .await
        .expect("an allowlisted frontier model must run");
        assert_eq!(
            runner.sessions().len(),
            1,
            "the allowlisted frontier model runs exactly once"
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
    async fn produced_output_but_no_final_answer_is_not_converging_with_partial_work() {
        // WS3: a run that emitted output but never finalized is NOT_CONVERGING
        // (honestly distinct from a genuinely-empty NoResult) and carries the
        // partial work rather than discarding it.
        let exec = exec_with(MockSessionRunner::not_converging());
        let err = exec
            .execute(request(
                json!({ "affinity": "coding", "goal": "g" }),
                bare_def(),
            ))
            .await
            .expect_err("no final_answer → error");
        let msg = format!("{err:?}");
        assert!(msg.contains("AGENT_NOT_CONVERGING"), "got {msg}");
        assert!(
            msg.contains("Partial work"),
            "partial work must be surfaced, not discarded: {msg}"
        );
        assert!(
            !msg.contains("AGENT_NO_RESULT"),
            "a run that did work must not be mislabeled NoResult: {msg}"
        );
    }

    #[tokio::test]
    async fn no_file_writes_outcome_maps_to_escalatable_error_with_partial_work() {
        // Coding-evidence forcing function: the runner reports the NoFileWrites
        // OUTCOME; the executor maps it to the escalatable AGENT_NO_FILE_WRITES
        // and carries the partial work (never a bare NoResult).
        let exec = exec_with(MockSessionRunner::no_file_writes());
        let err = exec
            .execute(request(
                json!({ "affinity": "coding", "goal": "g" }),
                bare_def(),
            ))
            .await
            .expect_err("a zero-write coding success → escalatable error");
        let msg = format!("{err:?}");
        assert!(msg.contains("AGENT_NO_FILE_WRITES"), "got {msg}");
        assert!(
            msg.contains("Partial work"),
            "partial work must be surfaced: {msg}"
        );
    }

    /// Honest accounting: because NoFileWrites is an OUTCOME (not a runner Err),
    /// the burned tokens survive onto the `agent.model_attempt` event — so the
    /// wasted spend is attributed to the model that narrated instead of writing,
    /// exactly the "glm burned $1.88 for nothing" case the flywheel must see.
    #[tokio::test]
    async fn a_no_file_writes_attempt_records_its_burned_tokens() {
        let audit = praxec_core::audit::MemoryAuditSink::new();
        let exec = AgentExecutor::new(
            Arc::new(MockSessionRunner::no_file_writes()),
            Arc::new(MockModelResolver("anthropic:claude-sonnet-4-6".into())),
        )
        .with_audit_sink(Arc::new(audit.clone()));
        let _ = exec
            .execute(request(
                json!({ "affinity": "coding", "goal": "g" }),
                bare_def(),
            ))
            .await
            .expect_err("zero-write coding success escalates");

        let attempts: Vec<_> = audit
            .snapshot()
            .into_iter()
            .filter(|e| e.event_type == AGENT_MODEL_ATTEMPT_EVENT)
            .collect();
        assert_eq!(attempts.len(), 1, "one failed attempt recorded");
        assert_eq!(attempts[0].payload["outcome"], json!("Capability"));
        assert!(
            attempts[0].payload["error"]
                .as_str()
                .is_some_and(|e| e.contains("AGENT_NO_FILE_WRITES")),
            "carries its typed error: {:#}",
            attempts[0].payload
        );
        assert_eq!(
            attempts[0].payload["prompt_tokens"],
            json!(800),
            "the burned tokens must survive the outcome→error map (honest accounting)"
        );
        assert_eq!(attempts[0].payload["completion_tokens"], json!(200));
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
    async fn timeout_maps_to_budget_exceeded() {
        // WS3: a spent time budget surfaces AGENT_BUDGET_EXCEEDED (a spend/time
        // ceiling), NOT a dead-air ExecutorError::Timeout — the model may have
        // been actively streaming when the wall hit.
        let exec = exec_with(MockSessionRunner::timed_out());
        let err = exec
            .execute(request(
                json!({ "affinity": "coding", "goal": "g" }),
                bare_def(),
            ))
            .await
            .expect_err("timeout");
        assert!(
            format!("{err:?}").contains("AGENT_BUDGET_EXCEEDED"),
            "got {err:?}"
        );
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
        ) -> Result<Vec<crate::session::ResolvedHop>, ExecutorError> {
            Ok(["openrouter:weak", "openrouter:strong"]
                .into_iter()
                .map(|m| crate::session::ResolvedHop {
                    model: m.to_string(),
                    effort: None,
                })
                .collect())
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

    // ── (CR#1) the chain-walk wall-clock budget ──────────────────────────

    /// Three reasoning models, all of which will fail to sign off — the live
    /// fix-loop chain (deepseek → glm → qwen-thinking).
    struct ThreeModelResolver;
    #[async_trait]
    impl AgentModelResolver for ThreeModelResolver {
        async fn resolve(&self, _binding: &ModelBinding) -> Result<String, ExecutorError> {
            Ok("openrouter:reason-1".into())
        }
        async fn resolve_chain(
            &self,
            _binding: &ModelBinding,
        ) -> Result<Vec<crate::session::ResolvedHop>, ExecutorError> {
            Ok([
                "openrouter:reason-1",
                "openrouter:reason-2",
                "openrouter:reason-3",
            ]
            .into_iter()
            .map(|m| crate::session::ResolvedHop {
                model: m.to_string(),
                effort: None,
            })
            .collect())
        }
    }

    /// Every model burns its ENTIRE granted wall and then returns NoResult —
    /// the observed reasoning-model fix-loop signature. Sleeping exactly
    /// `session.timeout` is what makes this a faithful mock: it proves the
    /// executor's clamp on each attempt's wall is what bounds the walk.
    struct BudgetBurningRunner {
        seen: std::sync::Mutex<Vec<AgentSession>>,
    }
    impl BudgetBurningRunner {
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
    impl AgentSessionRunner for BudgetBurningRunner {
        async fn run(&self, session: AgentSession) -> Result<AgentRunReport, ExecutorError> {
            self.seen.lock().unwrap().push(session.clone());
            tokio::time::sleep(session.timeout).await;
            Ok(AgentRunReport {
                outcome: AgentRunOutcome::NoResult,
                transcript: String::new(),
                model: session.model.clone(),
                prompt_tokens: 0,
                completion_tokens: 0,
            })
        }
    }

    /// THE CR#1 REGRESSION TEST. A chain of reasoning models that all return
    /// `AGENT_NO_RESULT` used to give EACH model its own full `max_seconds`
    /// wall — 3 × 600s = 1800s of silent churn with no forward progress and no
    /// state transition (the reported fix-loop stall).
    ///
    /// With the step budget, the WHOLE walk is bounded by one shared clock:
    /// attempt 1 gets 600s (its own wall, the smaller bound), attempt 2 is
    /// clamped to the 300s that remain, and the budget is then spent — so the
    /// walk STOPS and surfaces `AGENT_STEP_BUDGET_EXHAUSTED` instead of
    /// starting a third full-length attempt. Total wall == the budget, exactly.
    #[tokio::test(start_paused = true)]
    async fn an_all_no_result_chain_walk_is_bounded_by_the_step_budget() {
        let runner = Arc::new(BudgetBurningRunner::new());
        let exec = AgentExecutor::new(runner.clone(), Arc::new(ThreeModelResolver));
        let started = tokio::time::Instant::now();

        let err = exec
            .execute(request(
                json!({
                    "affinity": "coding",
                    "goal": "fix the failing test",
                    "max_seconds": 600,
                    "step_budget_seconds": 900,
                }),
                bare_def(),
            ))
            .await
            .expect_err("an all-NoResult chain must not silently churn to success");

        match &err {
            ExecutorError::Permanent(msg) => assert!(
                msg.starts_with("AGENT_STEP_BUDGET_EXHAUSTED"),
                "a spent step budget must surface as its own terminal code, not \
                 another NoResult — got: {msg}"
            ),
            other => panic!("expected a Permanent budget error, got {other:?}"),
        }

        // It must SURFACE, not re-escalate: ContentOther, never Capability.
        let class = FailureClass::from_executor_error(&err);
        assert_eq!(
            class,
            FailureClass::ContentOther,
            "the escalation layer running out of budget must route to a human, \
             not hand another layer a reason to escalate again"
        );
        assert!(
            !class.is_infrastructure(),
            "AGENT_STEP_BUDGET_EXHAUSTED must never be chain-escalated"
        );

        // The hard bound actually held: 2 attempts, not 3, and the total wall
        // is the budget — not 3 × max_seconds.
        let sessions = runner.sessions();
        assert_eq!(
            sessions.len(),
            2,
            "the third full-length attempt must never start once the budget is spent"
        );
        assert_eq!(
            sessions[0].timeout,
            Duration::from_secs(600),
            "attempt 1 gets its own max_seconds wall (the smaller of the two bounds)"
        );
        assert_eq!(
            sessions[1].timeout,
            Duration::from_secs(300),
            "attempt 2 is clamped to the budget REMAINING — without this clamp the \
             budget would bound nothing, because each escalation would start a fresh \
             full-length wall"
        );
        assert_eq!(
            started.elapsed(),
            Duration::from_secs(900),
            "the whole chain-walk is bounded by the budget (was 3 × 600s = 1800s)"
        );
    }

    /// Poka-yoke: the budget governs ESCALATION, never the first attempt. A
    /// budget configured below `MIN_ATTEMPT_SECONDS` must still run model 0
    /// (clamped) — a knob that silently turns every agent step into a no-op
    /// would be a trap, not a bound.
    #[tokio::test(start_paused = true)]
    async fn a_tiny_budget_still_runs_the_first_attempt_rather_than_no_opping_the_step() {
        let runner = Arc::new(BudgetBurningRunner::new());
        let exec = AgentExecutor::new(runner.clone(), Arc::new(ThreeModelResolver));

        let err = exec
            .execute(request(
                json!({
                    "affinity": "coding",
                    "goal": "fix the failing test",
                    "max_seconds": 600,
                    "step_budget_seconds": 5, // below MIN_ATTEMPT_SECONDS
                }),
                bare_def(),
            ))
            .await
            .expect_err("the runner never signs off, so the step still fails");

        let sessions = runner.sessions();
        assert_eq!(
            sessions.len(),
            1,
            "model 0 must still be attempted under a sub-minimum budget (and only model 0)"
        );
        assert_eq!(
            sessions[0].timeout,
            Duration::from_secs(5),
            "the first attempt's wall is clamped to the operator's stated budget — \
             honestly applied, not silently ignored"
        );
        assert!(
            matches!(&err, ExecutorError::Permanent(m) if m.starts_with("AGENT_STEP_BUDGET_EXHAUSTED")),
            "and the walk then stops on the spent budget: {err:?}"
        );
    }

    /// P12 R1.4 — a Suspended outcome is FIRST-CLASS: the executor returns
    /// `Ok(ExecuteResult)` whose `suspend` carries the AgentAwait source with
    /// the correlation_id (the runtime then parks the mission `waiting`), and
    /// it is NOT chain-escalated: with a two-model chain, the second model
    /// must never run (that would start a duplicate agent while the parked
    /// frame awaits its human reply).
    #[tokio::test]
    async fn a_suspended_agent_returns_a_first_class_suspend_and_is_not_escalated() {
        let runner = Arc::new(MockSessionRunner::suspended("corr-42", "approve the plan?"));
        let exec = AgentExecutor::new(runner.clone(), Arc::new(TwoModelResolver));
        let result = exec
            .execute(request(
                json!({ "affinity": "coding", "goal": "do something", "await_enabled": true }),
                bare_def(),
            ))
            .await
            .expect("a suspension is first-class control flow, not an error");
        let suspend = result
            .suspend
            .expect("a suspended agent must return ExecuteResult.suspend");
        let awaiting = suspend
            .as_agent_await()
            .expect("an agent suspension is the AgentAwait source");
        assert_eq!(
            awaiting.correlation_id, "corr-42",
            "the resume handle (correlation_id) must be carried"
        );
        assert_eq!(awaiting.prompt, "approve the plan?");
        let sessions = runner.sessions();
        assert_eq!(
            sessions.len(),
            1,
            "a suspension must never escalate to the next model in the chain"
        );
        // And the opt-in flag reached the session.
        assert!(
            sessions[0].await_enabled,
            "await_enabled must plumb through"
        );
    }

    // ── P12 R1.4 — the correlated-resume path ────────────────────────────

    /// The `_agent_await` marker for `transition` in the workflow context.
    fn awaiting_request(reply_args: serde_json::Value) -> ExecuteRequest {
        let mut req = request(
            json!({ "affinity": "coding", "goal": "do something", "await_enabled": true }),
            bare_def(),
        );
        req.transition = Some("do_work".into());
        req.arguments = reply_args;
        req.workflow.context = json!({
            "_agent_await": {
                "correlation_id": "corr-42",
                "prompt": "approve the plan?",
                "transition": "do_work",
            }
        });
        req
    }

    /// A re-submit of a parked transition WITH `arguments.reply` routes to the
    /// runner's correlated `resume` — never a fresh `run` (which would start a
    /// duplicate agent).
    #[tokio::test]
    async fn a_reply_on_a_parked_transition_resumes_the_exact_frame() {
        let runner = Arc::new(
            MockSessionRunner::suspended("unused", "unused").with_resume_outcome(
                AgentRunOutcome::Completed(AgentResult {
                    status: AgentStatus::Success,
                    output: json!({ "verdict": "shipped" }),
                    internal_monologue: None,
                }),
            ),
        );
        let exec = AgentExecutor::new(runner.clone(), Arc::new(TwoModelResolver));
        let result = exec
            .execute(awaiting_request(json!({ "reply": "yes, approved" })))
            .await
            .expect("resume completes the step");
        assert_eq!(result.output, json!({ "verdict": "shipped" }));
        assert_eq!(
            runner.resumes(),
            vec![("corr-42".to_string(), "yes, approved".to_string())],
            "the reply must route to the parked frame's correlation_id"
        );
        assert!(
            runner.sessions().is_empty(),
            "a resume must never start a fresh session"
        );
    }

    /// A re-fire of a parked transition WITHOUT a reply fails typed — never a
    /// silent fresh run alongside the still-parked frame.
    #[tokio::test]
    async fn a_parked_transition_without_a_reply_fails_typed() {
        let runner = Arc::new(MockSessionRunner::suspended("unused", "unused"));
        let exec = AgentExecutor::new(runner.clone(), Arc::new(TwoModelResolver));
        let err = exec
            .execute(awaiting_request(json!({})))
            .await
            .expect_err("no reply → typed refusal");
        let msg = format!("{err:?}");
        assert!(msg.contains("AGENT_AWAIT_REPLY_REQUIRED"), "got {msg}");
        assert!(msg.contains("corr-42"), "carries the resume handle: {msg}");
        assert!(runner.sessions().is_empty(), "must not run");
        assert!(runner.resumes().is_empty(), "must not resume without reply");
    }

    /// An `_agent_await` belonging to a DIFFERENT transition is ignored — the
    /// dispatched transition runs fresh (transition-identity guard).
    #[tokio::test]
    async fn an_await_marker_for_another_transition_does_not_hijack_a_fresh_run() {
        let runner = Arc::new(MockSessionRunner::completed(AgentResult {
            status: AgentStatus::Success,
            output: json!({ "ok": true }),
            internal_monologue: None,
        }));
        let exec = AgentExecutor::new(
            runner.clone(),
            Arc::new(MockModelResolver("anthropic:x".into())),
        );
        let mut req = awaiting_request(json!({}));
        req.transition = Some("other_step".into());
        let result = exec.execute(req).await.expect("fresh run");
        assert_eq!(result.output, json!({ "ok": true }));
        assert_eq!(runner.sessions().len(), 1, "fresh run happened");
        assert!(runner.resumes().is_empty());
    }

    /// A resume that itself re-suspends surfaces as a NEW first-class suspend
    /// under the fresh correlation_id (the runtime re-parks the mission).
    #[tokio::test]
    async fn a_resume_that_resuspends_carries_the_new_correlation() {
        let runner = Arc::new(
            MockSessionRunner::suspended("unused", "unused").with_resume_outcome(
                AgentRunOutcome::Suspended(crate::session::AgentSuspension {
                    correlation_id: "corr-43".into(),
                    prompt: "and the budget?".into(),
                }),
            ),
        );
        let exec = AgentExecutor::new(runner.clone(), Arc::new(TwoModelResolver));
        let result = exec
            .execute(awaiting_request(json!({ "reply": "yes" })))
            .await
            .expect("a re-suspension is first-class");
        let suspend = result.suspend.expect("suspend present");
        let awaiting = suspend.as_agent_await().expect("AgentAwait source");
        assert_eq!(awaiting.correlation_id, "corr-43");
        assert_eq!(awaiting.prompt, "and the budget?");
    }

    /// `await_enabled` defaults off: an ordinary config yields a session that
    /// cannot suspend.
    #[tokio::test]
    async fn await_enabled_defaults_to_false_in_the_session() {
        let runner = Arc::new(MockSessionRunner::completed(AgentResult {
            status: AgentStatus::Success,
            output: json!({}),
            internal_monologue: None,
        }));
        let exec = AgentExecutor::new(
            runner.clone(),
            Arc::new(MockModelResolver("anthropic:x".into())),
        );
        exec.execute(request(
            json!({ "affinity": "coding", "goal": "build" }),
            bare_def(),
        ))
        .await
        .expect("success");
        assert!(!runner.sessions()[0].await_enabled);
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

    // ── P12 — per-model cooldown breaker (cross-call) ────────────────────

    /// A runner where the weak model always times out (escalatable) and the
    /// strong model succeeds — stands in for a persistently-down primary.
    struct StallingWeakRunner {
        seen: std::sync::Mutex<Vec<AgentSession>>,
    }
    impl StallingWeakRunner {
        fn new() -> Self {
            Self {
                seen: std::sync::Mutex::new(Vec::new()),
            }
        }
        fn models_tried(&self) -> Vec<String> {
            self.seen
                .lock()
                .unwrap()
                .iter()
                .map(|s| s.model.clone())
                .collect()
        }
    }
    #[async_trait]
    impl AgentSessionRunner for StallingWeakRunner {
        async fn run(&self, session: AgentSession) -> Result<AgentRunReport, ExecutorError> {
            self.seen.lock().unwrap().push(session.clone());
            let outcome = if session.model == "openrouter:weak" {
                AgentRunOutcome::TimedOut
            } else {
                AgentRunOutcome::Completed(AgentResult {
                    status: AgentStatus::Success,
                    output: json!({ "result": "done" }),
                    internal_monologue: None,
                })
            };
            Ok(AgentRunReport {
                outcome,
                transcript: String::new(),
                model: session.model.clone(),
                prompt_tokens: 0,
                completion_tokens: 0,
            })
        }
    }

    #[tokio::test]
    async fn breaker_skips_a_repeatedly_timing_out_model_across_calls() {
        // Calls 1 and 2: weak times out (escalatable) → escalate to strong,
        // succeed. That is BREAKER_FAILURE_THRESHOLD (2) consecutive failures,
        // so on call 3 the breaker is open and the walk must start at strong —
        // no re-probe, no re-timeout of the known-bad primary.
        let runner = Arc::new(StallingWeakRunner::new());
        let exec = AgentExecutor::new(runner.clone(), Arc::new(TwoModelResolver));
        let req = || {
            request(
                json!({ "affinity": "coding", "goal": "do something" }),
                bare_def(),
            )
        };

        for _ in 0..3 {
            let result = exec.execute(req()).await.expect("strong model succeeds");
            assert_eq!(result.output, json!({ "result": "done" }));
        }

        assert_eq!(
            runner.models_tried(),
            vec![
                "openrouter:weak",   // call 1: probe weak → timeout
                "openrouter:strong", //         escalate → success
                "openrouter:weak",   // call 2: probe weak → timeout (opens breaker)
                "openrouter:strong", //         escalate → success
                "openrouter:strong", // call 3: weak skipped by the open breaker
            ],
            "the third call must skip the weak model entirely"
        );
    }

    #[tokio::test]
    async fn single_model_chain_with_open_breaker_still_attempts_it() {
        // Degrade path end-to-end: fail the ONLY model past the threshold,
        // then call again — the walk must still attempt it (degrade, don't
        // leave the drive with zero models), and surface its error.
        let runner = Arc::new(StallingWeakRunner::new());
        struct WeakOnlyResolver;
        #[async_trait]
        impl AgentModelResolver for WeakOnlyResolver {
            async fn resolve(&self, _binding: &ModelBinding) -> Result<String, ExecutorError> {
                Ok("openrouter:weak".into())
            }
        }
        let exec = AgentExecutor::new(runner.clone(), Arc::new(WeakOnlyResolver));
        let req = || {
            request(
                json!({ "affinity": "coding", "goal": "do something" }),
                bare_def(),
            )
        };

        for _ in 0..3 {
            let err = exec
                .execute(req())
                .await
                .expect_err("weak always times out");
            assert!(
                format!("{err:?}").contains("AGENT_BUDGET_EXCEEDED"),
                "got {err:?}"
            );
        }

        assert_eq!(
            runner.models_tried().len(),
            3,
            "an all-open chain must still yield one attempt per call"
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

    // ── chain exhaustion (v0.0.28 dogfood defect 1) ──────────────────────

    /// Every model in the chain times out — the whole walk exhausts. Stands in
    /// for the observed incident: successive attempts consumed the step wall and
    /// the LAST attempt's clamped `timeout after 74000 ms` surfaced as the
    /// terminal error for the WHOLE chain, with zero evidence of what was tried.
    struct AllTimeoutRunner {
        seen: std::sync::Mutex<Vec<AgentSession>>,
    }
    impl AllTimeoutRunner {
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
    impl AgentSessionRunner for AllTimeoutRunner {
        async fn run(&self, session: AgentSession) -> Result<AgentRunReport, ExecutorError> {
            self.seen.lock().unwrap().push(session.clone());
            Ok(AgentRunReport {
                outcome: AgentRunOutcome::TimedOut,
                transcript: String::new(),
                model: session.model.clone(),
                prompt_tokens: 0,
                completion_tokens: 0,
            })
        }
    }

    /// THE DEFECT-1 REGRESSION TEST. When a multi-model chain-walk EXHAUSTS
    /// (the last candidate fails with an escalatable class), the terminal
    /// error must be a typed `AGENT_CHAIN_EXHAUSTED` carrying the walk
    /// summary — every model tried, each attempt's outcome class — NEVER the
    /// last attempt's clamped `timeout after Nms`, which reads as a
    /// single-model timeout and hides the whole walk from the operator.
    #[tokio::test]
    async fn an_exhausted_multi_model_chain_surfaces_chain_exhausted_with_the_walk_summary() {
        let runner = Arc::new(AllTimeoutRunner::new());
        let exec = AgentExecutor::new(runner.clone(), Arc::new(TwoModelResolver));
        let err = exec
            .execute(request(
                json!({ "affinity": "coding", "goal": "do something" }),
                bare_def(),
            ))
            .await
            .expect_err("an all-timeout chain must fail");
        let msg = match &err {
            ExecutorError::Permanent(m) => m.clone(),
            other => panic!(
                "chain exhaustion must be the typed Permanent AGENT_CHAIN_EXHAUSTED, \
                 not the last attempt's error — got {other:?}"
            ),
        };
        assert!(
            msg.starts_with("AGENT_CHAIN_EXHAUSTED"),
            "the terminal error must LEAD with the exhaustion class: {msg}"
        );
        // The walk summary carries every model attempted…
        assert!(msg.contains("openrouter:weak"), "walk summary: {msg}");
        assert!(msg.contains("openrouter:strong"), "walk summary: {msg}");
        // …and each attempt's outcome class (WS3: TimedOut → BudgetExceeded — a
        // spent time budget, honestly distinct from a dead-air NetworkTimeout
        // stall).
        assert!(
            msg.contains("BudgetExceeded"),
            "each attempt's outcome class must be carried: {msg}"
        );
        // Both models were actually attempted.
        assert_eq!(runner.sessions().len(), 2);
        // It must SURFACE (route to the flow / a human), never re-escalate.
        let class = FailureClass::from_executor_error(&err);
        assert_eq!(
            class,
            FailureClass::ContentOther,
            "an exhausted chain must not hand another layer a reason to escalate"
        );
    }

    /// WS3: a ONE-model chain that spends its whole time budget surfaces the
    /// honest AGENT_BUDGET_EXCEEDED (a spend/time ceiling) — still distinct from
    /// a dead-air stall, even in the single-attempt case where there is no walk
    /// to summarize.
    #[tokio::test]
    async fn a_single_model_chain_timeout_surfaces_budget_exceeded() {
        let runner = Arc::new(AllTimeoutRunner::new());
        let exec = AgentExecutor::new(
            runner.clone(),
            Arc::new(MockModelResolver("openrouter:only".into())),
        );
        let err = exec
            .execute(request(
                json!({ "affinity": "coding", "goal": "do something" }),
                bare_def(),
            ))
            .await
            .expect_err("timeout");
        assert!(
            format!("{err:?}").contains("AGENT_BUDGET_EXCEEDED"),
            "a single-attempt budget-cut must surface as BudgetExceeded, not a stall: {err:?}"
        );
        assert_eq!(runner.sessions().len(), 1);
    }

    /// The budget-exhausted terminal (mid-walk stop) must carry the same
    /// per-attempt evidence — every model tried with its outcome class — not
    /// just the bare model list.
    #[tokio::test(start_paused = true)]
    async fn a_spent_step_budget_reports_every_attempt_with_its_outcome() {
        let runner = Arc::new(BudgetBurningRunner::new());
        let exec = AgentExecutor::new(runner.clone(), Arc::new(ThreeModelResolver));
        let err = exec
            .execute(request(
                json!({
                    "affinity": "coding",
                    "goal": "fix the failing test",
                    "max_seconds": 600,
                    "step_budget_seconds": 900,
                }),
                bare_def(),
            ))
            .await
            .expect_err("budget spent");
        let msg = match &err {
            ExecutorError::Permanent(m) => m.clone(),
            other => panic!("expected Permanent, got {other:?}"),
        };
        assert!(msg.starts_with("AGENT_STEP_BUDGET_EXHAUSTED"), "{msg}");
        assert!(msg.contains("openrouter:reason-1"), "walk evidence: {msg}");
        assert!(msg.contains("openrouter:reason-2"), "walk evidence: {msg}");
        // NoResult classifies Capability — the summary names the class.
        assert!(
            msg.contains("Capability"),
            "each attempt's outcome class must be carried: {msg}"
        );
    }

    // ── per-attempt audit telemetry (v0.0.28 dogfood defect 2) ───────────

    /// Every model attempt of the chain-walk must land ONE
    /// `agent.model_attempt` audit event under the step's workflow id +
    /// correlation, carrying {model, outcome, duration_ms, attempt_index} —
    /// so an operator can tell a provider outage from a hang-prone lead
    /// model without reading process logs.
    #[tokio::test]
    async fn every_model_attempt_lands_a_typed_audit_event() {
        let audit = praxec_core::audit::MemoryAuditSink::new();
        let runner = Arc::new(EscalatingRunner::new());
        let exec = AgentExecutor::new(runner, Arc::new(TwoModelResolver))
            .with_audit_sink(Arc::new(audit.clone()));
        let mut req = request(
            json!({ "affinity": "coding", "goal": "do something" }),
            bare_def(),
        );
        req.correlation_id = Some("cor_attempt_test".into());
        exec.execute(req).await.expect("strong model succeeds");

        let attempts: Vec<_> = audit
            .snapshot()
            .into_iter()
            .filter(|e| e.event_type == AGENT_MODEL_ATTEMPT_EVENT)
            .collect();
        assert_eq!(
            attempts.len(),
            2,
            "one event per attempt (weak failed, strong succeeded)"
        );

        // Attempt 0: the weak model's NoResult → Capability.
        assert_eq!(attempts[0].payload["attempt_index"], json!(0));
        assert_eq!(attempts[0].payload["model"], json!("openrouter:weak"));
        assert_eq!(attempts[0].payload["outcome"], json!("Capability"));
        assert!(
            attempts[0].payload["error"]
                .as_str()
                .is_some_and(|e| e.contains("AGENT_NO_RESULT")),
            "the failed attempt carries its error: {:#}",
            attempts[0].payload
        );
        assert!(attempts[0].payload["duration_ms"].is_u64());
        assert_eq!(
            attempts[0].workflow_id.as_deref(),
            Some("wf_agent"),
            "attempt events join the child workflow's id"
        );
        assert_eq!(
            attempts[0].correlation_id, "cor_attempt_test",
            "attempt events join the step's correlation"
        );

        // Attempt 1: the strong model succeeded.
        assert_eq!(attempts[1].payload["attempt_index"], json!(1));
        assert_eq!(attempts[1].payload["model"], json!("openrouter:strong"));
        assert_eq!(attempts[1].payload["outcome"], json!("success"));
        assert_eq!(attempts[1].payload["error"], json!(null));
    }
}
