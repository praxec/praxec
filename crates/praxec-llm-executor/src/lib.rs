// T26 — restriction-category lint on production code only.
#![cfg_attr(not(test), warn(clippy::unwrap_used))]

//! praxec-llm-executor — SPEC §33 in-runtime LLM executor.
//!
//! This crate hosts the `LlmExecutor` that turns one workflow state's
//! LLM-driven transition into a real audited dispatch. The runtime drives
//! the loop (see SPEC §33 plan, D3): each `execute()` call runs ONE LLM
//! turn and returns a `NextTransition` via `ExecuteResult.next_transition`;
//! the runtime's submit pipeline chains into another submit cycle.
//!
//! Phase B progression:
//! - D0 landed the crate skeleton.
//! - D4 wired the executor shell + config parser.
//! - **D5 (this commit)** wires the real aether-llm flow: build a
//!   `Context`, drain the stream, validate, and return the chosen
//!   `NextTransition`. Apply-caps and emit-audit are still stub-shaped
//!   call sites — D6 / D7 fill them in without touching `execute()`.

pub mod affinity;
pub mod audit;
pub mod caps;
pub mod config;
pub mod config_doctor;
pub mod cost;
pub mod pool_execution;
pub mod prompt;
pub mod provider_factory;
pub mod response;
mod skills;
pub mod stream_event;

use std::sync::Arc;

use async_trait::async_trait;
use praxec_core::audit::AuditSink;
use praxec_core::error::{ExecutorError, LlmErrorCode};
use praxec_core::model::{ExecuteRequest, ExecuteResult, NextTransition, Principal};
use praxec_core::ports::{Executor, TransitionResolver};
use serde_json::{Value, json};

pub use affinity::{AffinityResolver, RejectingAffinityResolver};
pub use audit::InvocationContext;
pub use config::LlmExecutorConfig;
pub use provider_factory::{DefaultProviderFactory, ProviderFactory, TurnRequest};
pub use response::DrainedResponse;
pub use stream_event::{StopReason, StreamEvent, TokenUsage, ToolCallRequest};

/// SPEC §33 — the in-runtime LLM executor.
///
/// Holds the long-lived collaborators every turn needs:
///
/// - `audit` — sink for `llm.invocation` events (D7 fills the emitter).
/// - `transition_resolver` — runtime-backed view of the per-state,
///   guard-filtered transition list. Each link becomes one provider tool.
/// - `provider_factory` — builds the streaming provider for the resolved
///   `provider:model-id` string. Production wires
///   [`DefaultProviderFactory`] (anthropic/openai/gemini/openrouter/ollama);
///   integration tests inject a mock factory that returns canned
///   `LlmResponse` streams.
pub struct LlmExecutor {
    audit: Arc<dyn AuditSink>,
    transition_resolver: Arc<dyn TransitionResolver>,
    provider_factory: Arc<dyn ProviderFactory>,
    affinity_resolver: Arc<dyn affinity::AffinityResolver>,
}

impl LlmExecutor {
    /// Build an executor with the production provider factory
    /// ([`DefaultProviderFactory`]). Convenience for the binary; tests
    /// should prefer [`Self::with_provider_factory`].
    pub fn new(
        audit: Arc<dyn AuditSink>,
        transition_resolver: Arc<dyn TransitionResolver>,
    ) -> Self {
        Self::with_provider_factory(
            audit,
            transition_resolver,
            Arc::new(DefaultProviderFactory) as Arc<dyn ProviderFactory>,
        )
    }

    /// Build an executor over the given collaborators with an explicit
    /// provider factory. SPEC §33 D9 — tests inject an adversarial mock
    /// factory here to exercise every `LlmResponse` variant without ever
    /// touching the network.
    pub fn with_provider_factory(
        audit: Arc<dyn AuditSink>,
        transition_resolver: Arc<dyn TransitionResolver>,
        provider_factory: Arc<dyn ProviderFactory>,
    ) -> Self {
        Self {
            audit,
            transition_resolver,
            provider_factory,
            affinity_resolver: Arc::new(affinity::RejectingAffinityResolver),
        }
    }

