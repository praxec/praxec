//! Task 2 — the executor consults its injected AffinityResolver when a config
//! sets `affinity:` instead of `model:`.
use std::sync::Arc;

use async_trait::async_trait;
use praxec_core::audit::NullAuditSink;
use praxec_core::error::ExecutorError;
use praxec_core::model::{ExecuteRequest, Principal, WorkflowInstance};
use praxec_core::ports::{Executor, TransitionResolver};
use praxec_llm_executor::LlmExecutor;
use praxec_llm_executor::affinity::AffinityResolver;
use serde_json::{Value, json};

// A resolver whose error is a unique marker — proves the executor CALLED it
// (instead of the old hardcoded "not wired in v0.6 D5" reject).
struct ReachedMarker;
#[async_trait]
impl AffinityResolver for ReachedMarker {
    async fn resolve(&self, _a: &str) -> Result<String, ExecutorError> {
        Err(ExecutorError::Permanent("AFFINITY_RESOLVER_REACHED".into()))
    }
}

struct EmptyTransitions;
#[async_trait]
impl TransitionResolver for EmptyTransitions {
    async fn available_transitions(
        &self,
        _i: &WorkflowInstance,
        _p: &Principal,
    ) -> anyhow::Result<Vec<Value>> {
        Ok(vec![])
    }
}

fn instance() -> WorkflowInstance {
    WorkflowInstance {
        id: "wf".into(),
        definition_id: "d".into(),
        definition_version: "1.0.0".into(),
        definition: json!({}),
        state: "s".into(),
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

#[tokio::test]
async fn executor_consults_injected_affinity_resolver() {
    let exec = LlmExecutor::new(Arc::new(NullAuditSink), Arc::new(EmptyTransitions))
        .with_affinity_resolver(Arc::new(ReachedMarker));
    let request = ExecuteRequest {
        workflow: instance(),
        transition: None,
        arguments: json!({}),
        executor_config: json!({ "affinity": "coding-frontier", "prompt_template": "hi" }),
        idempotency_key: None,
        correlation_id: None,
    };
    let err = exec.execute(request).await.unwrap_err();
    assert!(
        format!("{err:?}").contains("AFFINITY_RESOLVER_REACHED"),
        "executor must consult the injected resolver, got: {err:?}"
    );
}
