//! Verifies the human executor emits `human.approval.requested` to the
//! audit sink it's wired with.

use std::sync::Arc;

use praxec_core::audit::MemoryAuditSink;
use praxec_core::model::{ExecuteRequest, WorkflowInstance};
use praxec_core::ports::Executor;
use praxec_executors::HumanExecutor;
use serde_json::json;

#[tokio::test]
async fn human_executor_emits_approval_requested() {
    let audit = Arc::new(MemoryAuditSink::new());
    let exec = HumanExecutor::with_audit(audit.clone());

    let request = ExecuteRequest {
        workflow: WorkflowInstance {
            id: "wf_test".into(),
            definition_id: "demo".into(),
            definition_version: "1.0.0".into(),
            definition: json!({"initialState": "awaiting_approval", "states": {}}),
            state: "awaiting_approval".into(),
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
        transition: Some("request_approval".into()),
        arguments: json!({}),
        executor_config: json!({ "kind": "human", "queue": "engineering-approvals" }),
        idempotency_key: None,
        correlation_id: None,
    };

    exec.execute(request)
        .await
        .expect("human executor succeeds");

    let events = audit.snapshot();
    let evt = events
        .iter()
        .find(|e| e.event_type == "human.approval.requested")
        .expect("must emit human.approval.requested");

    assert_eq!(evt.payload["queue"], "engineering-approvals");
    assert_eq!(evt.payload["transition"], "request_approval");
    assert_eq!(evt.workflow_id.as_deref(), Some("wf_test"));
    assert!(evt.payload["requestId"].is_string());
}
