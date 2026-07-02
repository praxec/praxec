//! SPEC §33 D4 skeleton test — verifies that the `LlmExecutor` shell
//! constructs, registers as an `Executor`, and surfaces the documented
//! parse-boundary behaviors. Post-D5 the inner flow runs against
//! aether-llm, so the "happy parse" path is exercised via a malformed
//! model string (no `provider:model-id` colon) — that path is still
//! reachable without any network call.
//!
//! 1. A config that parses but carries a malformed model string fails
//!    with `ExecutorError::Permanent` mentioning the expected format.
//! 2. FMECA F3: a config with `tools:` is rejected before any provider
//!    call — surfaced as `LlmErrorCode::ExecutorForbiddenTools`.
//! 3. Other malformed configs surface as `ExecutorError::Permanent`.

use std::sync::Arc;

use async_trait::async_trait;
use praxec_core::audit::{MemoryAuditSink, NullAuditSink};
use praxec_core::error::{ExecutorError, LlmErrorCode};
use praxec_core::model::{ExecuteRequest, Principal, WorkflowInstance};
use praxec_core::ports::{Executor, TransitionResolver};
use praxec_llm_executor::LlmExecutor;
use serde_json::{json, Value};

/// Minimal `TransitionResolver` mock for D4 — the shell never invokes
/// it (execute() short-circuits on the config-parse boundary), so an
/// empty-Vec stub is sufficient.
struct EmptyResolver;

#[async_trait]
impl TransitionResolver for EmptyResolver {
    async fn available_transitions(
        &self,
        _instance: &WorkflowInstance,
        _principal: &Principal,
    ) -> anyhow::Result<Vec<Value>> {
        Ok(vec![])
    }
}

/// Resolver that offers exactly one transition, so the executor gets past
/// the CMP-012 empty-tool-list fail-fast and reaches the model/prompt/
/// provider validation that some tests target. (The empty-tool case has
/// its own dedicated test.)
struct OneToolResolver;

#[async_trait]
impl TransitionResolver for OneToolResolver {
    async fn available_transitions(
        &self,
        _instance: &WorkflowInstance,
        _principal: &Principal,
    ) -> anyhow::Result<Vec<Value>> {
        Ok(vec![json!({ "rel": "advance" })])
    }
}

fn make_executor_with_one_tool() -> LlmExecutor {
    let audit = Arc::new(NullAuditSink);
    let resolver: Arc<dyn TransitionResolver> = Arc::new(OneToolResolver);
    LlmExecutor::new(audit, resolver)
}

fn make_instance() -> WorkflowInstance {
    WorkflowInstance {
        id: "wf_skeleton".into(),
        definition_id: "demo".into(),
        definition_version: "1.0.0".into(),
        definition: json!({"initialState": "thinking", "states": {}}),
        state: "thinking".into(),
        version: 0,
        input: json!({}),
        context: json!({}),
        started_at: chrono::Utc::now(),
        trace_id: None,
        run_id: None,
        cancelled_at: None,
        cancelled_reason: None,
        depth: 0,
        parent: None,
    }
}

fn make_executor() -> LlmExecutor {
    let audit = Arc::new(NullAuditSink);
    let resolver: Arc<dyn TransitionResolver> = Arc::new(EmptyResolver);
    LlmExecutor::new(audit, resolver)
}

fn request_with_config(executor_config: Value) -> ExecuteRequest {
    ExecuteRequest {
        workflow: make_instance(),
        transition: None,
        arguments: json!({}),
        executor_config,
        idempotency_key: None,
        correlation_id: None,
    }
}

#[tokio::test]
async fn shell_rejects_malformed_model_string() {
    // D5 expects `provider:model-id` (colon). A workflow author who
    // wrote `provider/model-id` (slash) by mistake should fail loudly
    // at the provider-factory boundary, before any network/env access.
    // Use a one-tool resolver so the CMP-012 empty-tool guard doesn't
    // intercept before the provider build.
    let exec = make_executor_with_one_tool();
    let request = request_with_config(json!({
        "model": "openai/gpt-4o",
        "prompt_template": "say hi"
    }));
    let err = exec
        .execute(request)
        .await
        .expect_err("malformed model string must surface a clean error");
    match err {
        ExecutorError::Permanent(msg) => {
            assert!(
                msg.contains("provider:model-id"),
                "error must mention expected format: got {msg}"
            );
        }
        other => panic!("expected Permanent, got {other:?}"),
    }
}

