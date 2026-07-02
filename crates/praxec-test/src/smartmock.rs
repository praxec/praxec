//! A mock executor that emits the planned output for the transition it's running
//! (so guard-gated flows traverse). Falls back to `{}` when no plan exists.
//! Optionally injects failures via a [`FailureInjector`].

use std::collections::HashMap;
use std::sync::Arc;

use async_trait::async_trait;
use serde_json::{json, Map, Value};

use praxec_core::error::ExecutorError;
use praxec_core::model::{ExecuteRequest, ExecuteResult};
use praxec_core::ports::{Executor, ExecutorRegistry};

use crate::inject::FailureInjector;

#[derive(Clone)]
pub struct SmartMockRegistry {
    plan: Arc<HashMap<String, Map<String, Value>>>,
    injector: Option<Arc<FailureInjector>>,
}

impl SmartMockRegistry {
    /// No failure injection — Task 4's unit test uses this.
    pub fn new(plan: HashMap<String, Map<String, Value>>) -> Self {
        Self {
            plan: Arc::new(plan),
            injector: None,
        }
    }

    /// With failure injection: `rate` is 0–100 (percent of calls that fail).
    pub fn with_injection(plan: HashMap<String, Map<String, Value>>, seed: u64, rate: u8) -> Self {
        Self {
            plan: Arc::new(plan),
            injector: Some(Arc::new(FailureInjector::new(seed, rate))),
        }
    }
}

struct SmartMockExecutor {
    plan: Arc<HashMap<String, Map<String, Value>>>,
    injector: Option<Arc<FailureInjector>>,
}

impl ExecutorRegistry for SmartMockRegistry {
    fn get(&self, _kind: &str) -> Option<Arc<dyn Executor>> {
        Some(Arc::new(SmartMockExecutor {
            plan: self.plan.clone(),
            injector: self.injector.clone(),
        }))
    }
}

#[async_trait]
impl Executor for SmartMockExecutor {
    async fn execute(&self, request: ExecuteRequest) -> Result<ExecuteResult, ExecutorError> {
        if self
            .injector
            .as_ref()
            .map(|i| i.should_fail())
            .unwrap_or(false)
        {
            return Err(ExecutorError::Permanent("fuzz-injected failure".into()));
        }
        let key = request.transition.clone().unwrap_or_default();
        let output = self
            .plan
            .get(&key)
            .map(|m| Value::Object(m.clone()))
            .unwrap_or_else(|| json!({}));
        Ok(ExecuteResult {
            output,
            evidence: vec![],
            child_workflow_id: None,
            next_transition: None,
            suspend: None,
            telemetry: None,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use praxec_core::model::WorkflowInstance;

    fn instance_stub() -> WorkflowInstance {
        WorkflowInstance {
            id: "wf_smartmock_test".into(),
            definition_id: "smartmock".into(),
            definition_version: "0".into(),
            definition: Value::Null,
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

    fn make_request(transition: Option<&str>) -> ExecuteRequest {
        ExecuteRequest {
            workflow: instance_stub(),
            transition: transition.map(str::to_string),
            arguments: json!({}),
            executor_config: Value::Null,
            idempotency_key: None,
            correlation_id: None,
        }
    }

    #[tokio::test]
    async fn emits_planned_output_for_transition() {
        let mut plan = HashMap::new();
        let mut fields = Map::new();
        fields.insert("approved".into(), json!(true));
        plan.insert("go".into(), fields);

        let reg = SmartMockRegistry::new(plan);
        let ex = reg.get("noop").unwrap();

        let req = make_request(Some("go"));
        let r = ex.execute(req).await.unwrap();
        assert_eq!(r.output, json!({ "approved": true }));

        // unknown transition → empty
        let r2 = ex.execute(make_request(Some("other"))).await.unwrap();
        assert_eq!(r2.output, json!({}));
    }
}
