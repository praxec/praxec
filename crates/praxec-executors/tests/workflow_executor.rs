//! Integration tests for the workflow executor.
//!
//! Tests use MemoryAuditSink and InMemoryWorkflowStore for fast,
//! filesystem-free verification.

use std::sync::Arc;

use async_trait::async_trait;
use praxec_core::audit::{AuditSink, MemoryAuditSink};
use praxec_core::config::resolve_str;
use praxec_core::guards::DefaultGuardEvaluator;
use praxec_core::model::{ExecuteRequest, ExecuteResult};
use praxec_core::ports::{Executor, ExecutorRegistry, WorkflowStore};
use praxec_core::runtime::WorkflowRuntime;
use praxec_core::store::{ConfigDefinitionStore, InMemoryEvidenceStore, InMemoryWorkflowStore};
use praxec_executors::workflow::WorkflowExecutor;
use serde_json::json;

fn build_runtime() -> (WorkflowRuntime, Arc<MemoryAuditSink>) {
    let config = resolve_str(
        r#"
version: "1.0.0"
workflows:
  auto_complete:
    initialState: done
    states:
      done:
        terminal: true
  two_step:
    initialState: first
    states:
      first:
        transitions:
          go:
            target: done
            executor:
              kind: noop
      done:
        terminal: true
  never_ends:
    initialState: waiting
    states:
      waiting:
        transitions:
          loop:
            target: waiting
            executor:
              kind: noop
"#,
    )
    .unwrap();

    let audit = Arc::new(MemoryAuditSink::new());
    let definitions = Arc::new(ConfigDefinitionStore::from_config(&config));
    let store: Arc<dyn WorkflowStore> = Arc::new(InMemoryWorkflowStore::new());
    let evidence = Arc::new(InMemoryEvidenceStore::new());
    let guards = Arc::new(DefaultGuardEvaluator::with_evidence(evidence.clone()));

    // Build a minimal executor registry with noop
    struct NoopExecutor;
    #[async_trait]
    impl Executor for NoopExecutor {
        async fn execute(
            &self,
            _request: ExecuteRequest,
        ) -> Result<ExecuteResult, praxec_core::error::ExecutorError> {
            Ok(ExecuteResult::default())
        }
    }

    struct SingleExecutorRegistry(Arc<dyn Executor>);
    impl ExecutorRegistry for SingleExecutorRegistry {
        fn get(&self, _kind: &str) -> Option<Arc<dyn Executor>> {
            Some(self.0.clone())
        }
    }

    let runtime = WorkflowRuntime::new(
        definitions,
        store,
        Arc::new(SingleExecutorRegistry(Arc::new(NoopExecutor))),
        guards,
        audit.clone() as Arc<dyn AuditSink>,
    )
    .with_evidence(evidence);

    (runtime, audit)
}

#[tokio::test]
async fn sub_workflow_completes_and_returns_context() {
    let (runtime, _audit) = build_runtime();
    let executor = WorkflowExecutor::new(runtime.clone(), runtime.audit().clone());

    let result = executor
        .execute(ExecuteRequest {
            workflow: praxec_core::model::WorkflowInstance {
                id: "parent_wf".to_string(),
                definition_id: "parent".to_string(),
                definition_version: "1.0.0".to_string(),
                definition: json!({"initialState": "running", "states": {}}),
                state: "running".to_string(),
                version: 1,
                input: json!({}),
                context: json!({}),
                started_at: chrono::Utc::now(),
                trace_id: None,
                run_id: None,
                cancelled_at: None,
                cancelled_reason: None,
                depth: 0,
                parent: None,
            },
            transition: Some("run_sub".to_string()),
            arguments: json!({}),
            executor_config: json!({
                "definitionId": "auto_complete",
                "input": {},
            }),
            idempotency_key: None,
            correlation_id: None,
        })
        .await
        .unwrap();

    assert!(
        result.output.is_object(),
        "sub-workflow should return context object"
    );
}