    /// Inject the affinity → concrete-model resolver. The default is the
    /// fail-loud [`affinity::RejectingAffinityResolver`]; the gateway binary
    /// swaps in the models.yaml-backed resolver here (when
    /// `gateway.models_yaml` is configured) so `affinity:` configs resolve to a
    /// concrete `provider:model-id` string.
    pub fn with_affinity_resolver(mut self, r: Arc<dyn affinity::AffinityResolver>) -> Self {
        self.affinity_resolver = r;
        self
    }

    /// SPEC §33 D6 — synthetic `_llm.*` slot read + cumulative cap
    /// check. Reads `_llm.cumulative_tokens`,
    /// `_llm.cumulative_cost_usd`, `_llm.cumulative_iterations`,
    /// `_llm.consecutive_no_tool_call`, and `_llm.session.<state>.started_at`
    /// out of `request.workflow.context` and runs the four pre-turn
    /// gates documented on [`caps::apply_caps`].
    async fn apply_cumulative_caps(
        &self,
        request: &ExecuteRequest,
        config: &LlmExecutorConfig,
    ) -> Result<(), ExecutorError> {
        let snapshot = caps::read_snapshot(&request.workflow, &request.workflow.state);
        caps::apply_caps(&snapshot, config, chrono::Utc::now())
    }

    /// SPEC §33 D7 — emit a `llm.invocation` audit event built from
    /// the per-turn context and the drained response. Fires exactly
    /// once per turn, on BOTH the success and the failure path
    /// (audit-before-error pattern: the validate-error caller emits
    /// with `ctx.error_code = Some(_)` before returning the Err).
    async fn emit_invocation_audit(
        &self,
        ctx: InvocationContext<'_>,
        drained: &DrainedResponse,
    ) -> Result<(), anyhow::Error> {
        let event = audit::build_invocation_event(ctx, drained);
        self.audit.record(event).await
    }
}

/// SPEC §33 — the in-runtime LLM executor has no per-request human
/// principal threaded through `ExecuteRequest` (a deliberate scope
/// decision for D5; see SPEC §33 plan). Until that field lands, every
/// resolver call uses this synthetic agent principal. The transition
/// resolver's guard-filter pass treats agent principals as non-human;
/// the closed-by-design FMECA F3 list stays empty.
///
/// Subject prefix matches the audit `actor` written by
/// [`LlmExecutor::emit_invocation_audit`] so operators reading the
/// audit log can trace tool-list narrowing back to the same identity.
fn synthetic_agent_principal() -> Principal {
    Principal {
        subject: "agent:llm-executor".to_string(),
        roles: Vec::new(),
        permissions: Vec::new(),
    }
}

/// Assemble a [`TurnRequest`] from the optional skill system message + the
/// rendered prompt + tool list + optional reasoning effort.
///
/// Per the agent/skill/prompt contract: the skill bodies (when any are in scope)
/// become the SYSTEM preamble; the rendered `prompt_template` is always the USER
/// message. The reasoning effort is mapped to the provider's native
/// `additional_params` via the shared `core::tuning` config, keyed off the
/// vendor in `model_str`.
///
/// NOTE: the aether-llm `prompt_cache_key` (skill-caching optimization, §33.12)
/// has no uniform rig equivalent — a follow-up wires Anthropic `cache_control`
/// via `additional_params`. Governance (caps/audit/reliability) is unaffected.
fn build_turn(
    system: Option<String>,
    prompt: String,
    tools: Vec<rig::completion::ToolDefinition>,
    model_str: &str,
    reasoning_effort: Option<&str>,
) -> TurnRequest {
    let vendor = model_str.split_once(':').map(|(v, _)| v).unwrap_or("");
    let reasoning =
        reasoning_effort.and_then(|level| praxec_core::tuning::reasoning_params(vendor, level));
    TurnRequest {
        system,
        prompt: rig::completion::Message::user(prompt),
        tools,
        history: vec![],
        reasoning,
        // Single-turn `kind: llm` calls don't run the agent tool-loop, so there is
        // no terminal-turn forcing — provider default.
        tool_choice: None,
    }
}

