//! SPEC §33 D3 — runtime-driven chain loop tests.
//!
//! Covers the `submit()` chain loop on top of `dispatch_once`:
//!
//! 1. A two-turn chain (executor yields `NextTransition` once, then
//!    `None`) fires two complete audit sequences in order, with two
//!    `workflow.transitioned` events.
//! 2. An always-chaining executor hits `max_chained_llm_turns` and
//!    surfaces `LLM_CHAIN_DEPTH_EXCEEDED`.

use std::sync::{
    Arc,
    atomic::{AtomicUsize, Ordering},
};

use async_trait::async_trait;
use praxec_core::WorkflowRuntime;
use praxec_core::audit::{AuditSink, MemoryAuditSink};
use praxec_core::error::{ErrorClass, ExecutorError};
use praxec_core::guards::DefaultGuardEvaluator;
use praxec_core::model::{
    ExecuteRequest, ExecuteResult, NextTransition, Principal, StartWorkflow, SubmitTransition,
};
use praxec_core::ports::{Executor, ExecutorRegistry};
use praxec_core::store::{ConfigDefinitionStore, InMemoryWorkflowStore};
use serde_json::{Value, json};

// ---- shared executors ------------------------------------------------------

/// Yields `next_transition: Some(next)` for the first N calls, then `None`.
struct ChainingExecutor {
    next: String,
    chain_for: usize,
    calls: AtomicUsize,
}

impl ChainingExecutor {
    fn new(next: impl Into<String>, chain_for: usize) -> Self {
        Self {
            next: next.into(),
            chain_for,
            calls: AtomicUsize::new(0),
        }
    }
    fn count(&self) -> usize {
        self.calls.load(Ordering::SeqCst)
    }
}

#[async_trait]
impl Executor for ChainingExecutor {
    async fn execute(&self, _: ExecuteRequest) -> Result<ExecuteResult, ExecutorError> {
        let n = self.calls.fetch_add(1, Ordering::SeqCst);
        let next_transition = if n < self.chain_for {
            Some(NextTransition {
                transition: self.next.clone(),
                arguments: json!({}),
                summary: Some(format!("turn {n} → {}", self.next)),
            })
        } else {
            None
        };
        Ok(ExecuteResult {
            output: json!({}),
            evidence: vec![],
            child_workflow_id: None,
            next_transition,
            suspend: None,
            telemetry: None,
        })
    }
}

/// Executor that ALWAYS yields a next_transition pointing at the
/// passed-in self-loop transition name. Used for the depth-cap test.
struct AlwaysChainsExecutor {
    next: String,
    calls: AtomicUsize,
}

impl AlwaysChainsExecutor {
    fn new(next: impl Into<String>) -> Self {
        Self {
            next: next.into(),
            calls: AtomicUsize::new(0),
        }
    }
    fn count(&self) -> usize {
        self.calls.load(Ordering::SeqCst)
    }
}