#[tokio::test]
async fn sub_workflow_terminal_at_spawn_returns_context() {
    // A child already terminal at spawn (`auto_complete`'s initialState is
    // terminal) advances on the deterministic fast-path: `execute` reads the
    // status off the `start` response, sees `succeeded`, and returns the child
    // context to the parent — no poll. (Formerly `sub_workflow_polls_until_terminal`;
    // there is no poll anymore, only a single check-once.)
    let (runtime, _audit) = build_runtime();
    let executor = WorkflowExecutor::new(runtime.clone(), runtime.audit().clone());

    let result = executor
        .execute(ExecuteRequest {
            workflow: praxec_core::model::WorkflowInstance {
                id: "parent_wf_2".to_string(),
                definition_id: "parent".to_string(),
                definition_version: "1.0.0".to_string(),
                definition: json!({"initialState": "running", "states": {}}),
                state: "running".to_string(),
                version: 1,
                input: json!({}),
                context: json!({}),
                started_at: chrono::Utc::now(),
                trace_id: None,
                run_id: None,
                cancelled_at: None,
                cancelled_reason: None,
                depth: 0,
                parent: None,
            },
            transition: Some("run_sub".to_string()),
            arguments: json!({}),
            executor_config: json!({
                // auto_complete's initialState is already terminal,
                // proving the run-on-enter completion path returns
                // context to the parent executor.
                "definitionId": "auto_complete",
                "input": {"trigger": "polled"},
                "timeoutMs": 5_000,
            }),
            idempotency_key: None,
            correlation_id: None,
        })
        .await
        .unwrap();

    assert!(result.output.is_object(), "sub-workflow should complete");
}

// NOTE: the former `sub_workflow_with_no_explicit_timeout_still_aborts_when_it_stalls`
// test is retired — it asserted a no-progress backstop that existed only because
// the executor polled. There is no poll anymore: a non-terminal child suspends
// (see `non_terminal_child_suspends_instead_of_polling` below), so there is
// nothing to "stall" on.

#[tokio::test]
async fn non_terminal_child_suspends_instead_of_polling() {
    // P2: a non-terminal child (`never_ends` sits non-terminal forever) must
    // NOT poll. `execute` checks status once and returns `Ok` carrying
    // `suspend = Some(_)` with a non-empty child id, so the runtime durably
    // parks the parent. The whole call must return promptly (no 200ms poll).
    let (runtime, _audit) = build_runtime();
    let executor = WorkflowExecutor::new(runtime.clone(), runtime.audit().clone());

    let fut = executor.execute(ExecuteRequest {
        workflow: praxec_core::model::WorkflowInstance {
            id: "parent_wf_suspend".to_string(),
            definition_id: "parent".to_string(),
            definition_version: "1.0.0".to_string(),
            definition: json!({"initialState": "running", "states": {}}),
            state: "running".to_string(),
            version: 1,
            input: json!({}),
            context: json!({}),
            started_at: chrono::Utc::now(),
            trace_id: None,
            run_id: None,
            cancelled_at: None,
            cancelled_reason: None,
            depth: 0,
            parent: None,
        },
        transition: Some("run_sub".to_string()),
        arguments: json!({}),
        executor_config: json!({
            "definitionId": "never_ends",
            "input": {},
        }),
        idempotency_key: None,
        correlation_id: None,
    });

    // Promptness: a poll tick is 200ms; check-once must finish well under that.
    let result = tokio::time::timeout(std::time::Duration::from_millis(150), fut)
        .await
        .expect("check-once must return promptly — no poll")
        .expect("a non-terminal child must suspend, not error");

    let suspend = result
        .suspend
        .expect("a non-terminal child must produce ExecuteResult.suspend");
    assert!(
        !suspend.child_workflow_id.is_empty(),
        "suspend must carry the child workflow id"
    );
    assert_eq!(
        result.child_workflow_id.as_deref(),
        Some(suspend.child_workflow_id.as_str()),
        "child_workflow_id must mirror the suspended child"
    );
}

