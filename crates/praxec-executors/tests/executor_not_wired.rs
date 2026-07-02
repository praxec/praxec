//! `parallel` and `pipeline` need a back-reference to the registry (set via
//! `set_registry` after the registry is built) so their branches/steps can
//! invoke other executors. If an embedder constructs one and forgets to wire
//! it, dispatch must fail loud with a `*_NOT_WIRED` Permanent error rather
//! than panic or hang. The happy path (registry wired) is covered by
//! `parallel_executor.rs` / `pipeline_executor.rs`.

use std::sync::Arc;

use praxec_core::audit::{AuditSink, MemoryAuditSink};
use praxec_core::error::ExecutorError;
use praxec_core::model::{ExecuteRequest, WorkflowInstance};
use praxec_core::ports::Executor;
use praxec_executors::{ParallelExecutor, PipelineExecutor};
use serde_json::{json, Value};

fn req(executor_config: Value) -> ExecuteRequest {
    ExecuteRequest {
        workflow: WorkflowInstance {
            id: "wf_nw".into(),
            definition_id: "demo".into(),
            definition_version: "1.0.0".into(),
            definition: json!({ "initialState": "s", "states": { "s": {} } }),
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
        },
        transition: Some("go".into()),
        arguments: json!({}),
        executor_config,
        idempotency_key: None,
        correlation_id: None,
    }
}

#[tokio::test]
async fn parallel_without_registry_fails_not_wired() {
    let audit: Arc<dyn AuditSink> = Arc::new(MemoryAuditSink::new());
    let exec = ParallelExecutor::new(audit); // set_registry intentionally NOT called
    let err = exec
        .execute(req(json!({
            "kind": "parallel",
            "branches": [{ "kind": "noop" }],
            "join": "all",
        })))
        .await
        .expect_err("dispatch without a wired registry must fail");
    match err {
        ExecutorError::Permanent(msg) => {
            assert!(msg.contains("PARALLEL_EXECUTOR_NOT_WIRED"), "got: {msg}")
        }
        other => panic!("expected Permanent, got {other:?}"),
    }
}

#[tokio::test]
async fn pipeline_without_registry_fails_not_wired() {
    let audit: Arc<dyn AuditSink> = Arc::new(MemoryAuditSink::new());
    let exec = PipelineExecutor::new(audit); // set_registry intentionally NOT called
    let err = exec
        .execute(req(json!({
            "kind": "pipeline",
            "steps": [{ "kind": "noop" }],
        })))
        .await
        .expect_err("dispatch without a wired registry must fail");
    match err {
        ExecutorError::Permanent(msg) => {
            assert!(msg.contains("PIPELINE_EXECUTOR_NOT_WIRED"), "got: {msg}")
        }
        other => panic!("expected Permanent, got {other:?}"),
    }
}
