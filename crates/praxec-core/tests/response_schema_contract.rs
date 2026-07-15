//! Contract test C1 (testing-strategy) — the **real §32 response conforms to
//! `schemas/workflow-response.schema.json`**. This is the linchpin that keeps the
//! cockpit's hand-written `GatewayResponse` mirror and the `ScriptedGateway` mock
//! honest: if the runtime adds/changes a response field, this fails until the
//! schema (the shared contract) is updated to match.
//!
//! It produces a response that exercises the ADR-0008/0009 surface — `outcomes`,
//! the typed status, `orchestrator` — for both a `running` and a resolved mission.

use std::sync::Arc;

use praxec_core::audit::{AuditSink, MemoryAuditSink};
use praxec_core::error::ExecutorError;
use praxec_core::guards::DefaultGuardEvaluator;
use praxec_core::model::{
    ExecuteRequest, ExecuteResult, Principal, StartWorkflow, SubmitTransition,
};
use praxec_core::ports::{Executor, ExecutorRegistry, WorkflowStore};
use praxec_core::runtime::WorkflowRuntime;
use praxec_core::store::{ConfigDefinitionStore, InMemoryWorkflowStore};
use serde_json::{Value, json};

const SCHEMA: &str = include_str!("../../../schemas/workflow-response.schema.json");

struct NoopExec;
#[async_trait::async_trait]
impl Executor for NoopExec {
    async fn execute(&self, _r: ExecuteRequest) -> Result<ExecuteResult, ExecutorError> {
        Ok(ExecuteResult::default())
    }
}
struct NoopReg;
impl ExecutorRegistry for NoopReg {
    fn get(&self, _kind: &str) -> Option<Arc<dyn Executor>> {
        Some(Arc::new(NoopExec))
    }
}

/// A workflow exercising the full response surface: an `orchestrator` binding,
/// an `outcomes` block, a human gate, and a `success` terminal.
fn runtime() -> WorkflowRuntime {
    let config = json!({
        "workflows": { "demo": {
            "orchestrator": "anthropic:claude-sonnet-4-6",
            "initialState": "review",
            "outcomes": [
                { "id": "approved", "statement": "A human approved.", "check": "$.context.approved == true" }
            ],
            "blackboard": { "approved": { "type": "boolean" } },
            "states": {
                "review": {
                    "goal": "Approve the change.",
                    "actor": "human",
                    "transitions": {
                        "approve": {
                            "target": "done",
                            "actor": "human",
                            "executor": { "kind": "noop" },
                            "output": { "approved": true }
                        }
                    }
                },
                "done": { "terminal": true, "outcome": "success" }
            }
        }}
    });
    let evidence = Arc::new(praxec_core::store::InMemoryEvidenceStore::new());
    WorkflowRuntime::new(
        Arc::new(ConfigDefinitionStore::from_config(&config)),
        Arc::new(InMemoryWorkflowStore::new()) as Arc<dyn WorkflowStore>,
        Arc::new(NoopReg),
        Arc::new(DefaultGuardEvaluator::with_evidence(evidence.clone())),
        Arc::new(MemoryAuditSink::new()) as Arc<dyn AuditSink>,
    )
    .with_writable_repo_roots(vec![praxec_core::RepoRoot::for_test()])
    .with_evidence(evidence)
}

fn schema_errors(response: &Value) -> Vec<String> {
    let schema: Value = serde_json::from_str(SCHEMA).expect("the response schema is valid JSON");
    let validator = jsonschema::validator_for(&schema).expect("the response schema compiles");
    validator
        .iter_errors(response)
        .map(|e| e.to_string())
        .collect()
}

#[tokio::test]
async fn a_running_mission_response_conforms_to_the_schema() {
    let rt = runtime();
    let start = rt
        .start(StartWorkflow {
            definition_id: "demo".into(),
            input: json!({}),
            principal: Principal::anonymous(),
            run_env: praxec_core::RunEnv::for_test(),
            depth: 0,
            parent: None,
        })
        .await
        .expect("start succeeds");
    let errors = schema_errors(&start);
    assert!(
        errors.is_empty(),
        "start response violates the schema:\n{errors:#?}\n\nresponse: {start:#}"
    );
}

#[tokio::test]
async fn a_resolved_mission_response_conforms_to_the_schema() {
    let rt = runtime();
    let start = rt
        .start(StartWorkflow {
            definition_id: "demo".into(),
            input: json!({}),
            principal: Principal::anonymous(),
            run_env: praxec_core::RunEnv::for_test(),
            depth: 0,
            parent: None,
        })
        .await
        .expect("start succeeds");
    let id = start["workflow"]["id"].as_str().unwrap().to_string();
    let version = start["workflow"]["version"].as_u64().unwrap();
    let resolved = rt
        .submit(SubmitTransition {
            workflow_id: id,
            expected_version: version,
            transition: "approve".into(),
            arguments: json!({}),
            principal: Principal::anonymous(),
            summary: None,
            trace_id: None,
            run_id: None,
        })
        .await
        .expect("approve succeeds");
    let errors = schema_errors(&resolved);
    assert!(
        errors.is_empty(),
        "resolved response violates the schema:\n{errors:#?}\n\nresponse: {resolved:#}"
    );
}

#[tokio::test]
async fn the_running_response_carries_the_orchestrator_binding() {
    let rt = runtime();
    let start = rt
        .start(StartWorkflow {
            definition_id: "demo".into(),
            input: json!({}),
            principal: Principal::anonymous(),
            run_env: praxec_core::RunEnv::for_test(),
            depth: 0,
            parent: None,
        })
        .await
        .expect("start succeeds");
    assert_eq!(
        start["orchestrator"].as_str(),
        Some("anthropic:claude-sonnet-4-6")
    );
}
