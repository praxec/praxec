//! Workflow failure-path coverage. The happy-path tests prove the runtime
//! works when everything succeeds; these tests prove it degrades gracefully
//! when something inside the walk goes wrong.
//!
//! Covers:
//! - Permanent executor error: state machine stays in the originating state
//!   (no auto-advance to target), result.status is NOT "succeeded".
//! - Guard rejection: a transition's guard evaluates false on `submit` →
//!   workflow stays in current state, no state advance, status is "rejected".
//! - Timeout: the runtime's timeout check is lazy (fires on next `get` or
//!   `submit` call, not a wall-clock goroutine). A real-time executor-sleep
//!   test would require a runtime-level async watchdog that doesn't exist yet.
//!   Stubbed as #[ignore] with a precise description of what's missing.
//! - Cancellation: no cancel/stop API on WorkflowRuntime today. Stubbed as
//!   #[ignore] so the gap is named explicitly rather than silently absent.

use std::sync::Arc;

use async_trait::async_trait;
use praxec_core::audit::{AuditSink, MemoryAuditSink};
use praxec_core::error::ExecutorError;
use praxec_core::guards::DefaultGuardEvaluator;
use praxec_core::model::{
    ExecuteRequest, ExecuteResult, Principal, StartWorkflow, SubmitTransition,
};
use praxec_core::ports::{Executor, ExecutorRegistry, WorkflowStore};
use praxec_core::runtime::WorkflowRuntime;
use praxec_core::store::{ConfigDefinitionStore, InMemoryEvidenceStore, InMemoryWorkflowStore};
use serde_json::{Value, json};

// ---------------------------------------------------------------------------
// Shared executor impls
// ---------------------------------------------------------------------------

struct PermanentFailExecutor {
    error_msg: String,
}

#[async_trait]
impl Executor for PermanentFailExecutor {
    async fn execute(&self, _r: ExecuteRequest) -> Result<ExecuteResult, ExecutorError> {
        Err(ExecutorError::Permanent(self.error_msg.clone()))
    }
}

struct FailingRegistry {
    error_msg: String,
}

impl ExecutorRegistry for FailingRegistry {
    fn get(&self, _kind: &str) -> Option<Arc<dyn Executor>> {
        Some(Arc::new(PermanentFailExecutor {
            error_msg: self.error_msg.clone(),
        }))
    }
}

struct NoopExecutor;

#[async_trait]
impl Executor for NoopExecutor {
    async fn execute(&self, _r: ExecuteRequest) -> Result<ExecuteResult, ExecutorError> {
        Ok(ExecuteResult::default())
    }
}

struct NoopRegistry;

impl ExecutorRegistry for NoopRegistry {
    fn get(&self, _kind: &str) -> Option<Arc<dyn Executor>> {
        Some(Arc::new(NoopExecutor))
    }
}

// ---------------------------------------------------------------------------
// Test 1 — permanent executor failure
// ---------------------------------------------------------------------------
//
// A deterministic transition whose executor returns Permanent(..) must NOT
// silently report status="succeeded". The chain runner captures the failure
// in ChainOutcome::Failed and the runtime builds a "failed" response. The
// partial instance stays in the originating state ("ready") which is NOT
// a terminal state, so `response()` cannot override the status to "completed".

