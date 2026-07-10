//! Guarantee tests for SPEC §6.2 — typed blackboard slot validation.
//!
//! When a workflow declares `blackboard: { name: <JSON-Schema fragment> }` and
//! a transition's `output:` writes that slot, the post-write value must
//! conform to the fragment. Mismatch raises `BLACKBOARD_TYPE_ERROR` BEFORE the
//! transition advances — the snapshot version stays unchanged and the caller
//! sees a rejection response carrying the error code.

use std::sync::Arc;

use async_trait::async_trait;
use praxec_core::WorkflowRuntime;
use praxec_core::audit::{AuditSink, MemoryAuditSink};
use praxec_core::guards::DefaultGuardEvaluator;
use praxec_core::model::{Principal, StartWorkflow, SubmitTransition};
use praxec_core::ports::{Executor, ExecutorRegistry};
use praxec_core::store::{ConfigDefinitionStore, InMemoryWorkflowStore};
use serde_json::{Value, json};

// ── harness ──────────────────────────────────────────────────────────────────

/// Executor that returns a controlled `output` value, so the test can drive
/// the post-write blackboard value into any shape.
struct FixedOutputExecutor {
    output: Value,
}

#[async_trait]
impl Executor for FixedOutputExecutor {
    async fn execute(
        &self,
        _: praxec_core::model::ExecuteRequest,
    ) -> Result<praxec_core::model::ExecuteResult, praxec_core::error::ExecutorError> {
        Ok(praxec_core::model::ExecuteResult {
            output: self.output.clone(),
            evidence: vec![],
            child_workflow_id: None,
            next_transition: None,
            suspend: None,
            telemetry: None,
        })
    }
}

struct SingleExecRegistry {
    inner: Arc<dyn Executor>,
}

impl ExecutorRegistry for SingleExecRegistry {
    fn get(&self, _kind: &str) -> Option<Arc<dyn Executor>> {
        Some(self.inner.clone())
    }
}

fn build_runtime(config: Value, executor_output: Value) -> (WorkflowRuntime, Arc<MemoryAuditSink>) {
    let definitions = Arc::new(ConfigDefinitionStore::from_config(&config));
    let store = Arc::new(InMemoryWorkflowStore::new());
    let executors = Arc::new(SingleExecRegistry {
        inner: Arc::new(FixedOutputExecutor {
            output: executor_output,
        }),
    });
    let guards = Arc::new(DefaultGuardEvaluator::new());
    let audit = Arc::new(MemoryAuditSink::new());
    let runtime = WorkflowRuntime::new(
        definitions,
        store,
        executors,
        guards,
        audit.clone() as Arc<dyn AuditSink>,
    );
    (runtime, audit)
}

// ── test 1 ────────────────────────────────────────────────────────────────────
// Typed-slot mismatch aborts the transition with BLACKBOARD_TYPE_ERROR.

#[tokio::test]
async fn typed_slot_mismatch_aborts_with_blackboard_type_error() {
    let cfg = json!({
        "version": "1.0.0",
        "workflows": {
            "ci": {
                "initialState": "lint",
                "blackboard": {
                    "testCount": { "type": "integer" }
                },
                "states": {
                    "lint": {
                        "transitions": {
                            "done": {
                                "target": "deployed",
                                "actor": "agent",
                                "executor": { "kind": "noop" },
                                "output": { "testCount": "$.output.value" }
                            }
                        }
                    },
                    "deployed": { "terminal": true }
                }
            }
        }
    });

    // Executor returns a string where the schema says integer → violation.
    let (runtime, _audit) = build_runtime(cfg, json!({ "value": "not-an-integer" }));
    let start = runtime
        .start(StartWorkflow {
            definition_id: "ci".into(),
            input: json!({}),
            principal: Principal::anonymous(),
            trace_id: None,
            run_id: None,
            depth: 0,
            parent: None,
        })
        .await
        .unwrap();
    let workflow_id = start["workflow"]["id"].as_str().unwrap().to_string();
    let pre_version = start["workflow"]["version"].as_u64().unwrap();

    let resp = runtime
        .submit(SubmitTransition {
            workflow_id: workflow_id.clone(),
            expected_version: pre_version,
            transition: "done".into(),
            arguments: json!({}),
            principal: Principal::anonymous(),
            summary: None,
            trace_id: None,
            run_id: None,
        })
        .await
        .unwrap();

    // Rejection response: status=rejected, error code BLACKBOARD_TYPE_ERROR.
    assert_eq!(resp["result"]["status"], "running");
    assert_eq!(
        resp["error"]["code"].as_str(),
        Some("BLACKBOARD_TYPE_ERROR"),
        "expected BLACKBOARD_TYPE_ERROR; got: {}",
        resp["error"]
    );
    let message = resp["error"]["message"].as_str().unwrap_or_default();
    assert!(
        message.contains("testCount"),
        "error message should name the offending slot; got: {message}"
    );

    // Snapshot version unchanged — proof the transition was aborted, not committed.
    assert_eq!(
        resp["workflow"]["version"].as_u64(),
        Some(pre_version),
        "version must NOT advance when typed-slot validation fails"
    );
}