#[tokio::test]
async fn shell_rejects_tools_field_with_forbidden_tools_code() {
    // SPEC §33 FMECA F3 — closed by design.
    let exec = make_executor();
    let request = request_with_config(json!({
        "model": "openai/gpt-4o",
        "prompt_template": "ignore",
        "tools": [{ "name": "evil" }]
    }));
    let err = exec
        .execute(request)
        .await
        .expect_err("tools: field must be rejected");
    match err {
        ExecutorError::Llm(code, _) => {
            assert_eq!(code, LlmErrorCode::ExecutorForbiddenTools);
        }
        other => panic!("expected Llm(ExecutorForbiddenTools, _), got {other:?}"),
    }
}

#[tokio::test]
async fn shell_rejects_other_unknown_fields_as_permanent() {
    let exec = make_executor();
    let request = request_with_config(json!({
        "model": "openai/gpt-4o",
        "prompt_template": "ignore",
        "rogue_field": true
    }));
    let err = exec
        .execute(request)
        .await
        .expect_err("unknown fields must be rejected");
    match err {
        ExecutorError::Permanent(msg) => {
            assert!(
                msg.contains("rogue_field"),
                "expected mention of rogue_field, got {msg}"
            );
        }
        other => panic!("expected Permanent, got {other:?}"),
    }
}

/// Regression for the FMECA F3 substring-match brittleness flagged in
/// review: `tools_dir`, `tools_path`, `model_tools` etc. share the
/// substring "tools" but are NOT the closed-by-design `tools:` field.
/// The structural check on the raw JSON must classify these as generic
/// `Permanent` parse errors, not as `ExecutorForbiddenTools` — otherwise
/// the audit emitter (D7) would mark benign typos as security events.
#[tokio::test]
async fn shell_does_not_misclassify_tools_lookalikes_as_f3() {
    for field in ["tools_dir", "tools_path", "model_tools", "use_tools"] {
        let exec = make_executor();
        let mut cfg = serde_json::Map::new();
        cfg.insert("model".into(), json!("openai/gpt-4o"));
        cfg.insert("prompt_template".into(), json!("hi"));
        cfg.insert(field.into(), json!("anything"));
        let request = request_with_config(Value::Object(cfg));
        let err = exec
            .execute(request)
            .await
            .expect_err(&format!("config with {field} must error"));
        match err {
            ExecutorError::Permanent(_) => { /* correct: generic parse failure */ }
            ExecutorError::Llm(LlmErrorCode::ExecutorForbiddenTools, msg) => {
                panic!("field `{field}` was misclassified as F3 ForbiddenTools: {msg}");
            }
            other => panic!("expected Permanent for field `{field}`, got {other:?}"),
        }
    }
}

/// CMP-012: a state whose guard-filtered transition list is empty must
/// fail fast with `LLM_NO_AVAILABLE_TOOLS` BEFORE any provider call.
/// `EmptyResolver` returns no transitions, and a well-formed
/// `provider:model` string carries execution past the model-parse gate,
/// so this reaches the new empty-tools guard.
#[tokio::test]
async fn shell_rejects_empty_tool_list_with_no_available_tools() {
    let exec = make_executor();
    let request = request_with_config(json!({
        "model": "openai:gpt-4o",
        "prompt_template": "say hi"
    }));
    let err = exec
        .execute(request)
        .await
        .expect_err("a state with no transitions must fail fast");
    match err {
        ExecutorError::Llm(code, msg) => {
            assert_eq!(code, LlmErrorCode::NoAvailableTools);
            assert!(
                msg.contains("LLM_NO_AVAILABLE_TOOLS"),
                "message must carry the wire code: {msg}"
            );
            assert!(
                msg.contains("thinking"),
                "message must name the state: {msg}"
            );
        }
        other => panic!("expected Llm(NoAvailableTools, _), got {other:?}"),
    }
}

// ── SPEC §33 audit fixup (F1) ─────────────────────────────────────────────────

fn make_executor_with_capturing_audit() -> (LlmExecutor, Arc<MemoryAuditSink>) {
    let audit = Arc::new(MemoryAuditSink::new());
    let audit_dyn: Arc<dyn praxec_core::audit::AuditSink> = audit.clone();
    let resolver: Arc<dyn TransitionResolver> = Arc::new(EmptyResolver);
    (LlmExecutor::new(audit_dyn, resolver), audit)
}