/// Open the turn via the injected factory, then drain its event stream.
async fn build_provider_and_stream(
    factory: &dyn ProviderFactory,
    model_str: &str,
    turn: TurnRequest,
) -> Result<response::DrainedResponse, ExecutorError> {
    let stream = factory.stream(model_str, turn).await?;
    response::drain_stream(stream).await
}

/// Extract the typed [`LlmErrorCode`] from an [`ExecutorError`] for the
/// audit-emit path. Non-`Llm(_)` variants are mapped to
/// [`LlmErrorCode::ProviderError`] — the audit log is operator-facing
/// and an opaque transport/permanent failure is best logged under the
/// closest matching FMECA bucket so the dashboards aggregate cleanly.
fn error_code_of(err: &ExecutorError) -> LlmErrorCode {
    match err {
        ExecutorError::Llm(code, _) => *code,
        ExecutorError::LlmWithUpdates { code, .. } => *code,
        _ => LlmErrorCode::ProviderError,
    }
}

/// Per-turn accumulators populated as `try_execute` progresses. The
/// outer [`LlmExecutor::execute`] reads these AFTER `try_execute`
/// returns (whether Ok or Err) and builds the `llm.invocation` audit
/// event from them — closing the SPEC §33 audit fixup (F1 STUB-001)
/// gap where early F3 / config-parse rejections leaked out without
/// any audit trail.
///
/// Defaults represent "we never got that far": `model_str` is the
/// `"<unconfigured>"` sentinel so an operator reading the audit log
/// can immediately distinguish a config-time failure from a runtime
/// one; `latency_ms == 0` means no provider call was issued; an empty
/// `drained` carries `usage_present: false` into the audit payload.
struct ExecutionTrace {
    model_str: String,
    capture_reasoning: bool,
    latency_ms: u64,
    cost_usd: Option<f64>,
    drained: DrainedResponse,
    tool_call_name: Option<String>,
}

impl ExecutionTrace {
    fn new() -> Self {
        Self {
            // Sentinel — replaced when config + model resolution land.
            model_str: "<unconfigured>".to_string(),
            // Audit defaults to capture (matches the D7 locked decision);
            // if config parses successfully, the value is replaced with
            // the workflow author's choice.
            capture_reasoning: true,
            latency_ms: 0,
            cost_usd: None,
            drained: DrainedResponse::default(),
            tool_call_name: None,
        }
    }
}

