//! End-to-end test: evidence collected from one transition's executor must be
//! visible to an `evidence` guard on a later transition.

use std::sync::Arc;

use async_trait::async_trait;
use praxec_core::audit::NullAuditSink;
use praxec_core::error::ExecutorError;
use praxec_core::guards::DefaultGuardEvaluator;
use praxec_core::model::{
    Evidence, ExecuteRequest, ExecuteResult, Principal, StartWorkflow, SubmitTransition,
};
use praxec_core::ports::{Executor, ExecutorRegistry};
use praxec_core::store::{ConfigDefinitionStore, InMemoryEvidenceStore, InMemoryWorkflowStore};
use praxec_core::WorkflowRuntime;
use serde_json::json;

struct EmitsEvidence(&'static str);

#[async_trait]
impl Executor for EmitsEvidence {
    async fn execute(&self, _req: ExecuteRequest) -> Result<ExecuteResult, ExecutorError> {
        Ok(ExecuteResult {
            output: json!({}),
            evidence: vec![Evidence {
                kind: self.0.to_string(),
                id: format!("ev_{}", self.0),
                uri: None,
                summary: None,
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

struct NoopExec;

#[async_trait]
impl Executor for NoopExec {
    async fn execute(&self, _req: ExecuteRequest) -> Result<ExecuteResult, ExecutorError> {
        Ok(ExecuteResult::default())
    }
}

struct PairRegistry {
    tests: Arc<dyn Executor>,
    accept: Arc<dyn Executor>,
}

impl ExecutorRegistry for PairRegistry {
    fn get(&self, kind: &str) -> Option<Arc<dyn Executor>> {
        match kind {
            "tests_pass" => Some(self.tests.clone()),
            "noop" => Some(self.accept.clone()),
            _ => None,
        }
    }
}

fn config() -> serde_json::Value {
    json!({
        "version": "1.0.0",
        "workflows": {
            "demo": {
                "initialState": "open",
                "states": {
                    "open": {
                        "transitions": {
                            "run_tests": {
                                "target": "tested",
                                "executor": { "kind": "tests_pass" }
                            }
                        }
                    },
                    "tested": {
                        "transitions": {
                            "verify": {
                                "target": "done",
                                "guards": [
                                    { "kind": "evidence", "requires": ["tests_passed"] }
                                ],
                                "executor": { "kind": "noop" }
                            }
                        }
                    },
                    "done": { "terminal": true }
                }
            }
        }
    })
}

fn build() -> (WorkflowRuntime, Arc<InMemoryEvidenceStore>) {
    let evidence = Arc::new(InMemoryEvidenceStore::new());
    let definitions = Arc::new(ConfigDefinitionStore::from_config(&config()));
    let store = Arc::new(InMemoryWorkflowStore::new());
    let executors = Arc::new(PairRegistry {
        tests: Arc::new(EmitsEvidence("tests_passed")),
        accept: Arc::new(NoopExec),
    });
    let guards = Arc::new(DefaultGuardEvaluator::with_evidence(
        evidence.clone() as Arc<dyn praxec_core::ports::EvidenceStore>
    ));
    let runtime = WorkflowRuntime::new(
        definitions,
        store,
        executors,
        guards,
        Arc::new(NullAuditSink),
    )
    .with_evidence(evidence.clone());
    (runtime, evidence)
}

#[tokio::test]
async fn evidence_guard_blocks_until_required_kind_recorded() {
    let (runtime, _) = build();

    let started = runtime
        .start(StartWorkflow {
            definition_id: "demo".into(),
            input: json!({}),
            principal: Principal::anonymous(),
            trace_id: None,
            run_id: None,
            depth: 0,
            parent: None,
        })
        .await
        .unwrap();
    let workflow_id = started["workflow"]["id"].as_str().unwrap().to_string();
    let mut version = started["workflow"]["version"].as_u64().unwrap();

    // Skip run_tests and try to verify directly: should be GUARD_REJECTED.
    // First we need to advance state to `tested` (the verify transition only
    // exists there). To exercise the "missing evidence" branch we'll route
    // through run_tests but with an executor that emits the WRONG evidence kind.
    // For directness here, we run the happy path and confirm verify succeeds
    // because run_tests records `tests_passed`.
    let after_tests = runtime
        .submit(SubmitTransition {
            workflow_id: workflow_id.clone(),
            expected_version: version,
            transition: "run_tests".into(),
            arguments: json!({}),
            principal: Principal::anonymous(),
            summary: None,
            trace_id: None,
            run_id: None,
        })
        .await
        .unwrap();
    version = after_tests["workflow"]["version"].as_u64().unwrap();

    let after_verify = runtime
        .submit(SubmitTransition {
            workflow_id,
            expected_version: version,
            transition: "verify".into(),
            arguments: json!({}),
            principal: Principal::anonymous(),
            summary: None,
            trace_id: None,
            run_id: None,
        })
        .await
        .unwrap();
    assert_eq!(after_verify["result"]["status"], "succeeded");
    assert_eq!(after_verify["workflow"]["state"], "done");
}

#[tokio::test]
async fn evidence_guard_rejects_without_required_kind() {
    // Same definitions but the executor emits a *different* evidence kind,
    // so verify's `requires: [tests_passed]` is unsatisfied.
    let evidence = Arc::new(InMemoryEvidenceStore::new());
    let definitions = Arc::new(ConfigDefinitionStore::from_config(&config()));
    let store = Arc::new(InMemoryWorkflowStore::new());

    struct WrongEvidence;
    #[async_trait]
    impl Executor for WrongEvidence {
        async fn execute(&self, _req: ExecuteRequest) -> Result<ExecuteResult, ExecutorError> {
            Ok(ExecuteResult {
                output: json!({}),
                evidence: vec![Evidence {
                    kind: "lint_passed".into(),
                    id: "ev_lint".into(),
                    uri: None,
                    summary: None,
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

    struct WrongRegistry {
        wrong: Arc<dyn Executor>,
        noop: Arc<dyn Executor>,
    }
    impl ExecutorRegistry for WrongRegistry {
        fn get(&self, kind: &str) -> Option<Arc<dyn Executor>> {
            match kind {
                "tests_pass" => Some(self.wrong.clone()),
                "noop" => Some(self.noop.clone()),
                _ => None,
            }
        }
    }

    let executors = Arc::new(WrongRegistry {
        wrong: Arc::new(WrongEvidence),
        noop: Arc::new(NoopExec),
    });
    let guards = Arc::new(DefaultGuardEvaluator::with_evidence(
        evidence.clone() as Arc<dyn praxec_core::ports::EvidenceStore>
    ));
    let runtime = WorkflowRuntime::new(
        definitions,
        store,
        executors,
        guards,
        Arc::new(NullAuditSink),
    )
    .with_evidence(evidence);

    let started = runtime
        .start(StartWorkflow {
            definition_id: "demo".into(),
            input: json!({}),
            principal: Principal::anonymous(),
            trace_id: None,
            run_id: None,
            depth: 0,
            parent: None,
        })
        .await
        .unwrap();
    let workflow_id = started["workflow"]["id"].as_str().unwrap().to_string();
    let version = started["workflow"]["version"].as_u64().unwrap();

    let after_tests = runtime
        .submit(SubmitTransition {
            workflow_id: workflow_id.clone(),
            expected_version: version,
            transition: "run_tests".into(),
            arguments: json!({}),
            principal: Principal::anonymous(),
            summary: None,
            trace_id: None,
            run_id: None,
        })
        .await
        .unwrap();
    let next_version = after_tests["workflow"]["version"].as_u64().unwrap();

    let denied = runtime
        .submit(SubmitTransition {
            workflow_id,
            expected_version: next_version,
            transition: "verify".into(),
            arguments: json!({}),
            principal: Principal::anonymous(),
            summary: None,
            trace_id: None,
            run_id: None,
        })
        .await
        .unwrap();
    assert_eq!(denied["error"]["code"], "GUARD_REJECTED");
}