#[tokio::test]
async fn terminal_child_advances_fast_path() {
    // P2: a child already terminal at spawn (`auto_complete`) advances on the
    // fast-path — no suspend — and returns the success path's output object.
    let (runtime, _audit) = build_runtime();
    let executor = WorkflowExecutor::new(runtime.clone(), runtime.audit().clone());

    let result = executor
        .execute(ExecuteRequest {
            workflow: praxec_core::model::WorkflowInstance {
                id: "parent_wf_fast".to_string(),
                definition_id: "parent".to_string(),
                definition_version: "1.0.0".to_string(),
                definition: json!({"initialState": "running", "states": {}}),
                state: "running".to_string(),
                version: 1,
                input: json!({}),
                context: json!({}),
                started_at: chrono::Utc::now(),
                trace_id: None,
                run_id: None,
                cancelled_at: None,
                cancelled_reason: None,
                depth: 0,
                parent: None,
            },
            transition: Some("run_sub".to_string()),
            arguments: json!({}),
            executor_config: json!({
                "definitionId": "auto_complete",
                "input": {},
            }),
            idempotency_key: None,
            correlation_id: None,
        })
        .await
        .unwrap();

    assert!(
        result.suspend.is_none(),
        "a terminal child must advance, not suspend"
    );
    assert!(
        result.output.is_object(),
        "the fast-path success return must carry the child context object"
    );
    assert!(
        result.child_workflow_id.is_some(),
        "the fast-path must report the child workflow id"
    );
}

#[tokio::test]
async fn reevaluation_reuses_the_recorded_child() {
    // P2: re-drive after a suspend. The first `execute` against `never_ends`
    // suspends and hands back the child id; feeding that id back via
    // `_subworkflow_wait` must re-check the SAME child — no fresh `start`.
    let (runtime, audit) = build_runtime();
    let executor = WorkflowExecutor::new(runtime.clone(), runtime.audit().clone());

    let make_request = |context: serde_json::Value| ExecuteRequest {
        workflow: praxec_core::model::WorkflowInstance {
            id: "parent_wf_reuse".to_string(),
            definition_id: "parent".to_string(),
            definition_version: "1.0.0".to_string(),
            definition: json!({"initialState": "running", "states": {}}),
            state: "running".to_string(),
            version: 1,
            input: json!({}),
            context,
            started_at: chrono::Utc::now(),
            trace_id: None,
            run_id: None,
            cancelled_at: None,
            cancelled_reason: None,
            depth: 0,
            parent: None,
        },
        transition: Some("run_sub".to_string()),
        arguments: json!({}),
        executor_config: json!({
            "definitionId": "never_ends",
            "input": {},
        }),
        idempotency_key: None,
        correlation_id: None,
    };

    // First pass: spawn → suspend, capturing the started child id.
    let first = executor
        .execute(make_request(json!({})))
        .await
        .expect("first pass suspends on a non-terminal child");
    let child_id = first
        .suspend
        .expect("first pass must suspend")
        .child_workflow_id;

    // Re-drive: feed the recorded child id back via `_subworkflow_wait`.
    let reused = executor
        .execute(make_request(json!({
            "_subworkflow_wait": {
                "child_workflow_id": child_id,
                "transition": "run_sub",
            }
        })))
        .await
        .expect("re-drive must re-check the recorded child");

    // No fresh `start`: the returned child id must equal the reused one.
    assert_eq!(
        reused.child_workflow_id.as_deref(),
        Some(child_id.as_str()),
        "re-drive must reuse the recorded child, not start a new one"
    );
    // Still non-terminal → still suspended on the same child.
    assert_eq!(
        reused.suspend.map(|s| s.child_workflow_id),
        Some(child_id),
        "the reused non-terminal child must suspend again on the same id"
    );
    // The "no duplicate child" invariant, asserted directly: the spawn path
    // emits exactly one `sub_workflow.started` per child it starts, and the
    // reuse path never calls `start`. So across both passes there must be
    // exactly ONE started event — proving the re-drive did not mint a second
    // instance (a stronger check than child-id equality alone).
    let started = audit
        .snapshot()
        .into_iter()
        .filter(|e| e.event_type == "sub_workflow.started")
        .count();
    assert_eq!(
        started, 1,
        "exactly one child must ever be started; the re-drive must not spawn a second"
    );
}

