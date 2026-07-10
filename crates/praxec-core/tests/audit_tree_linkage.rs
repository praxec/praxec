//! L1 tree-linkage — `workflow.started` carries the two optional tree-edge
//! fields (`parent_workflow_id`, `depth`) so a stream observer reconstructs the
//! execution tree (node = `workflow_id`, edge = `parent_workflow_id`) from the
//! audit stream. The edges are stamped from the ALREADY-PERSISTED
//! `WorkflowInstance` (`parent` link + `depth`), not a task-local, and only at
//! the single mission-level emission site — no `seq`, no per-event stamping.
//!
//! Covers:
//!   (a) a top-level mission's `workflow.started`: `parent_workflow_id: None`,
//!       `depth: 0`; a child mission's (started with the depth/parent linkage
//!       the WorkflowExecutor stamps at spawn): `parent_workflow_id =
//!       Some(parent)`, `depth = 1`;
//!   (b) serde back-compat: an audit line written BEFORE the fields existed
//!       still deserializes (parent `None`, depth 0).

use std::sync::Arc;

use async_trait::async_trait;
use praxec_core::WorkflowRuntime;
use praxec_core::audit::{AuditEvent, AuditSink, MemoryAuditSink};
use praxec_core::guards::DefaultGuardEvaluator;
use praxec_core::model::{ParentLink, Principal, StartWorkflow, SubmitTransition};
use praxec_core::ports::{Executor, ExecutorRegistry};
use praxec_core::store::{ConfigDefinitionStore, InMemoryWorkflowStore};
use serde_json::json;

struct NoopExecutor;

#[async_trait]
impl Executor for NoopExecutor {
    async fn execute(
        &self,
        _: praxec_core::model::ExecuteRequest,
    ) -> Result<praxec_core::model::ExecuteResult, praxec_core::error::ExecutorError> {
        Ok(praxec_core::model::ExecuteResult::default())
    }
}

struct AnyKindRegistry(Arc<dyn Executor>);
impl ExecutorRegistry for AnyKindRegistry {
    fn get(&self, _kind: &str) -> Option<Arc<dyn Executor>> {
        Some(self.0.clone())
    }
}

fn config() -> serde_json::Value {
    json!({
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
    })
}

fn build_runtime() -> (WorkflowRuntime, Arc<MemoryAuditSink>) {
    let definitions = Arc::new(ConfigDefinitionStore::from_config(&config()));
    let store = Arc::new(InMemoryWorkflowStore::new());
    let executors = Arc::new(AnyKindRegistry(Arc::new(NoopExecutor)));
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

async fn start_one(runtime: &WorkflowRuntime, depth: u32, parent: Option<ParentLink>) -> String {
    let start = runtime
        .start(StartWorkflow {
            definition_id: "wf".into(),
            input: json!({}),
            principal: Principal::anonymous(),
            trace_id: None,
            run_id: None,
            depth,
            parent,
        })
        .await
        .unwrap();
    let workflow_id = start["workflow"]["id"].as_str().unwrap().to_string();
    let version = start["workflow"]["version"].as_u64().unwrap();
    runtime
        .submit(SubmitTransition {
            workflow_id: workflow_id.clone(),
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
    workflow_id
}

fn started_event<'a>(events: &'a [AuditEvent], workflow_id: &str) -> &'a AuditEvent {
    events
        .iter()
        .find(|e| {
            e.event_type == "workflow.started" && e.workflow_id.as_deref() == Some(workflow_id)
        })
        .expect("the run must emit a workflow.started for this workflow")
}

/// (a) — a top-level mission's `workflow.started` carries no parent edge and
/// depth 0.
#[tokio::test]
async fn top_level_started_is_a_root_node() {
    let (runtime, audit) = build_runtime();
    let workflow_id = start_one(&runtime, 0, None).await;

    let events = audit.snapshot();
    let started = started_event(&events, &workflow_id);
    assert_eq!(
        started.parent_workflow_id, None,
        "top-level workflow.started must have no parent edge"
    );
    assert_eq!(started.depth, 0, "top-level workflow.started is at depth 0");
}

/// (a) — a mission started with the parent linkage the WorkflowExecutor stamps
/// at spawn (depth = parent+1, `ParentLink` to the parent) emits
/// `workflow.started` with `parent_workflow_id = Some(parent)`, `depth = 1`.
#[tokio::test]
async fn child_started_carries_the_parent_edge() {
    let (runtime, audit) = build_runtime();
    let parent_id = start_one(&runtime, 0, None).await;
    let child_id = start_one(
        &runtime,
        1,
        Some(ParentLink {
            workflow_id: parent_id.clone(),
            transition: "go".into(),
        }),
    )
    .await;

    let events = audit.snapshot();
    let started = started_event(&events, &child_id);
    assert_eq!(
        started.parent_workflow_id.as_deref(),
        Some(parent_id.as_str()),
        "child workflow.started must carry the edge to its parent"
    );
    assert_eq!(started.depth, 1, "child workflow.started is at depth 1");
}

/// (b) — additive/non-breaking: an audit line persisted BEFORE the tree-linkage
/// fields existed still deserializes, defaulting to root coordinates.
#[test]
fn pre_linkage_audit_lines_still_deserialize() {
    let old_line = r#"{
        "id": "evt_old",
        "timestamp": "2026-01-01T00:00:00Z",
        "workflow_id": "wf_old",
        "correlation_id": "cor_old",
        "event_type": "workflow.started",
        "payload": {}
    }"#;
    let e: AuditEvent = serde_json::from_str(old_line).expect("old trail lines must load");
    assert_eq!(e.parent_workflow_id, None);
    assert_eq!(e.depth, 0);

    // And a freshly-built event round-trips through JSON with the fields intact.
    let fresh = AuditEvent::new("workflow.started")
        .with_workflow("wf_child")
        .with_topology(Some("wf_parent".into()), 1);
    let round: AuditEvent = serde_json::from_str(&serde_json::to_string(&fresh).unwrap()).unwrap();
    assert_eq!(round.parent_workflow_id.as_deref(), Some("wf_parent"));
    assert_eq!(round.depth, 1);
}