#[tokio::test]
async fn permanent_executor_failure_does_not_report_completed() {
    let config = json!({
        "workflows": {
            "test.fail": {
                "initialState": "ready",
                "states": {
                    "ready": {
                        "transitions": {
                            "go": {
                                "target": "done",
                                "actor": "deterministic",
                                "executor": { "kind": "failing" }
                            }
                        }
                    },
                    "done": { "terminal": true },
                    "failed": { "terminal": true }
                }
            }
        }
    });

    let audit = Arc::new(MemoryAuditSink::new());
    let definitions = Arc::new(ConfigDefinitionStore::from_config(&config));
    let store: Arc<dyn WorkflowStore> = Arc::new(InMemoryWorkflowStore::new());
    let evidence = Arc::new(InMemoryEvidenceStore::new());
    let guards = Arc::new(DefaultGuardEvaluator::with_evidence(evidence.clone()));
    let registry: Arc<dyn ExecutorRegistry> = Arc::new(FailingRegistry {
        error_msg: "test-induced permanent failure".to_string(),
    });

    let runtime = WorkflowRuntime::new(
        definitions,
        store,
        registry,
        guards,
        audit.clone() as Arc<dyn AuditSink>,
    )
    .with_evidence(evidence);

    let resp = runtime
        .start(StartWorkflow {
            definition_id: "test.fail".to_string(),
            input: json!({}),
            principal: Principal::anonymous(),
            trace_id: None,
            run_id: None,
            depth: 0,
            parent: None,
        })
        .await
        .expect("start should not propagate an Err — failure paths return Ok(response)");

    let status = resp
        .pointer("/result/status")
        .and_then(Value::as_str)
        .unwrap_or("MISSING");
    let state = resp
        .pointer("/workflow/state")
        .and_then(Value::as_str)
        .unwrap_or("MISSING");

    // The contract: permanent executor failure must NOT silently say "succeeded".
    // Expected: status="failed", state="ready" (originating state, non-terminal,
    // so the response() method cannot upgrade it to "succeeded").
    assert_ne!(
        status, "succeeded",
        "permanent executor failure must NOT report status='completed'; \
         got status={status} state={state}\nfull response: {resp:#}"
    );

    // Tighter: the runtime should report "failed" specifically.
    assert_eq!(
        status, "failed",
        "permanent executor failure should report status='failed'; \
         got status={status} state={state}\nfull response: {resp:#}"
    );
}

// ---------------------------------------------------------------------------
// Test 2 — guard rejection blocks state advance
// ---------------------------------------------------------------------------
//
// Design note: the original plan proposed using actor="deterministic" with a
// guard on a single-candidate transition. That path does NOT evaluate guards —
// the chain runner's `select_deterministic_transition` skips guard evaluation
// when there is exactly one candidate (runtime_chain.rs line ~669). Guards on
// single deterministic transitions are a dead letter today.
//
// To actually exercise guard rejection, we use `runtime.submit()` with an
// agent-actor transition that carries a guard. `submit` always evaluates
// guards on the requested transition. When the guard fails, the runtime
// returns status="rejected" and the workflow stays in the originating state.
//
// Guard + unset-slot note: the SPEC (§9) mandates that `expr` guards reading
// a context path that is not set at all raise GUARD_UNSET_SLOT (fail-fast),
// NOT a silent `false`. To exercise the ordinary GUARD_REJECTED path we must
// ensure the slot IS set but to a falsy value. We do this via `initialContext`
// so that `context.ready` exists as `false` from the moment the workflow starts.
//
// This test also serves as a regression guard for the guard evaluation path
// itself: if someone accidentally removes the guards_pass() call from submit,
// the assertion on state will catch it.