#[async_trait]
impl Executor for LlmExecutor {
    async fn execute(&self, request: ExecuteRequest) -> Result<ExecuteResult, ExecutorError> {
        // Capture the audit identity fields upfront — they're known
        // even on the earliest rejection paths.
        let workflow_id = request.workflow.id.clone();
        let state = request.workflow.state.clone();
        let correlation_id = request.correlation_id.clone();

        let mut trace = ExecutionTrace::new();
        let outcome = self.try_execute(request, &state, &mut trace).await;

        // SPEC §33 audit fixup (F1 STUB-001): the `llm.invocation`
        // event MUST fire on EVERY return path — happy path, validate
        // failure, AND the early F3 / config-parse rejections that the
        // pre-fixup code returned before this point. Build the
        // context from the accumulated trace + the outcome's error code.
        let error_code = match &outcome {
            Ok(_) => None,
            Err(err) => Some(error_code_of(err)),
        };
        let invocation_ctx = InvocationContext {
            workflow_id: &workflow_id,
            state: &state,
            model: &trace.model_str,
            correlation_id: correlation_id.as_deref(),
            latency_ms: trace.latency_ms,
            cost_usd: trace.cost_usd,
            tool_call_emitted: trace.tool_call_name.as_deref(),
            error_code,
            capture_reasoning: trace.capture_reasoning,
        };
        let audit_result = self
            .emit_invocation_audit(invocation_ctx, &trace.drained)
            .await;

        // Reconcile audit failure with the primary outcome. The
        // primary error is the one operators care about; if audit
        // ALSO fails on an already-failing turn, log loudly and
        // surface the primary failure so the reliability layer can
        // classify it correctly.
        match (outcome, audit_result) {
            (Ok(result), Ok(())) => Ok(result),
            (Err(primary), Ok(())) => Err(primary),
            (Ok(_), Err(audit_err)) => {
                // The transition succeeded but we couldn't record it —
                // per SPEC §7.3 (record-first), refuse to surface a
                // success that the audit log doesn't reflect.
                //
                // CMP-009: classify the audit-sink failure as
                // `Connection` (retryable) rather than `Other`
                // (Permanent). An audit-sink blip on an otherwise
                // SUCCESSFUL — and already billed — turn must NOT
                // permanently discard the turn: the reliability layer
                // should retry so the turn re-runs and re-records.
                // We deliberately drop the success result here; the
                // retry produces a fresh `ExecuteResult` whose audit
                // event lands. (Re-running re-bills, but a turn the
                // audit log never reflects is worse than a double
                // charge an operator can reconcile from the log.)
                Err(ExecutorError::Connection(format!(
                    "LLM executor: turn succeeded but audit emission failed; \
                     record-first contract (SPEC §7.3) requires retrying so the \
                     invocation is recorded: {audit_err}"
                )))
            }
            (Err(primary), Err(audit_err)) => {
                tracing::error!(
                    target: "praxec_llm_executor",
                    audit_err = %audit_err,
                    primary_err = %primary,
                    "audit emission failed during failure path; primary error preserved"
                );
                Err(primary)
            }
        }
    }
}

