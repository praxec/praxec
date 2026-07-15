//! SPEC §7.3 — record-first audit ordering snapshot.
//!
//! Captures the exact sequence of audit `event_type`s produced by a fixed
//! `submit` call, and asserts byte-equality against a hand-pinned golden.
//! The golden encodes the §7.3 invariant: `workflow.transition` (the
//! transition record) MUST be emitted BEFORE the snapshot is committed,
//! which means before `workflow.transitioned`.
//!
//! This test is the safety net for the D3 mechanical refactor that hoists
//! the `submit()` body into a private `dispatch_once()` helper. The test
//! must pass against the pre-refactor code AND post-refactor code; a
//! change in audit ordering surfaces as a snapshot mismatch.
//!
//! Determinism: only event-type strings are compared. Timestamps,
//! IDs, and correlation ids are deliberately ignored — they are not
//! part of the §7.3 invariant.

use std::sync::Arc;

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
use serde_json::{Value, json};

struct OkNoopExecutor;

#[async_trait]
impl Executor for OkNoopExecutor {
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

fn fixture_config() -> Value {
    // A → B → terminal C.
    // - "advance" (A→B) is an agent transition with one trivially-passing guard
    //   so guard.evaluated fires once. Executor is noop.
    // - "finalize" (B→C) is a deterministic transition; included so the chain
    //   layer fires after submit and we observe chain.* + workflow.transition
    //   for the chained step.
    json!({
        "version": "1.0.0",
        "workflows": {
            "pipeline": {
                "initialState": "a",
                "states": {
                    "a": {
                        "transitions": {
                            "advance": {
                                "title": "Advance",
                                "target": "b",
                                "actor": "agent",
                                "executor": { "kind": "noop" },
                                "guards": [
                                    { "kind": "always_true" }
                                ]
                            }
                        }
                    },
                    "b": {
                        "transitions": {
                            "finalize": {
                                "title": "Finalize",
                                "target": "c",
                                "actor": "deterministic",
                                "executor": { "kind": "noop" }
                            }
                        }
                    },
                    "c": { "terminal": true }
                }
            }
        }
    })
}

/// A guard evaluator whose `always_true` guard kind passes unconditionally.
/// Avoids dependency on the default evaluator's specific guard implementations.
struct AlwaysTrueGuards;

#[async_trait]
impl praxec_core::ports::GuardEvaluator for AlwaysTrueGuards {
    async fn evaluate(
        &self,
        _guard: &Value,
        _instance: &praxec_core::model::WorkflowInstance,
        _arguments: &Value,
        _principal: &Principal,
    ) -> anyhow::Result<bool> {
        Ok(true)
    }
}

fn build_runtime() -> (WorkflowRuntime, Arc<MemoryAuditSink>) {
    let cfg = fixture_config();
    let definitions = Arc::new(ConfigDefinitionStore::from_config(&cfg));
    let store = Arc::new(InMemoryWorkflowStore::new());
    let executor = Arc::new(OkNoopExecutor);
    let executors = Arc::new(SingleExecRegistry {
        inner: executor as Arc<dyn Executor>,
    });
    // Use AlwaysTrueGuards so the fixture's guard kind passes regardless of the
    // default evaluator's registered guard kinds. Keeps the test focused on
    // audit ordering, not guard implementation details.
    let _default_guards = DefaultGuardEvaluator::new();
    let guards = Arc::new(AlwaysTrueGuards);
    let audit = Arc::new(MemoryAuditSink::new());
    let runtime = WorkflowRuntime::new(
        definitions,
        store,
        executors,
        guards,
        audit.clone() as Arc<dyn AuditSink>,
    )
    .with_writable_repo_roots(vec![praxec_core::RepoRoot::for_test()]);
    (runtime, audit)
}

#[tokio::test]
async fn submit_emits_audit_events_in_record_first_order() {
    let (runtime, audit) = build_runtime();

    let started = runtime
        .start(StartWorkflow {
            definition_id: "pipeline".into(),
            input: json!({}),
            principal: Principal::anonymous(),
            run_env: praxec_core::RunEnv::for_test(),
            depth: 0,
            parent: None,
        })
        .await
        .unwrap();

    let wf_id = started["workflow"]["id"].as_str().unwrap().to_string();
    let version = started["workflow"]["version"].as_u64().unwrap();

    // Clear start-phase events so the snapshot is only the submit pipeline.
    audit.clear();

    runtime
        .submit(SubmitTransition {
            workflow_id: wf_id,
            expected_version: version,
            transition: "advance".into(),
            arguments: json!({}),
            principal: Principal::anonymous(),
            summary: None,
            trace_id: None,
            run_id: None,
        })
        .await
        .unwrap();

    let types = audit.event_types();

    // Golden — pins the §7.3 record-first invariant and chain ordering:
    //
    // 1. `transition.requested` opens the pipeline.
    // 2. `guard.evaluated` fires once per guard (one guard in fixture).
    // 3. `executor.started` + `executor.succeeded` bracket the executor
    //    call (reliability layer instrumentation).
    // 4. `workflow.transition` — the RECORD — fires BEFORE the snapshot
    //    is committed. This is the §7.3 invariant.
    // 5. `workflow.transitioned` — the state-change announcement —
    //    fires AFTER the snapshot is committed.
    // 6. Deterministic chain then fires `chain.step`, brackets its own
    //    executor with `executor.started` + `executor.succeeded`,
    //    emits `workflow.transition` + `workflow.transitioned` for the
    //    chained step.
    // 7. `chain.completed` closes the chain.
    // 8. `workflow.completed` fires because the chained step landed in
    //    a terminal state.
    // 9. `outcome.recorded` (intent index) fires once beside it, at the
    //    terminal, on the non-critical audit path — after the terminal is
    //    announced, never before the §7.3 record.
    let expected = vec![
        "transition.requested",
        "guard.evaluated",
        "executor.started",
        "executor.succeeded",
        "workflow.transition",
        "workflow.transitioned",
        "chain.step",
        "executor.started",
        "executor.succeeded",
        "workflow.transition",
        "workflow.transitioned",
        "chain.completed",
        "workflow.completed",
        "outcome.recorded",
    ];
    let actual: Vec<&str> = types.iter().map(String::as_str).collect();
    assert_eq!(
        actual, expected,
        "audit event sequence drifted — SPEC §7.3 record-first ordering at risk.\n\
         Expected: {:?}\nActual:   {:?}",
        expected, actual
    );

    // Defense-in-depth: explicitly assert §7.3 by index.
    // workflow.transition (the record) MUST come before workflow.transitioned
    // (the post-commit announcement) within the SAME submit cycle.
    let record_idx = actual
        .iter()
        .position(|t| *t == "workflow.transition")
        .expect("record event missing");
    let transitioned_idx = actual
        .iter()
        .position(|t| *t == "workflow.transitioned")
        .expect("transitioned event missing");
    assert!(
        record_idx < transitioned_idx,
        "SPEC §7.3 violated: workflow.transition (record-first) must precede \
         workflow.transitioned (post-commit). record_idx={}, transitioned_idx={}",
        record_idx,
        transitioned_idx
    );
}
