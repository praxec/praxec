//! SPEC §20.1 + §20.4 — end-to-end: an executor returning evidence with
//! out-of-range confidence causes the submit to reject with
//! `INVALID_CONFIDENCE` rather than silently poisoning downstream guards.

use std::sync::Arc;

use async_trait::async_trait;
use praxec_core::WorkflowRuntime;
use praxec_core::audit::{AuditSink, MemoryAuditSink};
use praxec_core::error::ExecutorError;
use praxec_core::guards::DefaultGuardEvaluator;
use praxec_core::model::{
    Evidence, ExecuteRequest, ExecuteResult, Principal, StartWorkflow, SubmitTransition,
};
use praxec_core::ports::{Executor, ExecutorRegistry};
use praxec_core::store::{ConfigDefinitionStore, InMemoryWorkflowStore};
use serde_json::{Value, json};

/// Executor that returns one evidence record with the supplied confidence
/// (which the test can set out-of-range to trigger SPEC §20.1 validation).
struct EvidenceWithConfidence(f32);

#[async_trait]
impl Executor for EvidenceWithConfidence {
    async fn execute(&self, _req: ExecuteRequest) -> Result<ExecuteResult, ExecutorError> {
        Ok(ExecuteResult {
            output: json!({}),
            evidence: vec![Evidence {
                kind: "review".into(),
                id: "ev_test".into(),
                uri: None,
                summary: None,
                digest: None,
                confidence: Some(self.0),
            }],
            child_workflow_id: None,
            next_transition: None,
            suspend: None,
            telemetry: None,
        })
    }
}

struct OneKindRegistry(Arc<dyn Executor>);
impl ExecutorRegistry for OneKindRegistry {
    fn get(&self, _kind: &str) -> Option<Arc<dyn Executor>> {
        Some(self.0.clone())
    }
}

fn fixture_config() -> Value {
    json!({
        "version": "1.0.0",
        "workflows": {
            "demo": {
                "initialState": "s",
                "states": {
                    "s": {
                        "transitions": {
                            "go": { "target": "done", "executor": { "kind": "noop" } }
                        }
                    },
                    "done": { "terminal": true }
                }
            }
        }
    })
}

fn build_runtime(executor: Arc<dyn Executor>) -> WorkflowRuntime {
    let resolved = praxec_core::config::resolve(fixture_config()).expect("resolve");
    let defs = Arc::new(ConfigDefinitionStore::from_config(&resolved));
    let store = Arc::new(InMemoryWorkflowStore::new());
    let audit = Arc::new(MemoryAuditSink::new()) as Arc<dyn AuditSink>;
    let guards = Arc::new(DefaultGuardEvaluator::new());
    WorkflowRuntime::new(
        defs,
        store,
        Arc::new(OneKindRegistry(executor)),
        guards,
        audit,
    )
}

#[tokio::test]
async fn out_of_range_negative_confidence_rejects_with_invalid_confidence() {
    let runtime = build_runtime(Arc::new(EvidenceWithConfidence(-0.5)));
    let start = runtime
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
        .expect("start");
    let workflow_id = start["workflow"]["id"].as_str().unwrap().to_string();
    let version = start["workflow"]["version"].as_u64().unwrap();

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
        .expect("submit returns Ok with rejection in body");

    assert_eq!(
        resp["error"]["code"].as_str(),
        Some("INVALID_CONFIDENCE"),
        "negative confidence must surface as INVALID_CONFIDENCE; got: {resp}"
    );
}

#[tokio::test]
async fn out_of_range_above_one_confidence_rejects_with_invalid_confidence() {
    let runtime = build_runtime(Arc::new(EvidenceWithConfidence(1.5)));
    let start = runtime
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
        .expect("start");
    let workflow_id = start["workflow"]["id"].as_str().unwrap().to_string();
    let version = start["workflow"]["version"].as_u64().unwrap();

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
        .expect("submit returns Ok with rejection in body");

    assert_eq!(resp["error"]["code"].as_str(), Some("INVALID_CONFIDENCE"));
}

#[tokio::test]
async fn in_range_confidence_passes_through() {
    let runtime = build_runtime(Arc::new(EvidenceWithConfidence(0.85)));
    let start = runtime
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
        .expect("start");
    let workflow_id = start["workflow"]["id"].as_str().unwrap().to_string();
    let version = start["workflow"]["version"].as_u64().unwrap();

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
        .expect("submit");

    // The fixture workflow goes to terminal "done"; confidence-in-range
    // means we never hit the rejection path.
    assert!(
        resp.get("error").map(|e| e.is_null()).unwrap_or(true),
        "in-range confidence must not trigger INVALID_CONFIDENCE; got: {resp}"
    );
}
