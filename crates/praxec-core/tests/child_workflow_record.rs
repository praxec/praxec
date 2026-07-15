//! Guarantee test for SPEC §7.2 — `childWorkflowId` is set on transition
//! records when the executor was `kind: workflow` and reported the spawned
//! sub-workflow id.
//!
//! We don't depend on the executors crate here; we install a synthetic
//! executor that mimics the WorkflowExecutor's contract — returning a
//! `child_workflow_id` on its `ExecuteResult` — and assert the runtime
//! propagates it onto the record.

use std::sync::Arc;

use async_trait::async_trait;
use praxec_core::WorkflowRuntime;
use praxec_core::audit::{AuditSink, MemoryAuditSink};
use praxec_core::guards::DefaultGuardEvaluator;
use praxec_core::model::{Principal, StartWorkflow, SubmitTransition};
use praxec_core::ports::{Executor, ExecutorRegistry};
use praxec_core::store::{ConfigDefinitionStore, InMemoryWorkflowStore};
use serde_json::{Value, json};

struct ChildWorkflowExecutor {
    child_id: String,
}

#[async_trait]
impl Executor for ChildWorkflowExecutor {
    async fn execute(
        &self,
        _: praxec_core::model::ExecuteRequest,
    ) -> Result<praxec_core::model::ExecuteResult, praxec_core::error::ExecutorError> {
        Ok(praxec_core::model::ExecuteResult {
            output: json!({}),
            evidence: vec![],
            child_workflow_id: Some(self.child_id.clone()),
            next_transition: None,
            suspend: None,
            telemetry: None,
        })
    }
}

struct AnyKindRegistry(Arc<dyn Executor>);
impl ExecutorRegistry for AnyKindRegistry {
    fn get(&self, _kind: &str) -> Option<Arc<dyn Executor>> {
        Some(self.0.clone())
    }
}

#[tokio::test]
async fn workflow_executor_sets_child_workflow_id_on_record() {
    let cfg = json!({
        "version": "1.0.0",
        "workflows": {
            "parent": {
                "initialState": "spawn",
                "states": {
                    "spawn": {
                        "transitions": {
                            "go": {
                                "target": "done",
                                "actor": "agent",
                                "executor": { "kind": "workflow", "definitionId": "child" }
                            }
                        }
                    },
                    "done": { "terminal": true }
                }
            },
            "child": {
                "initialState": "s",
                "states": { "s": { "terminal": true } }
            }
        }
    });

    let definitions = Arc::new(ConfigDefinitionStore::from_config(&cfg));
    let store = Arc::new(InMemoryWorkflowStore::new());
    let executors = Arc::new(AnyKindRegistry(Arc::new(ChildWorkflowExecutor {
        child_id: "wf_child_abc".to_string(),
    })));
    let guards = Arc::new(DefaultGuardEvaluator::new());
    let audit: Arc<dyn AuditSink> = Arc::new(MemoryAuditSink::new());
    let runtime = WorkflowRuntime::new(definitions, store, executors, guards, audit.clone())
        .with_writable_repo_roots(vec![praxec_core::RepoRoot::for_test()]);

    let start = runtime
        .start(StartWorkflow {
            definition_id: "parent".into(),
            input: json!({}),
            principal: Principal::anonymous(),
            run_env: praxec_core::RunEnv::for_test(),
            depth: 0,
            parent: None,
        })
        .await
        .unwrap();
    let workflow_id = start["workflow"]["id"].as_str().unwrap().to_string();
    let version = start["workflow"]["version"].as_u64().unwrap();

    runtime
        .submit(SubmitTransition {
            workflow_id,
            expected_version: version,
            transition: "go".into(),
            arguments: json!({}),
            principal: Principal::anonymous(),
            summary: None,
            trace_id: None,
            run_id: None,
        })
        .await
        .unwrap();

    let events = audit.list_events().await.expect("memory sink lists");
    let record = events
        .iter()
        .find(|e| e.event_type == "workflow.transition")
        .expect("a workflow.transition record must be emitted");
    assert_eq!(
        record.payload["childWorkflowId"].as_str(),
        Some("wf_child_abc"),
        "childWorkflowId must carry the spawned sub-workflow id; got: {}",
        record.payload
    );
}

#[tokio::test]
async fn non_workflow_executor_leaves_child_workflow_id_null() {
    // A transition whose executor returns no child_workflow_id must emit
    // childWorkflowId: null on the record (no leakage from prior runs).
    struct PlainExecutor;
    #[async_trait]
    impl Executor for PlainExecutor {
        async fn execute(
            &self,
            _: praxec_core::model::ExecuteRequest,
        ) -> Result<praxec_core::model::ExecuteResult, praxec_core::error::ExecutorError> {
            Ok(praxec_core::model::ExecuteResult::default())
        }
    }

    let cfg = json!({
        "version": "1.0.0",
        "workflows": {
            "wf": {
                "initialState": "a",
                "states": {
                    "a": {
                        "transitions": {
                            "go": {
                                "target": "b",
                                "actor": "agent",
                                "executor": { "kind": "noop" }
                            }
                        }
                    },
                    "b": { "terminal": true }
                }
            }
        }
    });

    let definitions = Arc::new(ConfigDefinitionStore::from_config(&cfg));
    let store = Arc::new(InMemoryWorkflowStore::new());
    let executors = Arc::new(AnyKindRegistry(Arc::new(PlainExecutor)));
    let guards = Arc::new(DefaultGuardEvaluator::new());
    let audit: Arc<dyn AuditSink> = Arc::new(MemoryAuditSink::new());
    let runtime = WorkflowRuntime::new(definitions, store, executors, guards, audit.clone())
        .with_writable_repo_roots(vec![praxec_core::RepoRoot::for_test()]);

    let start = runtime
        .start(StartWorkflow {
            definition_id: "wf".into(),
            input: json!({}),
            principal: Principal::anonymous(),
            run_env: praxec_core::RunEnv::for_test(),
            depth: 0,
            parent: None,
        })
        .await
        .unwrap();
    let workflow_id = start["workflow"]["id"].as_str().unwrap().to_string();
    let version = start["workflow"]["version"].as_u64().unwrap();
    runtime
        .submit(SubmitTransition {
            workflow_id,
            expected_version: version,
            transition: "go".into(),
            arguments: json!({}),
            principal: Principal::anonymous(),
            summary: None,
            trace_id: None,
            run_id: None,
        })
        .await
        .unwrap();

    let events = audit.list_events().await.expect("memory sink lists");
    let record = events
        .iter()
        .find(|e| e.event_type == "workflow.transition")
        .expect("record must be present");
    assert_eq!(
        record.payload["childWorkflowId"],
        Value::Null,
        "non-workflow executor must leave childWorkflowId null"
    );
}
