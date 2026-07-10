//! Phase 1 (intent index) — the `outcome.recorded` terminal audit event, end to
//! end through the real runtime. Pins the parts only the runtime owns: that the
//! event fires **exactly once** at each terminal-reaching site (start-chain and
//! submit), carries the deterministic outcome done-signal + the resolved
//! terminal status, and identifies the workflow for the cost join.

use std::sync::Arc;

use praxec_core::audit::{AuditEvent, AuditSink, MemoryAuditSink};
use praxec_core::error::ExecutorError;
use praxec_core::guards::DefaultGuardEvaluator;
use praxec_core::intent_index::OUTCOME_RECORDED;
use praxec_core::model::{
    ExecuteRequest, ExecuteResult, Principal, StartWorkflow, SubmitTransition,
};
use praxec_core::ports::{Executor, ExecutorRegistry, WorkflowStore};
use praxec_core::runtime::WorkflowRuntime;
use praxec_core::store::{ConfigDefinitionStore, InMemoryEvidenceStore, InMemoryWorkflowStore};
use serde_json::{Value, json};

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

fn runtime_with_sink() -> (WorkflowRuntime, Arc<MemoryAuditSink>) {
    let config = json!({
        "workflows": {
            // Submit-driven: `work` → `ship` sets shipped, lands on a success
            // terminal; `break` lands on a failure terminal.
            "ship": {
                "initialState": "work",
                "outcomes": [
                    { "id": "done", "statement": "shipped", "check": "$.context.shipped == true" }
                ],
                "blackboard": { "shipped": { "type": "boolean" } },
                "states": {
                    "work": { "transitions": {
                        "ship":  { "target": "done",   "executor": { "kind": "noop" }, "output": { "shipped": true } },
                        "break": { "target": "broken", "executor": { "kind": "noop" } }
                    }},
                    "done":   { "terminal": true, "outcome": "success" },
                    "broken": { "terminal": true, "outcome": "failure" }
                }
            },
            // Start-driven: a deterministic transition auto-chains to a success
            // terminal during `start`, exercising the start-terminal emit site.
            "auto": {
                "initialState": "s0",
                "outcomes": [
                    { "id": "ok", "statement": "x set", "check": "$.context.x == true" }
                ],
                "blackboard": { "x": { "type": "boolean" } },
                "states": {
                    "s0": { "transitions": {
                        "go": { "target": "end", "actor": "deterministic", "executor": { "kind": "noop" }, "output": { "x": true } }
                    }},
                    "end": { "terminal": true, "outcome": "success" }
                }
            }
        }
    });
    let evidence = Arc::new(InMemoryEvidenceStore::new());
    let sink = Arc::new(MemoryAuditSink::new());
    let rt = WorkflowRuntime::new(
        Arc::new(ConfigDefinitionStore::from_config(&config)),
        Arc::new(InMemoryWorkflowStore::new()) as Arc<dyn WorkflowStore>,
        Arc::new(NoopReg),
        Arc::new(DefaultGuardEvaluator::with_evidence(evidence.clone())),
        sink.clone() as Arc<dyn AuditSink>,
    )
    .with_evidence(evidence);
    (rt, sink)
}

async fn start(rt: &WorkflowRuntime, def: &str) -> Value {
    rt.start(StartWorkflow {
        definition_id: def.into(),
        input: json!({}),
        principal: Principal::anonymous(),
        trace_id: None,
        run_id: None,
        depth: 0,
        parent: None,
    })
    .await
    .expect("start succeeds")
}

async fn submit(rt: &WorkflowRuntime, id: &str, version: u64, transition: &str) -> Value {
    rt.submit(SubmitTransition {
        workflow_id: id.into(),
        expected_version: version,
        transition: transition.into(),
        arguments: json!({}),
        principal: Principal::anonymous(),
        summary: None,
        trace_id: None,
        run_id: None,
    })
    .await
    .expect("submit succeeds")
}

async fn outcome_events(sink: &Arc<MemoryAuditSink>) -> Vec<AuditEvent> {
    sink.try_list_events()
        .await
        .expect("listable")
        .expect("memory sink stores events")
        .into_iter()
        .filter(|e| e.event_type == OUTCOME_RECORDED)
        .collect()
}

#[tokio::test]
async fn submit_to_a_success_terminal_emits_one_outcome_recorded() {
    let (rt, sink) = runtime_with_sink();
    let s = start(&rt, "ship").await;
    let id = s["workflow"]["id"].as_str().expect("id").to_string();
    let v = s["workflow"]["version"].as_u64().expect("version");
    // Not terminal yet — no outcome event from `start`.
    assert_eq!(outcome_events(&sink).await.len(), 0);

    submit(&rt, &id, v, "ship").await;

    let evts = outcome_events(&sink).await;
    assert_eq!(
        evts.len(),
        1,
        "exactly one outcome.recorded at the terminal"
    );
    let e = &evts[0];
    assert_eq!(
        e.workflow_id.as_deref(),
        Some(id.as_str()),
        "carries workflow_id for the cost join"
    );
    assert_eq!(e.payload["template_id"], "ship");
    assert_eq!(e.payload["outcomes_met"], true);
    assert_eq!(e.payload["outcomes_total"], 1);
    assert_eq!(e.payload["terminal_status"], "succeeded");
    assert!(
        e.payload.get("task_class").is_none(),
        "no process tag until Phase 2"
    );
}

#[tokio::test]
async fn submit_to_a_failure_terminal_records_failed_with_reason() {
    let (rt, sink) = runtime_with_sink();
    let s = start(&rt, "ship").await;
    let id = s["workflow"]["id"].as_str().expect("id").to_string();
    let v = s["workflow"]["version"].as_u64().expect("version");

    submit(&rt, &id, v, "break").await;

    let evts = outcome_events(&sink).await;
    assert_eq!(evts.len(), 1);
    let e = &evts[0];
    assert_eq!(e.payload["terminal_status"], "failed");
    assert_eq!(e.payload["outcomes_met"], false);
    assert_eq!(e.payload["fail_reason"], "guard_unmet");
}

#[tokio::test]
async fn start_chaining_to_a_terminal_emits_exactly_one_outcome_recorded() {
    let (rt, sink) = runtime_with_sink();
    start(&rt, "auto").await;

    let evts = outcome_events(&sink).await;
    assert_eq!(
        evts.len(),
        1,
        "deterministic auto-chain to terminal emits once"
    );
    let e = &evts[0];
    assert_eq!(e.payload["template_id"], "auto");
    assert_eq!(e.payload["outcomes_met"], true);
    assert_eq!(e.payload["terminal_status"], "succeeded");
}
