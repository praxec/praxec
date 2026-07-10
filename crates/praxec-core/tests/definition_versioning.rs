//! Tests for the per-instance workflow definition snapshot (SPEC §8.2 / §8.3).
//!
//! A `WorkflowInstance` carries its own resolved workflow definition snapshot,
//! captured at `workflow.start`. Every in-flight operation resolves the
//! definition from the instance's carried snapshot, never from the live
//! `DefinitionStore`. Editing or hot-reloading config therefore never disturbs
//! a running instance.

use std::sync::Arc;
use std::sync::Mutex;

use async_trait::async_trait;
use praxec_core::WorkflowRuntime;
use praxec_core::audit::{AuditSink, MemoryAuditSink};
use praxec_core::error::ExecutorError;
use praxec_core::guards::DefaultGuardEvaluator;
use praxec_core::model::{
    ExecuteRequest, ExecuteResult, GetWorkflow, Principal, StartWorkflow, SubmitTransition,
};
use praxec_core::ports::{DefinitionStore, Executor, ExecutorRegistry, WorkflowStore};
use praxec_core::store::InMemoryWorkflowStore;
use serde_json::{Value, json};

// ---- test harness -----------------------------------------------------------

/// An executor that always succeeds with an empty output.
struct NoopExecutor;

#[async_trait]
impl Executor for NoopExecutor {
    async fn execute(&self, _: ExecuteRequest) -> Result<ExecuteResult, ExecutorError> {
        Ok(ExecuteResult {
            output: json!({}),
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

/// A `DefinitionStore` whose served definition can be swapped at runtime,
/// modelling a config edit / hot reload. Each `load` reflects the *current*
/// definition, so the runtime must capture a snapshot to be immune to swaps.
struct SwappableDefStore {
    definition_id: String,
    current: Mutex<Value>,
}

impl SwappableDefStore {
    fn new(definition_id: &str, initial: Value) -> Self {
        Self {
            definition_id: definition_id.to_string(),
            current: Mutex::new(initial),
        }
    }

    fn swap(&self, new_definition: Value) {
        *self.current.lock().unwrap() = new_definition;
    }
}

#[async_trait]
impl DefinitionStore for SwappableDefStore {
    async fn load(&self, definition_id: &str) -> anyhow::Result<Value> {
        if definition_id != self.definition_id {
            anyhow::bail!("workflow definition '{}' not found", definition_id);
        }
        Ok(self.current.lock().unwrap().clone())
    }
}

fn build_runtime(
    definitions: Arc<SwappableDefStore>,
) -> (
    WorkflowRuntime,
    Arc<InMemoryWorkflowStore>,
    Arc<MemoryAuditSink>,
) {
    let store = Arc::new(InMemoryWorkflowStore::new());
    let executors = Arc::new(SingleExecRegistry {
        inner: Arc::new(NoopExecutor),
    });
    let guards = Arc::new(DefaultGuardEvaluator::new());
    let audit = Arc::new(MemoryAuditSink::new());
    let runtime = WorkflowRuntime::new(
        definitions,
        store.clone(),
        executors,
        guards,
        audit.clone() as Arc<dyn AuditSink>,
    );
    (runtime, store, audit)
}

fn workflow_id(response: &Value) -> String {
    response
        .pointer("/workflow/id")
        .and_then(Value::as_str)
        .expect("response carries a workflow id")
        .to_string()
}

// ---- definitions -------------------------------------------------------------

/// A → (next, agent) → B → (finish, agent) → done.
/// No deterministic transitions, so `start` stops at A waiting for an agent.
fn original_definition() -> Value {
    json!({
        "version": "1.0.0",
        "initialState": "a",
        "states": {
            "a": {
                "transitions": {
                    "next": { "title": "Next", "target": "b", "actor": "agent" }
                }
            },
            "b": {
                "transitions": {
                    "finish": { "title": "Finish", "target": "done", "actor": "agent" }
                }
            },
            "done": { "terminal": true }
        }
    })
}

/// A drastically different definition: the initial state is renamed, state "b"
/// is gone, and the "next" transition no longer exists. If an in-flight
/// instance ever consulted this, `get`/`submit` of the original `next`
/// transition from state "a" would fail.
fn rewritten_definition() -> Value {
    json!({
        "version": "9.9.9",
        "initialState": "intake",
        "states": {
            "intake": {
                "transitions": {
                    "process": { "title": "Process", "target": "shipped", "actor": "agent" }
                }
            },
            "shipped": { "terminal": true }
        }
    })
}

// ---- tests -------------------------------------------------------------------

/// After `workflow.start`, the persisted `WorkflowInstance` carries the full
/// resolved workflow definition snapshot — not merely an id/version.
#[tokio::test]
async fn instance_carries_definition_snapshot() {
    let defs = Arc::new(SwappableDefStore::new("wf", original_definition()));
    let (runtime, store, _audit) = build_runtime(defs);

    let response = runtime
        .start(StartWorkflow {
            definition_id: "wf".to_string(),
            input: json!({}),
            principal: Principal::anonymous(),
            trace_id: None,
            run_id: None,
            depth: 0,
            parent: None,
        })
        .await
        .expect("workflow starts");

    let id = workflow_id(&response);
    let instance = store.load(&id).await.expect("instance is persisted");

    // The carried snapshot is the *full* resolved definition, not a stub.
    assert_eq!(
        instance.definition,
        original_definition(),
        "instance must carry the full resolved workflow definition"
    );
    // Concretely: states and transitions are present in the snapshot.
    assert!(
        instance
            .definition
            .pointer("/states/a/transitions/next")
            .is_some(),
        "snapshot must contain the workflow's states and transitions"
    );
    // definition_version is sourced from the snapshot's `version`.
    assert_eq!(instance.definition_version, "1.0.0");
}

/// Starting an instance, then swapping the definition the `DefinitionStore`
/// would serve, must not disturb the in-flight instance: `get` and `submit`
/// continue to resolve against the instance's ORIGINAL carried snapshot.
#[tokio::test]
async fn config_edit_does_not_disturb_inflight_instance() {
    let defs = Arc::new(SwappableDefStore::new("wf", original_definition()));
    let (runtime, _store, _audit) = build_runtime(defs.clone());

    let started = runtime
        .start(StartWorkflow {
            definition_id: "wf".to_string(),
            input: json!({}),
            principal: Principal::anonymous(),
            trace_id: None,
            run_id: None,
            depth: 0,
            parent: None,
        })
        .await
        .expect("workflow starts");
    let id = workflow_id(&started);
    assert_eq!(started.pointer("/workflow/state").unwrap(), "a");

    // Config edit / hot reload: the store now serves a completely different
    // definition with a renamed initial state and no "next" transition.
    defs.swap(rewritten_definition());

    // `get` still resolves the in-flight instance against its carried
    // definition: state "a" and its "next" link survive the swap.
    let got = runtime
        .get(GetWorkflow {
            workflow_id: id.clone(),
            principal: Principal::anonymous(),
            trace_id: None,
            run_id: None,
        })
        .await
        .expect("get resolves against the carried snapshot");
    assert_eq!(got.pointer("/workflow/state").unwrap(), "a");
    let links = got.pointer("/links").and_then(Value::as_array).unwrap();
    assert!(
        links
            .iter()
            .any(|l| l.pointer("/rel").and_then(Value::as_str) == Some("next")),
        "the original 'next' transition must still be offered after a config swap"
    );

    // `submit` of the original "next" transition still advances the instance,
    // proving it resolved against the carried snapshot, not the swapped store.
    let submitted = runtime
        .submit(SubmitTransition {
            workflow_id: id.clone(),
            expected_version: 0,
            transition: "next".to_string(),
            arguments: json!({}),
            principal: Principal::anonymous(),
            summary: None,
            trace_id: None,
            run_id: None,
        })
        .await
        .expect("submit resolves against the carried snapshot");
    assert_eq!(
        submitted.pointer("/workflow/state").unwrap(),
        "b",
        "instance advances using its original definition, unaffected by the swap"
    );

    // And the next original transition continues to work too.
    let finished = runtime
        .submit(SubmitTransition {
            workflow_id: id.clone(),
            expected_version: 1,
            transition: "finish".to_string(),
            arguments: json!({}),
            principal: Principal::anonymous(),
            summary: None,
            trace_id: None,
            run_id: None,
        })
        .await
        .expect("the carried definition drives the instance to completion");
    assert_eq!(finished.pointer("/workflow/state").unwrap(), "done");
    assert_eq!(
        finished.pointer("/result/status").unwrap(),
        "succeeded",
        "in-flight instance reaches its terminal state via the carried definition"
    );
}
