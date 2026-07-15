//! Task D — restart recovery for sub-workflow-suspended parents.
//!
//! Tasks A/B/C make a `kind: workflow` parent durably suspend
//! (`_subworkflow_wait`) on a non-terminal child, and re-drive that parent
//! in-process when the child later terminates. But if the GATEWAY RESTARTS
//! while a parent is suspended and the child terminated during the downtime,
//! the in-process re-drive is gone — the parent would stay `waiting` forever.
//!
//! `recover_suspended_subworkflows` (mirroring `recover_suspended_locks`) is the
//! startup recovery pass: it scans the store for parents still carrying a
//! `_subworkflow_wait` record and re-drives each one. The reuse path re-checks
//! the recorded child; a now-terminal child advances the parent past its
//! `kind: workflow` transition.
//!
//! This test simulates a restart: it suspends a parent on a child via one
//! runtime, terminates the child DIRECTLY in the store (no runtime hooks — the
//! in-process re-drive that would have fired during normal operation is lost),
//! then builds a FRESH runtime over the same store and calls the recovery pass.
//! The parent must advance to its terminal `parent_done` state (not stay
//! `waiting`).

use std::sync::Arc;

use praxec_core::audit::{AuditSink, MemoryAuditSink};
use praxec_core::config::resolve_str;
use praxec_core::guards::DefaultGuardEvaluator;
use praxec_core::model::{Principal, StartWorkflow, SubmitTransition};
use praxec_core::ports::WorkflowStore;
use praxec_core::runtime::WorkflowRuntime;
use praxec_core::store::{ConfigDefinitionStore, InMemoryWorkflowStore};
use praxec_executors::default_registry_with_late_workflow;
use praxec_executors::{CliConnections, McpConnections, McpExecutor};
use serde_json::{Value, json};

/// Build a runtime over a CALLER-SUPPLIED store, so two successive runtimes can
/// share one store (the restart simulation: same durable state, fresh process).
fn build_runtime(
    cfg: &Value,
    store: Arc<InMemoryWorkflowStore>,
) -> (WorkflowRuntime, Arc<MemoryAuditSink>) {
    let audit = Arc::new(MemoryAuditSink::new());
    let definitions = Arc::new(ConfigDefinitionStore::from_config(cfg));
    let guards = Arc::new(DefaultGuardEvaluator::new());

    let mcp_conns = McpConnections::from_config(cfg);
    let cli_conns = Arc::new(CliConnections::from_config(cfg));
    let (registry, workflow_handle) = default_registry_with_late_workflow(
        cfg,
        Arc::new(McpExecutor::new(mcp_conns)),
        cli_conns,
        audit.clone() as Arc<dyn AuditSink>,
    );

    let runtime = WorkflowRuntime::new(
        definitions,
        store as Arc<dyn WorkflowStore>,
        registry,
        guards,
        audit.clone() as Arc<dyn AuditSink>,
    )
    .with_writable_repo_roots(vec![praxec_core::RepoRoot::for_test()]);
    workflow_handle.set_runtime(runtime.clone());

    (runtime, audit)
}

/// `parent` spawns `child` via a `kind: workflow` transition; `child` parks on a
/// single human gate. So the parent suspends (`waiting`) while the child is
/// non-terminal. The child's `child_done` target is a SUCCESS terminal, so a
/// re-driven parent (once the child is terminal) advances to `parent_done`.
fn config() -> Value {
    resolve_str(
        r#"
version: "1.0.0"
workflows:
  parent:
    version: "1.0.0"
    initialState: spawning
    states:
      spawning:
        actor: agent
        transitions:
          run_child:
            target: parent_done
            executor:
              kind: workflow
              definitionId: child
      parent_done:
        terminal: true
  child:
    version: "1.0.0"
    initialState: pending
    states:
      pending:
        transitions:
          approve:
            target: child_done
            actor: human
            executor:
              kind: human
      child_done:
        terminal: true
"#,
    )
    .unwrap()
}

/// Start `parent`, submit its `run_child` `kind: workflow` transition, and
/// return `(parent_id, child_id)` after asserting the parent durably suspended
/// (`waiting`) on a non-terminal child parked at its human gate.
async fn start_and_suspend(
    runtime: &WorkflowRuntime,
    store: &InMemoryWorkflowStore,
) -> (String, String) {
    let start = runtime
        .start(StartWorkflow {
            definition_id: "parent".into(),
            input: json!({}),
            principal: Principal::anonymous(),
            run_env: praxec_core::RunEnv::for_test(),
            depth: 0,
            parent: None,
        })
        .await
        .expect("parent start");
    let parent_id = start["workflow"]["id"].as_str().unwrap().to_string();

    let suspended = runtime
        .submit(SubmitTransition {
            workflow_id: parent_id.clone(),
            expected_version: 0,
            transition: "run_child".into(),
            arguments: json!({}),
            principal: Principal::anonymous(),
            summary: None,
            trace_id: None,
            run_id: None,
        })
        .await
        .expect("parent submit (parks, not errors)");

    assert_eq!(
        suspended["result"]["status"].as_str(),
        Some("waiting"),
        "parent must report waiting while parked on the child: {suspended:#}"
    );

    let child_id = {
        let parent_inst = store.load(&parent_id).await.unwrap();
        parent_inst.context["_subworkflow_wait"]["child_workflow_id"]
            .as_str()
            .expect("parent must record the parked child id")
            .to_string()
    };
    let child_inst = store.load(&child_id).await.unwrap();
    assert_eq!(
        child_inst.state, "pending",
        "child must park (non-terminal) on its human gate before the restart"
    );

    (parent_id, child_id)
}

