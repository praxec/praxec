//! Actor-gate tests + audit-records tests.
//!
//! Split from `tests/invariants.rs` (SPLIT-002). Shared fixtures live in
//! `tests/common/invariants.rs`. The fallback-selected test uses its own
//! pair-registry inline because it needs two distinct executor kinds.

mod common;

use std::sync::Arc;

use common::invariants::*;

use async_trait::async_trait;
use praxec_core::WorkflowRuntime;
use praxec_core::audit::{AuditSink, MemoryAuditSink};
use praxec_core::error::ExecutorError;
use praxec_core::guards::DefaultGuardEvaluator;
use praxec_core::model::{
    ExecuteRequest, ExecuteResult, Principal, StartWorkflow, SubmitTransition,
};
use praxec_core::ports::{Executor, ExecutorRegistry};
use praxec_core::store::{ConfigDefinitionStore, InMemoryWorkflowStore};
use serde_json::json;

// ---- bonus: audit emits a workflow.transitioned event on successful submit -

#[tokio::test]
async fn audit_records_workflow_transitioned() {
    let (runtime, _, audit) = build_runtime(governed_config(), json!({}));
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

    runtime
        .submit(SubmitTransition {
            workflow_id,
            expected_version: version,
            transition: "approve".into(),
            arguments: json!({}),
            principal: principal_with(&["demo.approve"]),
            summary: None,
            trace_id: None,
            run_id: None,
        })
        .await
        .unwrap();

    let types = audit.event_types();
    assert!(types.iter().any(|t| t == "workflow.started"));
    assert!(types.iter().any(|t| t == "transition.requested"));
    assert!(types.iter().any(|t| t == "guard.evaluated"));
    assert!(types.iter().any(|t| t == "executor.started"));
    assert!(types.iter().any(|t| t == "executor.succeeded"));
    assert!(types.iter().any(|t| t == "workflow.transitioned"));
    assert!(types.iter().any(|t| t == "workflow.completed"));
}

#[tokio::test]
async fn audit_records_transition_rejected_on_guard_rejection() {
    let (runtime, _, audit) = build_runtime(governed_config(), json!({}));
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

    // No permission → guard rejects → transition.rejected audited.
    runtime
        .submit(SubmitTransition {
            workflow_id,
            expected_version: version,
            transition: "approve".into(),
            arguments: json!({}),
            principal: Principal::anonymous(),
            summary: None,
            trace_id: None,
            run_id: None,
        })
        .await
        .unwrap();

    let events = audit.snapshot();
    let rejection = events
        .iter()
        .find(|e| e.event_type == "transition.rejected")
        .expect("transition.rejected event must be emitted");
    assert_eq!(rejection.payload["code"], "GUARD_REJECTED");
    assert_eq!(rejection.payload["transition"], "approve");
}

#[tokio::test]
async fn audit_records_transition_rejected_on_stale_version() {
    let (runtime, _, audit) = build_runtime(governed_config(), json!({}));
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

    runtime
        .submit(SubmitTransition {
            workflow_id,
            expected_version: version + 99,
            transition: "approve".into(),
            arguments: json!({}),
            principal: principal_with(&["demo.approve"]),
            summary: None,
            trace_id: None,
            run_id: None,
        })
        .await
        .unwrap();

    let codes: Vec<String> = audit
        .snapshot()
        .iter()
        .filter(|e| e.event_type == "transition.rejected")
        .filter_map(|e| {
            e.payload
                .get("code")
                .and_then(|c| c.as_str())
                .map(String::from)
        })
        .collect();
    assert!(codes.contains(&"STALE_WORKFLOW_VERSION".to_string()));
}