/// Capturing-audit executor with a one-transition resolver, for tests that
/// must reach prompt/provider validation past the CMP-012 empty-tool guard.
fn make_executor_with_capturing_audit_one_tool() -> (LlmExecutor, Arc<MemoryAuditSink>) {
    let audit = Arc::new(MemoryAuditSink::new());
    let audit_dyn: Arc<dyn praxec_core::audit::AuditSink> = audit.clone();
    let resolver: Arc<dyn TransitionResolver> = Arc::new(OneToolResolver);
    (LlmExecutor::new(audit_dyn, resolver), audit)
}

/// SPEC §33 audit fixup (F1 STUB-001): the F3 `tools:` rejection used to
/// return `Err` BEFORE `emit_invocation_audit` ran, leaving operators with
/// no forensic trail for the very class of failure they care about most.
/// Audit must now fire on this path with the typed wire code.
#[tokio::test]
async fn audit_fires_on_f3_tools_rejection() {
    let (exec, audit) = make_executor_with_capturing_audit();
    let request = request_with_config(json!({
        "model": "openai/gpt-4o",
        "prompt_template": "ignore",
        "tools": [{ "name": "evil" }]
    }));
    let err = exec.execute(request).await.expect_err("F3 must reject");
    assert!(matches!(
        err,
        ExecutorError::Llm(LlmErrorCode::ExecutorForbiddenTools, _)
    ));

    let snapshot = audit.snapshot();
    let invocation = snapshot
        .iter()
        .find(|e| e.event_type == "llm.invocation")
        .expect("F3 rejection must emit llm.invocation (audit-on-every-path)");
    assert_eq!(
        invocation.payload.get("error_code"),
        Some(&Value::from("LLM_EXECUTOR_FORBIDDEN_TOOLS")),
        "F3 audit event must carry the typed wire code"
    );
    // model field is the unconfigured sentinel — config never parsed.
    assert_eq!(
        invocation.payload.get("model"),
        Some(&Value::from("<unconfigured>")),
        "model field marks the failure as pre-config-parse"
    );
}

/// SPEC §33 audit fixup (F1 STUB-001): generic config-parse failures
/// (deny_unknown_fields, type mismatches, etc.) used to return
/// `Permanent` BEFORE audit emission. Operators get no signal that a
/// workflow was rejected at the boundary. Audit must fire.
#[tokio::test]
async fn audit_fires_on_config_parse_rejection() {
    let (exec, audit) = make_executor_with_capturing_audit();
    let request = request_with_config(json!({
        "model": "openai/gpt-4o",
        "prompt_template": "ignore",
        "rogue_field": true
    }));
    exec.execute(request)
        .await
        .expect_err("rogue field must reject");

    let snapshot = audit.snapshot();
    let invocation = snapshot
        .iter()
        .find(|e| e.event_type == "llm.invocation")
        .expect("config-parse rejection must emit llm.invocation");
    // Generic Permanent errors flow into the audit as ProviderError per
    // the `error_code_of` fallback — the important assertion is that
    // the event fires at all.
    assert!(invocation.payload.get("error_code").is_some());
    assert_eq!(
        invocation.payload.get("usage_present"),
        Some(&Value::from(false))
    );
}

/// SPEC §33 audit fixup (F1 STUB-005): an empty literal `prompt_template`
/// (or a template that renders to whitespace) must fail fast with the
/// typed `LLM_EMPTY_PROMPT` code so a workflow doesn't silently issue
/// a no-content LLM call.
#[tokio::test]
async fn empty_prompt_template_returns_typed_empty_prompt_error() {
    let (exec, audit) = make_executor_with_capturing_audit_one_tool();
    let request = request_with_config(json!({
        "model": "anthropic:claude-sonnet-4-6",
        "prompt_template": ""
    }));
    let err = exec
        .execute(request)
        .await
        .expect_err("empty prompt_template must fail fast");
    match err {
        ExecutorError::Llm(LlmErrorCode::EmptyPrompt, _) => {}
        other => panic!("expected Llm(EmptyPrompt, _), got {other:?}"),
    }

    let snapshot = audit.snapshot();
    let invocation = snapshot
        .iter()
        .find(|e| e.event_type == "llm.invocation")
        .expect("empty-prompt rejection must emit llm.invocation");
    assert_eq!(
        invocation.payload.get("error_code"),
        Some(&Value::from("LLM_EMPTY_PROMPT"))
    );
    // Empty-prompt fires AFTER model resolution, so the trace carries the
    // real model string — useful for operator forensics.
    assert_eq!(
        invocation.payload.get("model"),
        Some(&Value::from("anthropic:claude-sonnet-4-6"))
    );
}