#[tokio::test]
async fn recover_resumes_parent_whose_child_terminated_during_downtime() {
    let cfg = config();
    // One store, shared across the "before restart" and "after restart" runtimes.
    let store = Arc::new(InMemoryWorkflowStore::new());

    // --- before restart: suspend the parent on a non-terminal child ---
    let (runtime_a, _audit_a) = build_runtime(&cfg, store.clone());
    let (parent_id, child_id) = start_and_suspend(&runtime_a, &store).await;

    // Simulate the child terminating DURING DOWNTIME: flip it to its terminal
    // success state directly in the store, bypassing the runtime so the
    // in-process `resume_parent_if_any` re-drive NEVER fires. This is exactly
    // the gap — a child that finished while the gateway was down.
    {
        let mut child = store.load(&child_id).await.unwrap();
        let expected = child.version;
        child.state = "child_done".into();
        child.version += 1;
        store
            .save_if_version(child, expected)
            .await
            .expect("terminate child directly in store");
    }
    // Drop the first runtime: the in-memory re-drive that would have resumed the
    // parent is gone. The parent is still `waiting` with its `_subworkflow_wait`.
    drop(runtime_a);
    let parent_before = store.load(&parent_id).await.unwrap();
    assert_eq!(
        parent_before.state, "spawning",
        "parent is still parked at its kind:workflow source state after the simulated restart"
    );
    assert!(
        parent_before.context.get("_subworkflow_wait").is_some(),
        "parent still carries its durable _subworkflow_wait record across the restart"
    );

    // --- after restart: a fresh runtime over the same store ---
    let (runtime_b, audit_b) = build_runtime(&cfg, store.clone());

    // The startup recovery pass: re-drive every parent still suspended on a
    // sub-workflow. The reuse path sees the now-terminal child and advances.
    runtime_b.recover_suspended_subworkflows().await;

    // The parent must have advanced PAST its kind:workflow transition to its
    // own terminal state — it is NOT still waiting.
    let parent_after = store.load(&parent_id).await.unwrap();
    assert_eq!(
        parent_after.state, "parent_done",
        "restart recovery must re-drive the suspended parent so the now-terminal \
         child advances it to its terminal state (not stuck waiting)"
    );

    // A recovery-driven resume must be visible in the audit trail with the SAME
    // event name the live re-drive path emits, so audit consumers can't tell a
    // recovery-driven resume from a live one.
    assert!(
        audit_b
            .event_types()
            .iter()
            .any(|e| e == "sub_workflow.parent.resumed"),
        "restart recovery must emit a sub_workflow.parent.resumed audit event \
         (parity with the live resume_parent_if_any path): {:?}",
        audit_b.event_types()
    );
}

/// Corruption guard + mixed-list parity: a `_subworkflow_wait` record with an
/// empty `transition` is corrupt and must be SKIPPED (no blind-submit, no panic,
/// parent left untouched), while a VALID suspended parent in the same recovery
/// pass still advances. Mirrors the lock-recovery corruption guard.
#[tokio::test]
async fn recover_skips_corrupt_wait_and_still_recovers_valid_parent() {
    let cfg = config();
    let store = Arc::new(InMemoryWorkflowStore::new());

    // --- a VALID suspended parent whose child terminated during downtime ---
    let (runtime_a, _audit_a) = build_runtime(&cfg, store.clone());
    let (valid_parent_id, child_id) = start_and_suspend(&runtime_a, &store).await;
    {
        let mut child = store.load(&child_id).await.unwrap();
        let expected = child.version;
        child.state = "child_done".into();
        child.version += 1;
        store
            .save_if_version(child, expected)
            .await
            .expect("terminate valid parent's child directly in store");
    }

    // --- a CORRUPT suspended parent: a second parent whose _subworkflow_wait
    // carries an empty transition (corruption that survived the restart) ---
    let (corrupt_parent_id, _corrupt_child_id) = start_and_suspend(&runtime_a, &store).await;
    let corrupt_context_before = {
        let mut corrupt = store.load(&corrupt_parent_id).await.unwrap();
        let expected = corrupt.version;
        // Blank out the transition in the durable _subworkflow_wait record.
        corrupt.context["_subworkflow_wait"]["transition"] = json!("");
        corrupt.version += 1;
        store
            .save_if_version(corrupt.clone(), expected)
            .await
            .expect("persist corrupt _subworkflow_wait");
        store.load(&corrupt_parent_id).await.unwrap().context
    };
    drop(runtime_a);

    // --- after restart: a fresh runtime over the same store ---
    let (runtime_b, audit_b) = build_runtime(&cfg, store.clone());

    // Must not panic on the corrupt entry; must still advance the valid one.
    runtime_b.recover_suspended_subworkflows().await;

    // The VALID parent advanced past its kind:workflow transition.
    let valid_after = store.load(&valid_parent_id).await.unwrap();
    assert_eq!(
        valid_after.state, "parent_done",
        "a valid suspended parent must still recover even when a corrupt entry is \
         present in the same recovery pass"
    );

    // The CORRUPT parent was left UNTOUCHED — not advanced, not blind-submitted;
    // its context (including the corrupt _subworkflow_wait) is intact.
    let corrupt_after = store.load(&corrupt_parent_id).await.unwrap();
    assert_eq!(
        corrupt_after.state, "spawning",
        "the corrupt parent must NOT be advanced/blind-submitted by recovery"
    );
    assert_eq!(
        corrupt_after.context, corrupt_context_before,
        "the corrupt parent's context must be left untouched by recovery"
    );

    // The corruption is observable in the audit trail.
    assert!(
        audit_b
            .event_types()
            .iter()
            .any(|e| e == "sub_workflow.recover.corrupt"),
        "recovery must emit a sub_workflow.recover.corrupt audit event for the \
         skipped corrupt parent: {:?}",
        audit_b.event_types()
    );
}
