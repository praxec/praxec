//! A mock executor registry: every executor kind resolves to a no-op that
//! returns empty output, never spawning a process or calling a model.

use std::sync::Arc;

use async_trait::async_trait;
use praxec_core::error::ExecutorError;
use praxec_core::model::{ExecuteRequest, ExecuteResult};
use praxec_core::ports::{Executor, ExecutorRegistry};

pub struct MockExecutor;

#[async_trait]
impl Executor for MockExecutor {
    async fn execute(&self, _request: ExecuteRequest) -> Result<ExecuteResult, ExecutorError> {
        Ok(ExecuteResult {
            output: serde_json::json!({}),
            ..Default::default()
        })
    }
}

pub struct MockRegistry;

impl ExecutorRegistry for MockRegistry {
    fn get(&self, _kind: &str) -> Option<Arc<dyn Executor>> {
        Some(Arc::new(MockExecutor))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use praxec_core::model::WorkflowInstance;
    use serde_json::{Value, json};

    fn instance_stub() -> WorkflowInstance {
        WorkflowInstance {
            id: "wf_mock_test".into(),
            definition_id: "mock".into(),
            definition_version: "0".into(),
            definition: Value::Null,
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
        }
    }

    #[test]
    fn mock_registry_resolves_any_kind() {
        let reg = MockRegistry;
        assert!(reg.get("command").is_some());
        assert!(reg.get("llm").is_some());
        assert!(reg.get("anything_at_all").is_some());
    }

    #[tokio::test]
    async fn mock_executor_returns_empty_output() {
        let request = ExecuteRequest {
            workflow: instance_stub(),
            transition: None,
            arguments: json!({}),
            executor_config: Value::Null,
            idempotency_key: None,
            correlation_id: None,
        };
        let result = MockExecutor.execute(request).await.unwrap();
        assert_eq!(result.output, json!({}));
        assert!(result.evidence.is_empty());
        assert!(result.child_workflow_id.is_none());
        assert!(result.next_transition.is_none());
        assert!(result.suspend.is_none());
    }
}
