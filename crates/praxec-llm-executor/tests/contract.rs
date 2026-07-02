//! SPEC §33 D9 — smoke contract test for the in-runtime LLM executor.
//!
//! This file is the ONE place where the executor is exercised against
//! a REAL aether-llm provider. It is `#[ignore]` by default so normal
//! `cargo test` runs (and CI on commits without API access) skip it.
//!
//! Enable with:
//!
//! ```bash
//! PRAXEC_LLM_CONTRACT_TEST=1 \
//!   ANTHROPIC_API_KEY=... \
//!   cargo test -p praxec-llm-executor --test contract -- --ignored
//! ```
//!
//! What it verifies:
//! - A real `anthropic:claude-*` provider can be constructed via
//!   `DefaultProviderFactory`.
//! - The executor drives one turn end-to-end against a synthetic
//!   triage prompt with three valid transitions.
//! - The drained tool call's `name` is one of the declared
//!   transitions.
//! - The `llm.invocation` audit event fires with `tool_call_emitted`
//!   populated.
//!
//! Failure modes deliberately surface as test FAILURES, not skips:
//! once a developer sets `PRAXEC_LLM_CONTRACT_TEST=1` they have
//! accepted the cost of hitting a real API, and a broken integration
//! should fail loudly.

use std::sync::Arc;

use async_trait::async_trait;
use praxec_core::audit::MemoryAuditSink;
use praxec_core::model::{ExecuteRequest, Principal, WorkflowInstance};
use praxec_core::ports::{Executor, TransitionResolver};
use praxec_llm_executor::{DefaultProviderFactory, LlmExecutor, ProviderFactory};
use serde_json::{json, Value};

/// A static three-transition resolver matching the `examples/issue_triager.yaml`
/// triage state. Hand-coded so the contract test is self-contained and
/// doesn't depend on loading the YAML through the full runtime.
struct TriageResolver;

#[async_trait]
impl TransitionResolver for TriageResolver {
    async fn available_transitions(
        &self,
        _instance: &WorkflowInstance,
        _principal: &Principal,
    ) -> anyhow::Result<Vec<Value>> {
        Ok(vec![
            json!({
                "rel": "mark_as_bug",
                "title": "Classify as a bug",
                "inputSchema": {
                    "type": "object",
                    "properties": {
                        "severity": { "type": "string", "enum": ["low", "medium", "high"] }
                    },
                    "required": ["severity"]
                }
            }),
            json!({
                "rel": "mark_as_feature",
                "title": "Classify as a feature request",
                "inputSchema": {
                    "type": "object",
                    "properties": {
                        "priority": { "type": "string", "enum": ["p0", "p1", "p2", "p3"] }
                    },
                    "required": ["priority"]
                }
            }),
            json!({
                "rel": "close_as_noise",
                "title": "Close as noise",
                "inputSchema": {
                    "type": "object",
                    "properties": { "reason": { "type": "string" } },
                    "required": ["reason"]
                }
            }),
        ])
    }
}

fn make_instance() -> WorkflowInstance {
    WorkflowInstance {
        id: "wf_contract".into(),
        definition_id: "issue_triager".into(),
        definition_version: "2026-05-29".into(),
        definition: json!({
            "initialState": "triaging",
            "states": {
                "triaging": {},
                "investigating": { "terminal": true },
                "backlog": { "terminal": true },
                "closed": { "terminal": true }
            }
        }),
        state: "triaging".into(),
        version: 0,
        input: json!({
            "issue_body": "Login button is broken on mobile Safari; users see a 500 error"
        }),
        context: json!({
            "issue_body": "Login button is broken on mobile Safari; users see a 500 error"
        }),
        started_at: chrono::Utc::now(),
        trace_id: None,
        run_id: None,
        cancelled_at: None,
        cancelled_reason: None,
        depth: 0,
        parent: None,
    }
}

const VALID_TOOLS: &[&str] = &["mark_as_bug", "mark_as_feature", "close_as_noise"];

#[tokio::test]
#[ignore = "smoke contract test — requires PRAXEC_LLM_CONTRACT_TEST=1 and a provider API key"]
async fn smoke_contract_anthropic_issue_triager() {
    if std::env::var("PRAXEC_LLM_CONTRACT_TEST").ok().as_deref() != Some("1") {
        eprintln!(
            "skipping: smoke_contract_anthropic_issue_triager requires \
             PRAXEC_LLM_CONTRACT_TEST=1 (and an Anthropic API key)"
        );
        return;
    }
    if std::env::var("ANTHROPIC_API_KEY").is_err() {
        panic!(
            "smoke_contract_anthropic_issue_triager: PRAXEC_LLM_CONTRACT_TEST=1 \
             but ANTHROPIC_API_KEY is unset — refusing to silently pass. \
             Either set the key or clear PRAXEC_LLM_CONTRACT_TEST."
        );
    }

    let audit = Arc::new(MemoryAuditSink::new());
    let resolver: Arc<dyn TransitionResolver> = Arc::new(TriageResolver);
    let factory: Arc<dyn ProviderFactory> = Arc::new(DefaultProviderFactory);
    let executor = LlmExecutor::with_provider_factory(audit.clone(), resolver, factory);

    let request = ExecuteRequest {
        workflow: make_instance(),
        transition: None,
        arguments: json!({}),
        executor_config: json!({
            "kind": "llm",
            "model": "anthropic:claude-haiku-4-5",
            "prompt_template": "Triage this GitHub issue. \
             The body is in `$.blackboard.issue_body`. \
             Pick exactly one transition.\n\nIssue body:\n{{ blackboard.issue_body }}",
            "max_iterations": 1,
            "max_seconds": 60
        }),
        idempotency_key: None,
        correlation_id: Some("cor_contract".into()),
    };

    let result = executor
        .execute(request)
        .await
        .expect("real provider invocation must succeed");
    let next = result
        .next_transition
        .expect("contract test: real provider must select a transition");
    assert!(
        VALID_TOOLS.contains(&next.transition.as_str()),
        "tool call '{}' is not one of the three valid transitions: {VALID_TOOLS:?}",
        next.transition
    );

    let snapshot = audit.snapshot();
    let invocation = snapshot
        .iter()
        .find(|e| e.event_type == "llm.invocation")
        .expect("contract test: llm.invocation audit event must fire");
    let emitted = invocation
        .payload
        .get("tool_call_emitted")
        .and_then(Value::as_str)
        .expect("audit event must carry tool_call_emitted on success");
    assert_eq!(
        emitted, next.transition,
        "audit tool_call_emitted must match the chosen NextTransition"
    );
}