#[async_trait]
impl Executor for AlwaysChainsExecutor {
    async fn execute(&self, _: ExecuteRequest) -> Result<ExecuteResult, ExecutorError> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        Ok(ExecuteResult {
            output: json!({}),
            evidence: vec![],
            child_workflow_id: None,
            next_transition: Some(NextTransition {
                transition: self.next.clone(),
                arguments: json!({}),
                summary: None,
            }),
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

fn build_runtime_with(
    config: Value,
    executor: Arc<dyn Executor>,
) -> (WorkflowRuntime, Arc<MemoryAuditSink>) {
    let definitions = Arc::new(ConfigDefinitionStore::from_config(&config));
    let store = Arc::new(InMemoryWorkflowStore::new());
    let executors = Arc::new(SingleExecRegistry { inner: executor });
    let guards = Arc::new(DefaultGuardEvaluator::new());
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

// ---- two-turn chain --------------------------------------------------------

fn two_turn_config() -> Value {
    // A → B → C terminal.
    // - "begin" (A→B) is the first executor turn — yields Some(advance).
    // - "advance" (B→C) is the second turn — yields None, terminating.
    json!({
        "version": "1.0.0",
        "workflows": {
            "pipeline": {
                "initialState": "a",
                "states": {
                    "a": {
                        "transitions": {
                            "begin": {
                                "target": "b",
                                "actor": "agent",
                                "executor": { "kind": "noop" }
                            }
                        }
                    },
                    "b": {
                        "transitions": {
                            "advance": {
                                "target": "c",
                                "actor": "agent",
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

#[tokio::test]
async fn chain_loop_two_turns_emits_two_transitioned_events_in_order() {
    let exec = Arc::new(ChainingExecutor::new("advance", 1));
    let (runtime, audit) = build_runtime_with(two_turn_config(), exec.clone() as Arc<dyn Executor>);

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

    audit.clear();

    let resp = runtime
        .submit(SubmitTransition {
            workflow_id: wf_id,
            expected_version: version,
            transition: "begin".into(),
            arguments: json!({}),
            principal: Principal::anonymous(),
            summary: None,
            trace_id: None,
            run_id: None,
        })
        .await
        .unwrap();

    // Two executor invocations: one per chained turn.
    assert_eq!(exec.count(), 2, "chain should call executor twice");

    // Workflow landed in terminal C.
    assert_eq!(resp["workflow"]["state"], "c");

    // Audit assertions:
    // - exactly two `workflow.transitioned` events fire across the chain.
    // - exactly two `workflow.transition` records fire (record-first per turn).
    // - the record for each turn precedes that turn's `workflow.transitioned`.
    let snapshot = audit.snapshot();
    let types: Vec<&str> = snapshot.iter().map(|e| e.event_type.as_str()).collect();

    let transitioned_count = types
        .iter()
        .filter(|t| **t == "workflow.transitioned")
        .count();
    let record_count = types
        .iter()
        .filter(|t| **t == "workflow.transition")
        .count();
    assert_eq!(
        transitioned_count, 2,
        "chain produces N=2 workflow.transitioned events. Got types: {types:?}",
    );
    assert_eq!(
        record_count, 2,
        "chain produces N=2 workflow.transition records. Got types: {types:?}",
    );

    // Per-turn record-first ordering: each `workflow.transition` must
    // precede the next `workflow.transitioned` in document order.
    let mut record_idx = Vec::new();
    let mut transitioned_idx = Vec::new();
    for (i, t) in types.iter().enumerate() {
        match *t {
            "workflow.transition" => record_idx.push(i),
            "workflow.transitioned" => transitioned_idx.push(i),
            _ => {}
        }
    }
    assert_eq!(record_idx.len(), 2);
    assert_eq!(transitioned_idx.len(), 2);
    for (turn, (rec, trn)) in record_idx.iter().zip(transitioned_idx.iter()).enumerate() {
        assert!(
            *rec < *trn,
            "turn {turn}: workflow.transition (idx {rec}) must precede \
             workflow.transitioned (idx {trn}). full types: {types:?}",
        );
    }

    // Verify the two `transition.requested` events name the two
    // transitions in chain order (begin then advance).
    let requested_transitions: Vec<String> = snapshot
        .iter()
        .filter(|e| e.event_type == "transition.requested")
        .filter_map(|e| {
            e.payload
                .get("transition")
                .and_then(Value::as_str)
                .map(String::from)
        })
        .collect();
    assert_eq!(
        requested_transitions,
        vec!["begin".to_string(), "advance".to_string()],
        "transition.requested events must fire in chain order"
    );
}

// ---- depth-exceeded -------------------------------------------------------

fn self_loop_config() -> Value {
    // A → A via "loop" transition. Always-chains executor keeps the
    // workflow at A; only the chain cap stops the loop.
    json!({
        "version": "1.0.0",
        "workflows": {
            "pipeline": {
                "initialState": "a",
                "states": {
                    "a": {
                        "transitions": {
                            "loop": {
                                "target": "a",
                                "actor": "agent",
                                "executor": { "kind": "noop" }
                            }
                        }
                    }
                }
            }
        }
    })
}

#[tokio::test]
async fn chain_loop_caps_at_max_chained_llm_turns() {
    let exec = Arc::new(AlwaysChainsExecutor::new("loop"));
    let (runtime, audit) =
        build_runtime_with(self_loop_config(), exec.clone() as Arc<dyn Executor>);

    // Set the cap LOW so the test runs fast.
    let cap: u32 = 3;
    let runtime = runtime.with_max_chained_llm_turns(cap);

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

    audit.clear();

    let err = runtime
        .submit(SubmitTransition {
            workflow_id: wf_id,
            expected_version: version,
            transition: "loop".into(),
            arguments: json!({}),
            principal: Principal::anonymous(),
            summary: None,
            trace_id: None,
            run_id: None,
        })
        .await
        .expect_err("chain depth cap should surface an error");

    // Typed: LLM_CHAIN_DEPTH_EXCEEDED via ExecutorError::Llm.
    let exec_err = err
        .downcast_ref::<ExecutorError>()
        .expect("submit should propagate ExecutorError on chain cap breach");
    match exec_err {
        ExecutorError::Llm(code, _msg) => {
            assert_eq!(code.as_wire_code(), "LLM_CHAIN_DEPTH_EXCEEDED");
            assert_eq!(code.class(), ErrorClass::Permanent);
        }
        other => panic!("expected ExecutorError::Llm(ChainDepthExceeded, _); got {other:?}"),
    }

    // `max_chained_llm_turns` bounds the number of CHAINED continuations
    // beyond the initial submit. So with cap = N, we expect 1 initial
    // dispatch_once + N chained dispatch_once = N+1 executor calls,
    // and the (N+2)th continuation is refused by the chain loop.
    let expected_calls = cap as usize + 1;
    assert_eq!(
        exec.count(),
        expected_calls,
        "executor should be called exactly cap+1 times: 1 initial submit \
         cycle plus `max_chained_llm_turns` chained continuations. \
         cap = {cap}"
    );

    // The audit log must include a `transition.rejected` event whose
    // `code` is `LLM_CHAIN_DEPTH_EXCEEDED`.
    let snapshot = audit.snapshot();
    let cap_event = snapshot
        .iter()
        .find(|e| {
            e.event_type == "transition.rejected"
                && e.payload.get("code").and_then(Value::as_str) == Some("LLM_CHAIN_DEPTH_EXCEEDED")
        })
        .unwrap_or_else(|| {
            panic!(
                "expected a transition.rejected audit event carrying \
                 LLM_CHAIN_DEPTH_EXCEEDED. Saw: {:?}",
                snapshot
                    .iter()
                    .map(|e| e.event_type.as_str())
                    .collect::<Vec<_>>()
            )
        });
    // The cap audit event must name the transition that would have run.
    assert_eq!(
        cap_event.payload.get("transition").and_then(Value::as_str),
        Some("loop")
    );
}

// ---- CMP-011: next_transition unsupported off the submit loop --------------

/// onEnter executor on the initial state returns a `next_transition`. The
/// onEnter path has no interactive submit loop to consume it, so `start()`
/// must FAIL-FAST with NEXT_TRANSITION_UNSUPPORTED rather than dropping it.
#[tokio::test]
async fn on_enter_next_transition_fails_fast() {
    let exec = Arc::new(AlwaysChainsExecutor::new("noop_next"));
    let config = json!({
        "version": "1.0.0",
        "workflows": {
            "pipeline": {
                "initialState": "a",
                "states": {
                    "a": {
                        "onEnter": { "executor": { "kind": "noop" } },
                        "transitions": {}
                    }
                }
            }
        }
    });
    let (runtime, _audit) = build_runtime_with(config, exec as Arc<dyn Executor>);

    let err = runtime
        .start(StartWorkflow {
            definition_id: "pipeline".into(),
            input: json!({}),
            principal: Principal::anonymous(),
            run_env: praxec_core::RunEnv::for_test(),
            depth: 0,
            parent: None,
        })
        .await
        .expect_err("onEnter returning next_transition must fail-fast");
    assert!(
        err.to_string().contains("NEXT_TRANSITION_UNSUPPORTED"),
        "expected NEXT_TRANSITION_UNSUPPORTED, got: {err}"
    );
}

/// A `deterministic` transition's executor returns a `next_transition`.
/// The deterministic chain is driven by transition `target`s, not by
/// `next_transition`, so `start()` (which runs the deterministic chain from
/// the initial state) must FAIL-FAST with NEXT_TRANSITION_UNSUPPORTED.
#[tokio::test]
async fn deterministic_next_transition_fails_fast() {
    let exec = Arc::new(AlwaysChainsExecutor::new("noop_next"));
    let config = json!({
        "version": "1.0.0",
        "workflows": {
            "pipeline": {
                "initialState": "a",
                "states": {
                    "a": {
                        "transitions": {
                            "advance": {
                                "target": "b",
                                "actor": "deterministic",
                                "executor": { "kind": "noop" }
                            }
                        }
                    },
                    "b": { "terminal": true }
                }
            }
        }
    });
    let (runtime, _audit) = build_runtime_with(config, exec as Arc<dyn Executor>);

    let err = runtime
        .start(StartWorkflow {
            definition_id: "pipeline".into(),
            input: json!({}),
            principal: Principal::anonymous(),
            run_env: praxec_core::RunEnv::for_test(),
            depth: 0,
            parent: None,
        })
        .await
        .expect_err("deterministic executor returning next_transition must fail-fast");
    assert!(
        err.to_string().contains("NEXT_TRANSITION_UNSUPPORTED"),
        "expected NEXT_TRANSITION_UNSUPPORTED, got: {err}"
    );
}

// ---- input → context seeding (poka-yoke) -----------------------------------

// A flow with an inputSchema DEFAULT and NO manual "seeding state". The slot
// table already treats every declared input as a reachable `$.context` slot
// (SlotSource::Input), so a read of `$.context.<input>` passes V13 — but the
// runtime previously seeded `$.context` only from `initialContext`, so the
// value was null at runtime. The fix seeds resolved inputs (defaults applied)
// into the initial context for keys not already in initialContext.
fn input_default_config() -> Value {
    json!({
        "version": "1.0.0",
        "workflows": {
            "pipeline": {
                "initialState": "done",
                "inputSchema": {
                    "type": "object",
                    "properties": { "matrix_strategy": { "type": "string", "default": "q3x3" } }
                },
                "initialContext": { "seeded": 1 },
                "states": { "done": { "terminal": true } }
            }
        }
    })
}

#[tokio::test]
async fn input_defaults_seed_into_context_at_start() {
    let exec = Arc::new(ChainingExecutor::new("noop", 1)); // never called (terminal initial)
    let (runtime, _audit) = build_runtime_with(input_default_config(), exec as Arc<dyn Executor>);

    let started = runtime
        .start(StartWorkflow {
            definition_id: "pipeline".into(),
            input: json!({}), // no matrix_strategy supplied → schema default applies
            principal: Principal::anonymous(),
            run_env: praxec_core::RunEnv::for_test(),
            depth: 0,
            parent: None,
        })
        .await
        .unwrap();

    // The input default is present in $.context (the poka-yoke), AND the
    // explicit initialContext value is preserved (initialContext wins on
    // collision; here there is none).
    assert_eq!(
        started["context"]["matrix_strategy"].as_str(),
        Some("q3x3"),
        "declared input default must be seeded into $.context: {started}"
    );
    assert_eq!(started["context"]["seeded"].as_i64(), Some(1));
}