// NOTE: the former `sub_workflow_times_out` test is retired. It spawned a
// non-terminal child with a short `timeoutMs` and expected the poll loop to
// abort with `Timeout`. There is no poll to bound: a non-terminal child now
// suspends durably (the runtime owns liveness, not a wall-clock poll budget),
// covered by `non_terminal_child_suspends_instead_of_polling`.

#[tokio::test]
async fn sub_workflow_missing_definition_surfaces_as_executor_error() {
    let (runtime, _audit) = build_runtime();
    let executor = WorkflowExecutor::new(runtime.clone(), runtime.audit().clone());

    let result = executor
        .execute(ExecuteRequest {
            workflow: praxec_core::model::WorkflowInstance {
                id: "parent_wf_err".to_string(),
                definition_id: "parent".to_string(),
                definition_version: "1.0.0".to_string(),
                definition: json!({"initialState": "running", "states": {}}),
                state: "running".to_string(),
                version: 1,
                input: json!({}),
                context: json!({}),
                started_at: chrono::Utc::now(),
                trace_id: None,
                run_id: None,
                cancelled_at: None,
                cancelled_reason: None,
                depth: 0,
                parent: None,
            },
            transition: Some("run_sub".to_string()),
            arguments: json!({}),
            executor_config: json!({
                "definitionId": "does_not_exist",
                "input": {},
            }),
            idempotency_key: None,
            correlation_id: None,
        })
        .await;

    assert!(result.is_err(), "should fail when definitionId is unknown");
    assert!(
        matches!(
            result.unwrap_err(),
            praxec_core::error::ExecutorError::Permanent(_)
        ),
        "expected Permanent error for missing definition"
    );
}

#[tokio::test]
async fn sub_workflow_audit_events_emitted() {
    let (runtime, audit) = build_runtime();
    let executor = WorkflowExecutor::new(runtime.clone(), runtime.audit().clone());

    executor
        .execute(ExecuteRequest {
            workflow: praxec_core::model::WorkflowInstance {
                id: "parent_wf_4".to_string(),
                definition_id: "parent".to_string(),
                definition_version: "1.0.0".to_string(),
                definition: json!({"initialState": "running", "states": {}}),
                state: "running".to_string(),
                version: 1,
                input: json!({}),
                context: json!({}),
                started_at: chrono::Utc::now(),
                trace_id: None,
                run_id: None,
                cancelled_at: None,
                cancelled_reason: None,
                depth: 0,
                parent: None,
            },
            transition: Some("run_sub".to_string()),
            arguments: json!({}),
            executor_config: json!({
                "definitionId": "auto_complete",
                "input": {},
            }),
            idempotency_key: None,
            correlation_id: None,
        })
        .await
        .unwrap();

    let events = audit.snapshot();
    let event_types: Vec<&str> = events.iter().map(|e| e.event_type.as_str()).collect();
    assert!(
        event_types.contains(&"sub_workflow.started"),
        "should have sub_workflow.started event, got: {:?}",
        event_types
    );
    assert!(
        event_types.contains(&"sub_workflow.completed"),
        "should have sub_workflow.completed event, got: {:?}",
        event_types
    );
}

// ── CMP-044 — sub-workflow recursion depth guard ─────────────────────────────
//
// A `workflow`-kind transition whose sub-definition recurses (here `recurse`
// auto-advances into another `recurse` instance on start) would otherwise
// nest until the stack blows. The depth guard reads the parent instance's
// persisted `depth` and rejects with WORKFLOW_DEPTH_EXCEEDED once
// MAX_WORKFLOW_DEPTH (10) is reached; each spawn stamps the child at
// `parent.depth + 1` via `start`, so the count propagates parent→child as
// data (surviving an async re-drive — see the former task-local note above).