#[tokio::test]
async fn audit_records_fallback_selected_when_primary_exhausts() {
    // Build a config whose transition has a fallback executor; primary will
    // always fail; fallback should succeed and the audit must capture
    // `fallback.selected` exactly once for the second candidate.
    let cfg = json!({
        "version": "1.0.0",
        "workflows": {
            "demo": {
                "initialState": "open",
                "states": {
                    "open": {
                        "transitions": {
                            "go": {
                                "target": "done",
                                "executor": { "kind": "always_fail" },
                                "reliability": {
                                    "retry": { "maxAttempts": 1 },
                                    "fallback": {
                                        "executors": [{ "kind": "always_ok" }]
                                    }
                                }
                            }
                        }
                    },
                    "done": { "terminal": true }
                }
            }
        }
    });

    // Custom registry: two executors, one always fails, one always succeeds.
    struct AlwaysFail;
    #[async_trait]
    impl Executor for AlwaysFail {
        async fn execute(&self, _: ExecuteRequest) -> Result<ExecuteResult, ExecutorError> {
            Err(ExecutorError::Transient("nope".into()))
        }
    }
    struct AlwaysOk;
    #[async_trait]
    impl Executor for AlwaysOk {
        async fn execute(&self, _: ExecuteRequest) -> Result<ExecuteResult, ExecutorError> {
            Ok(ExecuteResult::default())
        }
    }
    struct PairRegistry {
        fail: Arc<dyn Executor>,
        ok: Arc<dyn Executor>,
    }
    impl ExecutorRegistry for PairRegistry {
        fn get(&self, kind: &str) -> Option<Arc<dyn Executor>> {
            match kind {
                "always_fail" => Some(self.fail.clone()),
                "always_ok" => Some(self.ok.clone()),
                _ => None,
            }
        }
    }

    let definitions = Arc::new(ConfigDefinitionStore::from_config(&cfg));
    let store = Arc::new(InMemoryWorkflowStore::new());
    let executors = Arc::new(PairRegistry {
        fail: Arc::new(AlwaysFail),
        ok: Arc::new(AlwaysOk),
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

    let resp = runtime
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

    // Result should be completed because fallback succeeded and target is terminal.
    assert_eq!(resp["result"]["status"], "succeeded");

    let fallbacks: Vec<_> = audit
        .snapshot()
        .into_iter()
        .filter(|e| e.event_type == "fallback.selected")
        .collect();
    assert_eq!(fallbacks.len(), 1, "exactly one fallback.selected event");
    assert_eq!(fallbacks[0].payload["candidate"], 1);
    assert_eq!(fallbacks[0].payload["kind"], "always_ok");
}

// ---- Actor gate: human-only transitions reject agent submits ---------------

#[tokio::test]
async fn actor_gate_rejects_agent_on_human_only_transition() {
    let (runtime, exec, audit) = build_runtime(human_only_config(), json!({}));
    let started = runtime
        .start(StartWorkflow {
            definition_id: "approval".into(),
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

    // Anonymous (agent-equivalent) principal must be rejected without
    // the executor ever running.
    let denied = runtime
        .submit(SubmitTransition {
            workflow_id: workflow_id.clone(),
            expected_version: version,
            transition: "approve".into(),
            arguments: json!({}),
            principal: Principal::anonymous(),
            summary: None,
            trace_id: None,
            run_id: None,
        })
        .await
        .unwrap();
    assert_eq!(denied["result"]["status"], "running");
    assert_eq!(denied["error"]["code"], "ACTOR_MISMATCH");
    assert_eq!(denied["workflow"]["state"], "pending");
    assert_eq!(exec.count(), 0, "executor must not run on actor mismatch");

    let rejections: Vec<_> = audit
        .snapshot()
        .into_iter()
        .filter(|e| e.event_type == "transition.rejected" && e.payload["code"] == "ACTOR_MISMATCH")
        .collect();
    assert_eq!(rejections.len(), 1, "one ACTOR_MISMATCH audit event");
}

#[tokio::test]
async fn actor_gate_admits_human_on_human_only_transition() {
    let (runtime, exec, _) = build_runtime(human_only_config(), json!({}));
    let started = runtime
        .start(StartWorkflow {
            definition_id: "approval".into(),
            input: json!({}),
            principal: human_principal(),
            trace_id: None,
            run_id: None,
            depth: 0,
            parent: None,
        })
        .await
        .unwrap();
    let workflow_id = started["workflow"]["id"].as_str().unwrap().to_string();
    let version = started["workflow"]["version"].as_u64().unwrap();

    let resp = runtime
        .submit(SubmitTransition {
            workflow_id,
            expected_version: version,
            transition: "approve".into(),
            arguments: json!({}),
            principal: human_principal(),
            summary: None,
            trace_id: None,
            run_id: None,
        })
        .await
        .unwrap();
    assert_eq!(resp["result"]["status"], "succeeded");
    assert_eq!(resp["workflow"]["state"], "done");
    assert_eq!(exec.count(), 1);
}
