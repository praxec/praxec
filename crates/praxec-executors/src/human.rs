use std::sync::Arc;

use async_trait::async_trait;
use praxec_core::audit::{AuditEvent, AuditSink, NullAuditSink};
use praxec_core::error::ExecutorError;
use praxec_core::model::{Evidence, ExecuteRequest, ExecuteResult};
use praxec_core::ports::Executor;
use serde_json::{Value, json};
use uuid::Uuid;

/// Human-in-the-loop executor. Records `human.approval.requested` and returns
/// success with queue metadata — the actual approval comes via a later
/// `workflow.submit` from a human principal. Pair with
/// `actor: human` and `kind: permission` guards on the receiving transition.
pub struct HumanExecutor {
    audit: Arc<dyn AuditSink>,
}

impl HumanExecutor {
    pub fn new() -> Self {
        Self {
            audit: Arc::new(NullAuditSink),
        }
    }

    pub fn with_audit(audit: Arc<dyn AuditSink>) -> Self {
        Self { audit }
    }
}

impl Default for HumanExecutor {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait]
impl Executor for HumanExecutor {
    async fn execute(&self, request: ExecuteRequest) -> Result<ExecuteResult, ExecutorError> {
        let queue = request
            .executor_config
            .get("queue")
            .and_then(Value::as_str)
            .unwrap_or("default")
            .to_string();

        let request_id = format!("hr_{}", Uuid::new_v4().simple());

        self.audit
            .record(
                AuditEvent::new("human.approval.requested")
                    .with_workflow(&request.workflow.id)
                    .with_payload(json!({
                        "queue": queue,
                        "requestId": request_id,
                        "transition": request.transition,
                    })),
            )
            .await
            .unwrap_or_else(|e| tracing::warn!(error = %e, "audit emit failed; event dropped"));

        Ok(ExecuteResult {
            output: json!({
                "queue": queue,
                "requestId": request_id,
                "status": "queued",
            }),
            evidence: vec![Evidence {
                kind: "human_request".to_string(),
                id: request_id,
                uri: None,
                summary: Some(format!("Human action queued in '{queue}'")),
                digest: None,
                confidence: None,
            }],
            child_workflow_id: None,
            next_transition: None,
            suspend: None,
            telemetry: None,
        })
    }
}