#[tokio::test]
async fn sub_workflow_recursion_depth_is_capped() {
    use std::sync::OnceLock;

    // `recurse`'s initial state has a single `workflow`-kind transition that
    // auto-advances on start, invoking `recurse` again → unbounded nesting
    // absent a guard.
    let config = resolve_str(
        r#"
version: "1.0.0"
workflows:
  recurse:
    initialState: spin
    states:
      spin:
        transitions:
          again:
            actor: deterministic
            target: done
            executor:
              kind: workflow
              definitionId: recurse
      done:
        terminal: true
"#,
    )
    .unwrap();

    let audit = Arc::new(MemoryAuditSink::new());
    let definitions = Arc::new(ConfigDefinitionStore::from_config(&config));
    let store: Arc<dyn WorkflowStore> = Arc::new(InMemoryWorkflowStore::new());
    let evidence = Arc::new(InMemoryEvidenceStore::new());
    let guards = Arc::new(DefaultGuardEvaluator::with_evidence(evidence.clone()));

    // Registry that routes the `workflow` kind to a WorkflowExecutor wired
    // after the runtime exists (chicken-and-egg via OnceLock).
    struct LazyWorkflowRegistry(Arc<OnceLock<Arc<WorkflowExecutor>>>);
    impl ExecutorRegistry for LazyWorkflowRegistry {
        fn get(&self, _kind: &str) -> Option<Arc<dyn Executor>> {
            self.0.get().map(|e| e.clone() as Arc<dyn Executor>)
        }
    }

    let cell: Arc<OnceLock<Arc<WorkflowExecutor>>> = Arc::new(OnceLock::new());
    let runtime = WorkflowRuntime::new(
        definitions,
        store,
        Arc::new(LazyWorkflowRegistry(cell.clone())),
        guards,
        audit.clone() as Arc<dyn AuditSink>,
    )
    .with_evidence(evidence);

    let wf_exec = Arc::new(WorkflowExecutor::new(runtime.clone(), audit.clone()));
    cell.set(wf_exec.clone()).ok();

    let result = wf_exec
        .execute(ExecuteRequest {
            workflow: praxec_core::model::WorkflowInstance {
                id: "depth_parent".to_string(),
                definition_id: "parent".to_string(),
                definition_version: "1.0.0".to_string(),
                definition: json!({"initialState": "running", "states": {}}),
                state: "running".to_string(),
                version: 1,
                input: json!({}),
                context: json!({}),
                started_at: chrono::Utc::now(),
                trace_id: None,
                run_id: None,
                cancelled_at: None,
                cancelled_reason: None,
                depth: 0,
                parent: None,
            },
            transition: Some("run_sub".to_string()),
            arguments: json!({}),
            executor_config: json!({
                "definitionId": "recurse",
                "input": {},
            }),
            idempotency_key: None,
            correlation_id: None,
        })
        .await;

    let err = result.expect_err("unbounded recursion must be rejected by the depth guard");
    assert!(
        format!("{err:?}").contains("WORKFLOW_DEPTH_EXCEEDED"),
        "expected WORKFLOW_DEPTH_EXCEEDED, got: {err:?}"
    );
}