#[tokio::test]
async fn guard_failure_on_submit_blocks_advance() {
    let config = json!({
        "workflows": {
            "test.guard": {
                "initialState": "waiting",
                // Set context.ready = false at start so the guard evaluates to false
                // (rather than GUARD_UNSET_SLOT, which fires when the path is absent).
                "initialContext": { "ready": false },
                "states": {
                    "waiting": {
                        "transitions": {
                            "advance": {
                                "target": "done",
                                // actor: "agent" (default) — submit evaluates guards
                                "guards": [
                                    { "kind": "expr", "expr": "$.context.ready == true" }
                                ]
                            }
                        }
                    },
                    "done": { "terminal": true }
                }
            }
        }
    });

    let audit = Arc::new(MemoryAuditSink::new());
    let definitions = Arc::new(ConfigDefinitionStore::from_config(&config));
    let store: Arc<dyn WorkflowStore> = Arc::new(InMemoryWorkflowStore::new());
    let evidence = Arc::new(InMemoryEvidenceStore::new());
    let guards = Arc::new(DefaultGuardEvaluator::with_evidence(evidence.clone()));
    let registry: Arc<dyn ExecutorRegistry> = Arc::new(NoopRegistry);

    let runtime = WorkflowRuntime::new(
        definitions,
        store,
        registry,
        guards,
        audit.clone() as Arc<dyn AuditSink>,
    )
    .with_evidence(evidence);

    // Step 1: start the workflow. It stops in "waiting" (no deterministic
    // transitions, so the chain runner exits at the decision-point check).
    let start_resp = runtime
        .start(StartWorkflow {
            definition_id: "test.guard".to_string(),
            input: json!({}),
            principal: Principal::anonymous(),
            trace_id: None,
            run_id: None,
            depth: 0,
            parent: None,
        })
        .await
        .expect("start should succeed");

    let workflow_id = start_resp
        .pointer("/workflow/id")
        .and_then(Value::as_str)
        .expect("response must carry workflow.id");
    let version = start_resp
        .pointer("/workflow/version")
        .and_then(Value::as_u64)
        .expect("response must carry workflow.version");

    let start_state = start_resp
        .pointer("/workflow/state")
        .and_then(Value::as_str)
        .unwrap_or("MISSING");
    assert_eq!(
        start_state, "waiting",
        "workflow should start in 'waiting'; got {start_state}"
    );

    // Step 2: attempt to advance via `submit` WITHOUT satisfying the guard
    // (context.ready is absent / not true). The guard `$.context.ready == true`
    // should evaluate false.
    let submit_resp = runtime
        .submit(SubmitTransition {
            workflow_id: workflow_id.to_string(),
            expected_version: version,
            transition: "advance".to_string(),
            arguments: json!({}),
            principal: Principal::anonymous(),
            summary: None,
            trace_id: None,
            run_id: None,
        })
        .await
        .expect("submit should return Ok(response) even when guard fails");

    let submit_status = submit_resp
        .pointer("/result/status")
        .and_then(Value::as_str)
        .unwrap_or("MISSING");
    let submit_state = submit_resp
        .pointer("/workflow/state")
        .and_then(Value::as_str)
        .unwrap_or("MISSING");

    // ADR-0008 — a rejected move does not resolve the mission; it stays in
    // process (`running`) with the rejection in `error`. The state-unchanged
    // assertion below is the load-bearing claim that the guard blocked advance.
    assert_eq!(
        submit_status, "running",
        "guard failure leaves the mission in process; \
         got status={submit_status} state={submit_state}\nfull response: {submit_resp:#}"
    );
    assert_eq!(
        submit_resp.pointer("/error/code").and_then(Value::as_str),
        Some("GUARD_REJECTED"),
        "guard failure must surface the rejection in error.code; got: {submit_resp:#}"
    );
    assert_eq!(
        submit_state, "waiting",
        "guard failure must NOT advance the workflow from 'waiting'; \
         got state={submit_state}\nfull response: {submit_resp:#}"
    );

    // Error code should be GUARD_REJECTED
    let error_code = submit_resp
        .pointer("/error/code")
        .and_then(Value::as_str)
        .unwrap_or("MISSING");
    assert_eq!(
        error_code, "GUARD_REJECTED",
        "guard failure should surface error.code='GUARD_REJECTED'; got {error_code}"
    );
}

// ---------------------------------------------------------------------------
// Test 3 — runtime-level timeout (STUBBED)
// ---------------------------------------------------------------------------
//
// The runtime's timeout mechanism is LAZY: it fires on the NEXT `get` or
// `submit` call after `definition.timeoutMs` has elapsed since `started_at`.
// There is no wall-clock goroutine / async watchdog that fires independently.
//
// What this means for testing:
// - A "slow executor + tight timeout" test would NOT exercise the runtime
//   timeout path; it would only race the executor against the start() caller.
// - The correct test shape is: start a workflow with a short `timeoutMs`,
//   wait for it to elapse, then call `get` and assert the workflow transitions
//   to `onTimeout.target` with status="timed_out".
// - That test is straightforward and should be added in v0.4; it belongs in
//   the main integration suite alongside the `sub_workflow_times_out` test in
//   workflow_executor.rs (which tests executor-level timeout, not runtime-level).
//
// Tracking: runtime lazy-timeout integration test — add to v0.4 scope.

