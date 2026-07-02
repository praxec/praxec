//! G5 (testing-strategy) — ADR-0008 **outcome evaluation aggregation**, end to
//! end through the real runtime. The per-`expr` truth is G2's job and the
//! met→succeeded derivation is G1's; this pins the parts only the runtime's
//! `evaluate_outcomes` owns:
//!
//!   - the **all-met fold** over more than one outcome (partial satisfaction),
//!   - the surfaced `[{id, statement, met}]` shape tracking the live context,
//!   - an **unset slot / eval error reads as `met: false`** (graceful), not an
//!     errored response.

use std::sync::Arc;

use praxec_core::audit::{AuditSink, MemoryAuditSink};
use praxec_core::error::ExecutorError;
use praxec_core::guards::DefaultGuardEvaluator;
use praxec_core::model::{
    ExecuteRequest, ExecuteResult, Principal, StartWorkflow, SubmitTransition,
};
use praxec_core::ports::{Executor, ExecutorRegistry, WorkflowStore};
use praxec_core::runtime::WorkflowRuntime;
use praxec_core::store::{ConfigDefinitionStore, InMemoryEvidenceStore, InMemoryWorkflowStore};
use serde_json::{json, Value};

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

/// A two-outcome workflow (`a`, `b`) with transitions that set each flag, plus a
/// third outcome `c` that reads an **unset** slot — so one run exercises the fold
/// and the graceful unset path together.
fn runtime() -> WorkflowRuntime {
    let config = json!({
        "workflows": { "multi": {
            "initialState": "work",
            "outcomes": [
                { "id": "a", "statement": "a is set", "check": "$.context.a == true" },
                { "id": "b", "statement": "b is set", "check": "$.context.b == true" },
                { "id": "c", "statement": "unset slot", "check": "$.context.never == true" }
            ],
            "blackboard": { "a": { "type": "boolean" }, "b": { "type": "boolean" } },
            "states": {
                "work": {
                    "goal": "Set the flags.",
                    "transitions": {
                        "set_a": { "target": "work", "executor": { "kind": "noop" }, "output": { "a": true } },
                        "set_b": { "target": "work", "executor": { "kind": "noop" }, "output": { "b": true } }
                    }
                }
            }
        }}
    });
    let evidence = Arc::new(InMemoryEvidenceStore::new());
    WorkflowRuntime::new(
        Arc::new(ConfigDefinitionStore::from_config(&config)),
        Arc::new(InMemoryWorkflowStore::new()) as Arc<dyn WorkflowStore>,
        Arc::new(NoopReg),
        Arc::new(DefaultGuardEvaluator::with_evidence(evidence.clone())),
        Arc::new(MemoryAuditSink::new()) as Arc<dyn AuditSink>,
    )
    .with_evidence(evidence)
}

async fn start(rt: &WorkflowRuntime) -> Value {
    rt.start(StartWorkflow {
        definition_id: "multi".into(),
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

/// Find the `met` flag for outcome `id` in a response's `outcomes` surface.
fn met(response: &Value, id: &str) -> Option<bool> {
    response["outcomes"]
        .as_array()?
        .iter()
        .find(|o| o["id"] == id)
        .and_then(|o| o["met"].as_bool())
}

#[tokio::test]
async fn an_unsatisfied_outcome_reads_not_met_at_start() {
    let rt = runtime();
    let r = start(&rt).await;
    assert_eq!(met(&r, "a"), Some(false));
}

#[tokio::test]
async fn an_unset_slot_check_reads_not_met_rather_than_erroring() {
    // Outcome `c` reads `$.context.never`, never set — it must read false, and
    // the response itself must still be well-formed (not an error).
    let rt = runtime();
    let r = start(&rt).await;
    assert_eq!(met(&r, "c"), Some(false));
}

#[tokio::test]
async fn setting_one_flag_meets_only_that_outcome() {
    let rt = runtime();
    let s = start(&rt).await;
    let id = s["workflow"]["id"].as_str().expect("id").to_string();
    let v = s["workflow"]["version"].as_u64().expect("version");
    let r = submit(&rt, &id, v, "set_a").await;
    assert_eq!(met(&r, "a"), Some(true));
}

#[tokio::test]
async fn setting_one_flag_leaves_the_other_unmet() {
    let rt = runtime();
    let s = start(&rt).await;
    let id = s["workflow"]["id"].as_str().expect("id").to_string();
    let v = s["workflow"]["version"].as_u64().expect("version");
    let r = submit(&rt, &id, v, "set_a").await;
    assert_eq!(met(&r, "b"), Some(false));
}

#[tokio::test]
async fn meeting_both_flags_meets_both_outcomes() {
    let rt = runtime();
    let s = start(&rt).await;
    let id = s["workflow"]["id"].as_str().expect("id").to_string();
    let v0 = s["workflow"]["version"].as_u64().expect("version");
    let r1 = submit(&rt, &id, v0, "set_a").await;
    let v1 = r1["workflow"]["version"].as_u64().expect("version");
    let r2 = submit(&rt, &id, v1, "set_b").await;
    assert_eq!((met(&r2, "a"), met(&r2, "b")), (Some(true), Some(true)));
}