#[tokio::test]
async fn depth_guard_reads_instance_data_not_a_task_local() {
    // A parent instance already at the cap must be rejected by the executor
    // using ONLY its instance `depth` field — invoked directly (no WORKFLOW_DEPTH
    // task-local scope in effect), proving the guard no longer depends on the
    // synchronous call stack.
    let (runtime, _audit) = build_runtime();
    let executor = WorkflowExecutor::new(runtime.clone(), runtime.audit().clone());

    let parent = praxec_core::model::WorkflowInstance {
        id: "parent_at_cap".to_string(),
        definition_id: "parent".to_string(),
        definition_version: "1.0.0".to_string(),
        definition: json!({"initialState": "running", "states": {}}),
        state: "running".to_string(),
        version: 1,
        input: json!({}),
        context: json!({}),
        started_at: chrono::Utc::now(),
        trace_id: None,
        run_id: None,
        cancelled_at: None,
        cancelled_reason: None,
        depth: 10, // == MAX_WORKFLOW_DEPTH
        parent: None,
    };

    let err = executor
        .execute(ExecuteRequest {
            workflow: parent,
            transition: Some("run_sub".to_string()),
            arguments: json!({}),
            executor_config: json!({ "definitionId": "auto_complete", "input": {} }),
            idempotency_key: None,
            correlation_id: None,
        })
        .await
        .expect_err("a parent at the depth cap must be rejected from instance data");
    assert!(
        format!("{err:?}").contains("WORKFLOW_DEPTH_EXCEEDED"),
        "expected WORKFLOW_DEPTH_EXCEEDED from instance.depth, got {err:?}"
    );
}

// --- C1 late-binding (set_runtime) tests --------------------------------

/// Helper: a parent-workflow ExecuteRequest invoking `auto_complete`.
fn run_sub_request(parent_id: &str) -> ExecuteRequest {
    ExecuteRequest {
        workflow: praxec_core::model::WorkflowInstance {
            id: parent_id.to_string(),
            definition_id: "parent".to_string(),
            definition_version: "1.0.0".to_string(),
            definition: json!({"initialState": "running", "states": {}}),
            state: "running".to_string(),
            version: 1,
            input: json!({}),
            context: json!({}),
            started_at: chrono::Utc::now(),
            trace_id: None,
            run_id: None,
            cancelled_at: None,
            cancelled_reason: None,
            depth: 0,
            parent: None,
        },
        transition: Some("run_sub".to_string()),
        arguments: json!({}),
        executor_config: json!({ "definitionId": "auto_complete", "input": {} }),
        idempotency_key: None,
        correlation_id: None,
    }
}

#[tokio::test]
async fn late_executor_without_runtime_fails_not_wired_not_panics() {
    // C1: the production registry registers a runtime-less WorkflowExecutor.
    // If the binary forgets to call set_runtime, a `kind: workflow` transition
    // must fail loud (WORKFLOW_EXECUTOR_NOT_WIRED), never panic or hang.
    let audit: Arc<dyn AuditSink> = Arc::new(MemoryAuditSink::new());
    let executor = WorkflowExecutor::late(audit);

    let err = executor
        .execute(run_sub_request("parent_unwired"))
        .await
        .expect_err("a runtime-less workflow executor must fail, not silently succeed");
    assert!(
        format!("{err:?}").contains("WORKFLOW_EXECUTOR_NOT_WIRED"),
        "expected WORKFLOW_EXECUTOR_NOT_WIRED, got: {err:?}"
    );
}

#[tokio::test]
async fn late_executor_runs_after_set_runtime() {
    // C1: the late-bound executor behaves identically to the eager one once the
    // runtime is injected — proving the wiring path the binary takes works.
    let (runtime, _audit) = build_runtime();
    let executor = WorkflowExecutor::late(runtime.audit().clone());
    executor.set_runtime(runtime.clone());

    let result = executor
        .execute(run_sub_request("parent_latebound"))
        .await
        .expect("late-bound workflow executor should run once set_runtime is called");
    assert!(result.output.is_object());
}

#[test]
#[should_panic(expected = "WORKFLOW_EXECUTOR_DOUBLE_WIRED")]
fn double_set_runtime_panics() {
    // Poka-yoke (mirrors ParallelExecutor::set_registry): wiring the runtime
    // twice is a construction bug and must fail loud, not silently keep the
    // first runtime.
    let (runtime, _audit) = build_runtime();
    let executor = WorkflowExecutor::late(runtime.audit().clone());
    executor.set_runtime(runtime.clone());
    executor.set_runtime(runtime);
}
