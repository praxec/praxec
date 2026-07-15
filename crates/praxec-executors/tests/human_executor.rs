//! Behavioral coverage for `HumanExecutor`: queue resolution and the
//! audit-sink-failure path. A human approval request must still be queued
//! even if the audit emit fails (the executor logs + swallows) — losing the
//! audit line must never block the human-in-the-loop step.

use std::sync::Arc;

use async_trait::async_trait;
use praxec_core::audit::{AuditEvent, AuditSink};
use praxec_core::model::{ExecuteRequest, WorkflowInstance};
use praxec_core::ports::Executor;
use praxec_executors::HumanExecutor;
use serde_json::{Value, json};

/// An audit sink whose `record` always fails — exercises the swallow path.
struct FailingAuditSink;

#[async_trait]
impl AuditSink for FailingAuditSink {
    async fn record(&self, _event: AuditEvent) -> anyhow::Result<()> {
        Err(anyhow::anyhow!("disk full"))
    }
}

fn req(executor_config: Value) -> ExecuteRequest {
    ExecuteRequest {
        workflow: WorkflowInstance {
            id: "wf_human".into(),
            definition_id: "demo".into(),
            definition_version: "1.0.0".into(),
            definition: json!({ "initialState": "s", "states": { "s": {} } }),
            state: "s".into(),
            version: 0,
            input: json!({}),
            context: json!({}),
            started_at: chrono::Utc::now(),
            run_env: praxec_core::RunEnv::for_test(),
            cancelled_at: None,
            cancelled_reason: None,
            depth: 0,
            parent: None,
        },
        transition: Some("approve".into()),
        arguments: json!({}),
        executor_config,
        idempotency_key: None,
        correlation_id: None,
    }
}

#[tokio::test]
async fn audit_failure_is_swallowed_and_request_still_queued() {
    let exec = HumanExecutor::with_audit(Arc::new(FailingAuditSink));
    let result = exec
        .execute(req(json!({ "queue": "approvals" })))
        .await
        .expect("a failing audit sink must NOT fail the human step");
    assert_eq!(result.output["status"], json!("queued"));
    assert_eq!(result.output["queue"], json!("approvals"));
    assert!(
        result.output["requestId"]
            .as_str()
            .unwrap()
            .starts_with("hr_")
    );
}

#[tokio::test]
async fn queue_defaults_when_unset() {
    let exec = HumanExecutor::new();
    let result = exec.execute(req(json!({}))).await.expect("queued");
    assert_eq!(result.output["queue"], json!("default"));
}