#[tokio::test]
async fn runtime_timeout_transitions_workflow_to_terminal() {
    // T25 — workflow with timeoutMs=50 should reach the onTimeout
    // target after the wall-clock elapses. Two mechanisms cooperate:
    //  - The active watchdog spawned at start fires at ~50ms and
    //    calls get() internally, triggering the lazy check.
    //  - The test's own get() at 150ms acts as the backstop in case
    //    the watchdog is slow under load.
    // Either way the workflow lands in the timed_out terminal state.
    use praxec_core::model::GetWorkflow;

    let config = json!({
        "workflows": {
            "test.timeout": {
                "initialState": "working",
                "timeoutMs": 50,
                "onTimeout": { "target": "timed_out" },
                "states": {
                    "working": { "transitions": {} },
                    // ADR-0008 — the onTimeout target is a failure terminal, so
                    // any read of a timed-out mission resolves it honestly to
                    // `failed` (not a success-shaped unmarked terminal).
                    "timed_out": { "terminal": true, "outcome": "failure" }
                }
            }
        }
    });

    let audit = Arc::new(MemoryAuditSink::new());
    let definitions = Arc::new(ConfigDefinitionStore::from_config(&config));
    let store: Arc<dyn WorkflowStore> = Arc::new(InMemoryWorkflowStore::new());
    let evidence = Arc::new(InMemoryEvidenceStore::new());
    let guards = Arc::new(DefaultGuardEvaluator::with_evidence(evidence.clone()));
    let registry: Arc<dyn ExecutorRegistry> = Arc::new(NoopRegistry);

    let runtime = WorkflowRuntime::new(
        definitions,
        store,
        registry,
        guards,
        audit.clone() as Arc<dyn AuditSink>,
    )
    .with_evidence(evidence);

    let resp = runtime
        .start(StartWorkflow {
            definition_id: "test.timeout".to_string(),
            input: json!({}),
            principal: Principal::anonymous(),
            trace_id: None,
            run_id: None,
            depth: 0,
            parent: None,
        })
        .await
        .expect("start succeeds");
    let workflow_id = resp
        .pointer("/workflow/id")
        .and_then(Value::as_str)
        .expect("workflow id present")
        .to_string();
    // Sanity: at start time, the workflow is in `working`.
    assert_eq!(
        resp.pointer("/workflow/state").and_then(Value::as_str),
        Some("working")
    );

    // Sleep past the 50ms timeout. 150ms gives the watchdog plenty
    // of headroom on a busy CI box.
    tokio::time::sleep(std::time::Duration::from_millis(150)).await;

    let get_resp = runtime
        .get(GetWorkflow {
            workflow_id: workflow_id.clone(),
            principal: Principal::anonymous(),
            trace_id: None,
            run_id: None,
        })
        .await
        .expect("get succeeds after timeout");

    assert_eq!(
        get_resp.pointer("/workflow/state").and_then(Value::as_str),
        Some("timed_out"),
        "workflow.state must transition to onTimeout.target; got: {get_resp:#}"
    );
    // ADR-0008 — a timed-out mission resolves to `failed` (the `timed_out`
    // state is marked `outcome: failure`). The precise `reason` is path-
    // dependent (the call that *applies* the timeout reports `timed_out`; a
    // later read of the failure terminal reports `guard_unmet`) since the
    // terminal state does not carry how it was reached — so we assert the
    // load-bearing claim: the mission is `failed`, never success-shaped.
    assert_eq!(
        get_resp.pointer("/result/status").and_then(Value::as_str),
        Some("failed"),
        "a timed-out mission must resolve to failed; got: {get_resp:#}"
    );
}

// ---------------------------------------------------------------------------
// Test 4 — cancellation mid-walk (STUBBED)
// ---------------------------------------------------------------------------
//
// WorkflowRuntime has no cancel/stop API today. There is no
// CancellationToken, no `fn cancel`, no stop signal. The runtime processes
// one call at a time per invocation; there is no background loop to cancel.
//
// If a caller wants to "abandon" a workflow, the current mechanism is:
// simply stop calling submit/get. The workflow stays in its current state
// in the store indefinitely (or until a timeout fires on next access).
//
// A proper cancellation API would need to:
//   1. Accept a workflow id and set a "cancelled" flag on the instance.
//   2. Return a structured "cancelled" terminal state.
//   3. Reject subsequent submit calls with a clear WORKFLOW_CANCELLED code.
//
// Tracking: cancellation API — file as v0.4 task before GA if needed.