// ── test 2 ────────────────────────────────────────────────────────────────────
// Typed-slot WRITES that conform pass through cleanly.

#[tokio::test]
async fn typed_slot_conforming_value_advances_transition() {
    let cfg = json!({
        "version": "1.0.0",
        "workflows": {
            "ci": {
                "initialState": "lint",
                "blackboard": {
                    "testCount": { "type": "integer", "minimum": 0 }
                },
                "states": {
                    "lint": {
                        "transitions": {
                            "done": {
                                "target": "deployed",
                                "actor": "agent",
                                "executor": { "kind": "noop" },
                                "output": { "testCount": "$.output.value" }
                            }
                        }
                    },
                    "deployed": { "terminal": true }
                }
            }
        }
    });

    // Conforming integer — schema is satisfied; transition advances.
    let (runtime, _) = build_runtime(cfg, json!({ "value": 42 }));
    let start = runtime
        .start(StartWorkflow {
            definition_id: "ci".into(),
            input: json!({}),
            principal: Principal::anonymous(),
            trace_id: None,
            run_id: None,
            depth: 0,
            parent: None,
        })
        .await
        .unwrap();
    let workflow_id = start["workflow"]["id"].as_str().unwrap().to_string();
    let pre_version = start["workflow"]["version"].as_u64().unwrap();

    let resp = runtime
        .submit(SubmitTransition {
            workflow_id,
            expected_version: pre_version,
            transition: "done".into(),
            arguments: json!({}),
            principal: Principal::anonymous(),
            summary: None,
            trace_id: None,
            run_id: None,
        })
        .await
        .unwrap();

    assert!(
        resp["error"].is_null(),
        "conforming write must not be rejected; got: {resp}"
    );
    assert_eq!(resp["workflow"]["state"], "deployed");
    assert_eq!(resp["context"]["testCount"], 42);
}

// ── test 3 ────────────────────────────────────────────────────────────────────
// Bare-name slots (no schema fragment) accept any value — typed validation
// is opt-in per slot.

#[tokio::test]
async fn bare_name_slot_accepts_any_value() {
    let cfg = json!({
        "version": "1.0.0",
        "workflows": {
            "ci": {
                "initialState": "lint",
                "blackboard": {
                    "testCount": {}
                },
                "states": {
                    "lint": {
                        "transitions": {
                            "done": {
                                "target": "deployed",
                                "actor": "agent",
                                "executor": { "kind": "noop" },
                                "output": { "testCount": "$.output.value" }
                            }
                        }
                    },
                    "deployed": { "terminal": true }
                }
            }
        }
    });

    let (runtime, _) = build_runtime(cfg, json!({ "value": "string-is-fine" }));
    let start = runtime
        .start(StartWorkflow {
            definition_id: "ci".into(),
            input: json!({}),
            principal: Principal::anonymous(),
            trace_id: None,
            run_id: None,
            depth: 0,
            parent: None,
        })
        .await
        .unwrap();
    let resp = runtime
        .submit(SubmitTransition {
            workflow_id: start["workflow"]["id"].as_str().unwrap().to_string(),
            expected_version: start["workflow"]["version"].as_u64().unwrap(),
            transition: "done".into(),
            arguments: json!({}),
            principal: Principal::anonymous(),
            summary: None,
            trace_id: None,
            run_id: None,
        })
        .await
        .unwrap();
    assert!(
        resp["error"].is_null(),
        "bare-name slot must not enforce type; got: {resp}"
    );
}