impl LlmExecutor {
    /// Drive one LLM turn, populating `trace` as side state so the
    /// outer [`Executor::execute`] can emit a faithful audit event on
    /// any return path. Each step mutates `trace` BEFORE returning Err
    /// so the audit event reflects how far the executor got.
    async fn try_execute(
        &self,
        request: ExecuteRequest,
        state: &str,
        trace: &mut ExecutionTrace,
    ) -> Result<ExecuteResult, ExecutorError> {
        // SPEC §33 FMECA F3 closed-by-design check, BEFORE
        // deserialization. Delegated to the shared structural helper
        // so the runtime path and the load-time
        // `config_doctor::doctor_check` path stay in lockstep.
        if config_doctor::has_forbidden_tools_field(&request.executor_config) {
            return Err(ExecutorError::Llm(
                LlmErrorCode::ExecutorForbiddenTools,
                "LLM executor: `tools:` field is closed by design \
                 (SPEC §33 FMECA F3); the per-turn tool list IS the workflow's \
                 available transitions"
                    .into(),
            ));
        }

        // Parse the executor config from the request. `deny_unknown_fields`
        // catches typos and other malformed configs at the boundary.
        //
        // The runtime hands us the whole `executor:` block, which in
        // workflow YAML carries the `kind: llm` discriminator at the top
        // level alongside the real config fields. Strip `kind` here so
        // `deny_unknown_fields` continues to fire on genuine typos
        // (FMECA F3 lookalikes, rogue fields) without rejecting the
        // discriminator the dispatcher already routed by.
        let mut config_value = request.executor_config.clone();
        if let Some(obj) = config_value.as_object_mut() {
            obj.remove("kind");
        }
        let config: LlmExecutorConfig = serde_json::from_value(config_value).map_err(|err| {
            ExecutorError::Permanent(format!("LLM executor: config parse failed: {err}"))
        })?;
        // From here on the audit reflects the workflow author's choice.
        trace.capture_reasoning = config.capture_reasoning;

        // D6 hook — cumulative-cap check.
        self.apply_cumulative_caps(&request, &config).await?;

        // Resolve the model string. `model:` is used verbatim (the explicit pin,
        // which wins). `affinity:` is the operator's curated binding, via the
        // injected `AffinityResolver`. `needs:` is score-based: the model
        // suggestor ranks the core catalog by `affinity_fit` over the needed
        // affinities, filtered to tool-capable + reachable models.
        let model_str = match (&config.model, &config.affinity, config.needs.is_empty()) {
            (Some(m), _, _) => m.clone(),
            (None, Some(a), _) => self.affinity_resolver.resolve(&a.to_string()).await?,
            (None, None, false) => {
                use praxec_core::model_catalog;
                let catalog = model_catalog::model_catalog();
                model_catalog::suggest_for_needs(
                    &catalog.models,
                    &config.needs,
                    model_catalog::vendor_available,
                )
                .map(|m| m.model_string())
                .ok_or_else(|| {
                    ExecutorError::Permanent(format!(
                        "LLM executor: no reachable tool-calling model fits needs {:?}; \
                         configure a provider key or set `model:`/`affinity:`",
                        config.needs
                    ))
                })?
            }
            (None, None, true) => {
                return Err(ExecutorError::Permanent(
                    "LLM executor: none of `model:`, `affinity:`, or `needs:` is set; \
                     one is required"
                        .into(),
                ));
            }
        };
        trace.model_str = model_str.clone();

        // D6 — read the synthetic-slot snapshot once, before the tool
        // list fetch. We need it twice: the cumulative caps already
        // ran on its values (via `apply_cumulative_caps` above), and
        // the post-turn delta is built against it after a successful
        // turn. Reading once keeps both call sites against the same
        // pre-turn baseline.
        let pre_turn_snapshot = caps::read_snapshot(&request.workflow, &request.workflow.state);

        // Get the per-turn tool list from the runtime.
        let principal = synthetic_agent_principal();
        let links = self
            .transition_resolver
            .available_transitions(&request.workflow, &principal)
            .await
            .map_err(ExecutorError::Other)?;

        // Build provider-shaped tools. FMECA F7 (duplicate rel) fires
        // here, BEFORE the provider call.
        let tools = prompt::links_to_tool_definitions(&links, state)?;

        // CMP-012: fail fast when the state offers NO transitions to the
        // model. A provider call with an empty tool list can only return
        // a final answer with no tool call (guaranteed F1 failure) — a
        // wasteful, billable round-trip. Reject BEFORE building context
        // or calling the provider. Mirrors the empty-prompt guard below.
        if tools.is_empty() {
            return Err(ExecutorError::Llm(
                LlmErrorCode::NoAvailableTools,
                format!(
                    "LLM_NO_AVAILABLE_TOOLS: state '{state}' offers no transitions \
                     to the model (the guard-filtered transition list is empty); \
                     refusing to issue a provider call that could only yield a \
                     no-tool-call turn"
                ),
            ));
        }

        // Owned name list so the tool list itself can move into Context.
        let valid_names_owned: Vec<String> = tools.iter().map(|t| t.name.clone()).collect();

        // Render prompt + assemble Context.
        let rendered_prompt = prompt::render_template(&config.prompt_template, &request);
        // SPEC §33 audit fixup (F1 STUB-005): a template that renders
        // to empty content is operationally worse than a hard failure
        // because the provider would respond to nothing and the
        // workflow would look like it advanced. Defense-in-depth
        // against (a) operators who slipped a workflow past
        // config_doctor::doctor_check with an empty literal AND
        // (b) templates whose only references resolved to missing
        // scope variables. Audit fires via the trace plumbing above.
        if rendered_prompt.trim().is_empty() {
            return Err(ExecutorError::Llm(
                LlmErrorCode::EmptyPrompt,
                "LLM executor: rendered `prompt_template` produced no content; \
                 check that scope references (blackboard / context / input) \
                 resolve in the current workflow state"
                    .into(),
            ));
        }
        // Resolve the in-scope skills' bodies into the system message
        // (Agent + Skill + Prompt). A declared-but-unstamped subject or an
        // empty body fails loud here; the `?` routes it through the same
        // audit-on-Err plumbing as every other pre-provider failure.
        let system_msg = skills::collect_system_message(
            &request.workflow.definition,
            state,
            request.transition.as_deref(),
        )?;
        let turn = build_turn(
            system_msg,
            rendered_prompt,
            tools,
            &model_str,
            config.reasoning_effort.as_deref(),
        );

        // Stream + drain.
        let start = std::time::Instant::now();
        let drained_result =
            build_provider_and_stream(self.provider_factory.as_ref(), &model_str, turn).await;
        trace.latency_ms = start.elapsed().as_millis() as u64;

        // Pull the drained response (or a default + the stream-level
        // error code) out of the result. We need a `&DrainedResponse`
        // either way so the audit event captures whatever the drainer
        // collected before the failure.
        let stream_error: Option<ExecutorError> = match drained_result {
            Ok(d) => {
                trace.drained = d;
                None
            }
            Err(err) => Some(err),
        };

        // D8 — compute USD cost via the catalog when usage is present.
        //
        // SPEC §33 audit fixup (F2 STUB-003): when ANY budget cap is set
        // (`max_cost_usd` or `max_tokens`), a catalog miss at runtime
        // MUST fail the turn rather than silently downgrade to `None`.
        // The cost_doctor load-time check is the first line of defense;
        // this runtime gate is the second, covering catalog drift,
        // affinity-resolved models that bypassed doctor (D9 follow-up),
        // and any other path that could land here with an unknown model
        // while budget tracking is active.
        //
        // Without budget caps, the soft-warn-and-None pattern stays —
        // operators graph the null rate to spot catalog drift.
        let has_budget_cap = config.max_cost_usd.is_some() || config.max_tokens.is_some();
        if let Some(usage) = trace.drained.usage.as_ref() {
            let input = usage.input_tokens;
            let output = usage.output_tokens;
            match cost::compute_cost_usd(&model_str, input, output) {
                Ok(c) => trace.cost_usd = Some(c),
                Err(err) if has_budget_cap => {
                    return Err(ExecutorError::Llm(
                        LlmErrorCode::UsageMissing,
                        format!(
                            "cost catalog lookup failed for model '{model_str}' while \
                             budget tracking is active (max_cost_usd / max_tokens set); \
                             FMECA F8 forbids silently bypassing the cap with a null cost: \
                             {err}"
                        ),
                    ));
                }
                Err(err) => {
                    tracing::warn!(
                        target: "praxec_llm_executor::cost",
                        model = %model_str,
                        error = %err,
                        "cost catalog lookup failed at runtime; cost_usd will be null \
                         (no budget cap set so this is a soft warning, not a failure)"
                    );
                }
            }
        }

        if let Some(err) = stream_error {
            return Err(err);
        }

        // Run the validation chain. Each branch produces a typed
        // `LlmErrorCode`; the reliability layer keys off the class to
        // decide whether to retry.
        let valid_names: Vec<&str> = valid_names_owned.iter().map(String::as_str).collect();
        if let Err(validate_err) = response::validate(&trace.drained, &config, &valid_names, state)
        {
            // SPEC §33 audit fixup (F3 STUB-004) — FMECA F1 counter
            // wiring. For `LLM_NO_TOOL_CALL` specifically, attach the
            // post-turn slot updates with `no_tool_call_this_turn = true`
            // so the runtime merges them into next.context BEFORE
            // recording the rejection. Without this, the consecutive
            // counter would never tick up and apply_caps's cap could
            // never fire (silent dead protection).
            //
            // Other validate failures (multi-tool, unknown-tool,
            // malformed-args, usage-missing) don't need a counter
            // increment: F1 specifically gates against models that
            // produce final answers in lieu of tool selection, not
            // against models that pick wrong / malformed tools.
            if matches!(
                &validate_err,
                ExecutorError::Llm(LlmErrorCode::NoToolCall, _)
            ) {
                // CMP-008: on the no-tool-call FAILURE path the usage
                // event is NOT validated upstream (validate() rejects
                // NoToolCall at step 4, before the step-7 usage check),
                // so a missing-usage turn under an active cap could
                // otherwise fold a silent 0 into the cumulative counter.
                // Thread the cap-active flag so the helper fails loudly
                // instead. If it errors, surface THAT error (the missing
                // spend is more important than the F1 counter tick).
                let cap_active = config.max_tokens.is_some() || config.max_cost_usd.is_some();
                let slot_updates = caps::build_post_turn_slot_updates(
                    &pre_turn_snapshot,
                    &trace.drained,
                    trace.cost_usd,
                    true, // no_tool_call_this_turn — increments F1 counter
                    cap_active,
                    state,
                    chrono::Utc::now(),
                )?;
                let detail = match &validate_err {
                    ExecutorError::Llm(_, msg) => msg.clone(),
                    _ => "no tool call".to_string(),
                };
                return Err(ExecutorError::LlmWithUpdates {
                    code: LlmErrorCode::NoToolCall,
                    detail,
                    output: slot_updates,
                });
            }
            return Err(validate_err);
        }

        // The validated single tool call.
        let tool_call = trace
            .drained
            .tool_calls
            .first()
            .ok_or_else(|| {
                ExecutorError::Permanent(
                    "internal invariant: validate() passed but no tool call captured".into(),
                )
            })?
            .clone();
        trace.tool_call_name = Some(tool_call.name.clone());

        let args: Value = if tool_call.arguments.trim().is_empty() {
            json!({})
        } else {
            serde_json::from_str(&tool_call.arguments).map_err(|err| {
                ExecutorError::Llm(
                    LlmErrorCode::MalformedArguments,
                    format!("Failed to parse tool arguments as JSON: {err}"),
                )
            })?
        };

        // D6 — post-turn synthetic-slot updates. Successful turn:
        // `no_tool_call_this_turn = false`, so the consecutive-failure
        // counter resets. D8 lands the catalog-derived cost into the
        // cumulative slot; `None` keeps the prior value when the
        // catalog has no entry for `model_str`.
        // CMP-008: pass the cap-active flag here too. On the success
        // path `validate()` already guaranteed usage is present when a
        // budget cap is active (step 7), so this `?` can only fire on a
        // genuine invariant break — surface it rather than undercount.
        let slot_updates = caps::build_post_turn_slot_updates(
            &pre_turn_snapshot,
            &trace.drained,
            trace.cost_usd,
            false,
            has_budget_cap,
            state,
            chrono::Utc::now(),
        )?;

        let summary = if trace.drained.text.trim().is_empty() {
            None
        } else {
            Some(std::mem::take(&mut trace.drained.text))
        };

        Ok(ExecuteResult {
            output: slot_updates,
            evidence: vec![],
            child_workflow_id: None,
            next_transition: Some(NextTransition {
                transition: tool_call.name,
                arguments: args,
                summary,
            }),
            suspend: None,
            telemetry: None,
        })
    }
}

#[cfg(test)]
mod build_turn_tests {
    use super::*;

    #[test]
    fn skill_body_becomes_the_system_preamble() {
        let turn = build_turn(
            Some("persona".into()),
            "task".into(),
            vec![],
            "openai:gpt-5",
            None,
        );
        assert_eq!(turn.system.as_deref(), Some("persona"));
        assert_eq!(turn.prompt, rig::completion::Message::user("task"));
        assert!(turn.reasoning.is_none());
    }

    #[test]
    fn reasoning_effort_maps_to_provider_additional_params() {
        // OpenRouter `high` → reasoning.effort (the shared tuning builder).
        let turn = build_turn(None, "task".into(), vec![], "openrouter:x/y", Some("high"));
        assert_eq!(
            turn.reasoning,
            Some(serde_json::json!({ "reasoning": { "effort": "high" } }))
        );
        // `medium`/absent → nothing sent (provider default).
        let none = build_turn(None, "t".into(), vec![], "openrouter:x/y", Some("medium"));
        assert!(none.reasoning.is_none());
    }
}