#[tokio::test]
async fn cancellation_mid_walk_leaves_recoverable_state() {
    // T24 contract: WorkflowRuntime::cancel(workflow_id, reason) sets
    // a cancelled flag on the instance without changing its `state`
    // field. Subsequent get() returns result.status="cancelled" but
    // workflow.state preserves the original position (recoverable).
    // Subsequent submit() refuses with WORKFLOW_CANCELLED.
    use praxec_core::model::GetWorkflow;

    let config = json!({
        "workflows": {
            "test.cancel": {
                "initialState": "ready",
                "states": {
                    "ready": {
                        "transitions": {
                            "go": {
                                "target": "done",
                                "actor": "agent",
                                "argumentsSchema": {"type": "object"},
                            }
                        }
                    },
                    "done": { "terminal": true }
                }
            }
        }
    });

    let audit = Arc::new(MemoryAuditSink::new());
    let definitions = Arc::new(ConfigDefinitionStore::from_config(&config));
    let store: Arc<dyn WorkflowStore> = Arc::new(InMemoryWorkflowStore::new());
    let evidence = Arc::new(InMemoryEvidenceStore::new());
    let guards = Arc::new(DefaultGuardEvaluator::with_evidence(evidence.clone()));
    let registry: Arc<dyn ExecutorRegistry> = Arc::new(NoopRegistry);

    let runtime = WorkflowRuntime::new(
        definitions,
        store,
        registry,
        guards,
        audit.clone() as Arc<dyn AuditSink>,
    )
    .with_evidence(evidence);

    let resp = runtime
        .start(StartWorkflow {
            definition_id: "test.cancel".to_string(),
            input: json!({}),
            principal: Principal::anonymous(),
            trace_id: None,
            run_id: None,
            depth: 0,
            parent: None,
        })
        .await
        .expect("start succeeds");
    let workflow_id = resp
        .pointer("/workflow/id")
        .and_then(Value::as_str)
        .expect("workflow id present")
        .to_string();

    // Cancel.
    runtime
        .cancel(&workflow_id, "operator-requested abort")
        .await
        .expect("cancel succeeds");

    // get() surfaces the cancellation; state is recoverable.
    let get_resp = runtime
        .get(GetWorkflow {
            workflow_id: workflow_id.clone(),
            principal: Principal::anonymous(),
            trace_id: None,
            run_id: None,
        })
        .await
        .expect("get succeeds after cancel");
    assert_eq!(
        get_resp.pointer("/result/status").and_then(Value::as_str),
        Some("failed"),
        "result.status must be 'failed' after cancel; got: {get_resp:#}"
    );
    assert_eq!(
        get_resp.pointer("/result/reason").and_then(Value::as_str),
        Some("cancelled"),
        "result.reason must be 'cancelled' after cancel; got: {get_resp:#}"
    );
    assert_eq!(
        get_resp.pointer("/workflow/state").and_then(Value::as_str),
        Some("ready"),
        "workflow.state must be PRESERVED (recoverable) after cancel; got: {get_resp:#}"
    );
    // Cancellation reason flows back through the response error payload.
    let err_reason = get_resp
        .pointer("/error/cancelled_reason")
        .and_then(Value::as_str)
        .unwrap_or("");
    assert!(
        err_reason.contains("operator-requested abort"),
        "cancellation reason must appear in error body; got: {get_resp:#}"
    );

    // Submit refuses with WORKFLOW_CANCELLED.
    let submit_err = runtime
        .submit(SubmitTransition {
            workflow_id: workflow_id.clone(),
            transition: "go".to_string(),
            arguments: json!({}),
            principal: Principal::anonymous(),
            expected_version: 0,
            summary: None,
            trace_id: None,
            run_id: None,
        })
        .await
        .expect_err("submit must refuse after cancel");
    let msg = submit_err.to_string();
    assert!(
        msg.contains("WORKFLOW_CANCELLED"),
        "submit error must name WORKFLOW_CANCELLED; got: {msg}"
    );
    assert!(
        msg.contains("operator-requested abort"),
        "submit error must surface the cancellation reason; got: {msg}"
    );

    // Idempotency: a second cancel returns Ok without erroring.
    runtime
        .cancel(&workflow_id, "re-cancel ignored")
        .await
        .expect("re-cancel is idempotent");
}
