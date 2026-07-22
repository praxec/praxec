//! Task C (Stage 1b) — async sub-workflow liveness end-to-end.
//!
//! A `kind: workflow` parent transition spawns a child that parks on a human
//! gate. The parent durably suspends (`_subworkflow_wait`). When the child's
//! human gate is driven to terminal, the runtime must RE-DRIVE the parent's
//! pending transition (linked child→parent), which re-enters the executor's
//! reuse path, sees the child terminal, and advances the parent to its own
//! terminal state — with NO second child spawned and NO re-drive loop (which
//! would hang this test).

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

fn human() -> Principal {
    Principal {
        subject: "user:reviewer".into(),
        roles: vec!["human".into()],
        permissions: vec![],
    }
}

/// `parent` has a single `actor: agent` `kind: workflow` transition that spawns
/// `child`. `child` parks on a single `actor: human` gate (`approve`) whose
/// target is terminal — so it is cleanly terminal after exactly one submit.
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

fn build_runtime() -> (
    WorkflowRuntime,
    Arc<InMemoryWorkflowStore>,
    Arc<MemoryAuditSink>,
) {
    let cfg = config();
    let audit = Arc::new(MemoryAuditSink::new());
    let definitions = Arc::new(ConfigDefinitionStore::from_config(&cfg));
    let store = Arc::new(InMemoryWorkflowStore::new());
    let guards = Arc::new(DefaultGuardEvaluator::new());

    let mcp_conns = McpConnections::from_config(&cfg);
    let cli_conns = Arc::new(CliConnections::from_config(&cfg));
    let (registry, workflow_handle) = default_registry_with_late_workflow(
        &cfg,
        Arc::new(McpExecutor::new(mcp_conns)),
        cli_conns,
        audit.clone() as Arc<dyn AuditSink>,
    );

    let runtime = WorkflowRuntime::new(
        definitions,
        store.clone() as Arc<dyn WorkflowStore>,
        registry,
        guards,
        audit.clone() as Arc<dyn AuditSink>,
    )
    .with_writable_repo_roots(vec![praxec_core::RepoRoot::for_test()]);
    // Late-bind the runtime into the `workflow` executor so `kind: workflow`
    // transitions spawn/drive sub-workflows through THIS runtime.
    workflow_handle.set_runtime(runtime.clone());

    (runtime, store, audit)
}

#[tokio::test]
async fn suspended_parent_resumes_when_child_terminates() {
    let (runtime, store, audit) = build_runtime();

    // 1. Start the parent. The `run_child` transition is agent-dispatchable, so
    //    the parent sits in `spawning` until we submit it.
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

    // Submit the `kind: workflow` transition. The child spawns, parks on its
    // human gate (non-terminal), so the parent durably suspends.
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
    // The child instance exists and is non-terminal (parked on its human gate).
    let child_inst = store.load(&child_id).await.unwrap();
    assert_eq!(
        child_inst.state, "pending",
        "child parked on its human gate"
    );

    // 2. Drive the child's human gate to its terminal state.
    let child_done = runtime
        .submit(SubmitTransition {
            workflow_id: child_id.clone(),
            expected_version: child_inst.version,
            transition: "approve".into(),
            arguments: json!({}),
            principal: human(),
            summary: None,
            trace_id: None,
            run_id: None,
        })
        .await
        .expect("child human submit");
    assert_eq!(
        child_done["result"]["status"].as_str(),
        Some("succeeded"),
        "child must reach its terminal state: {child_done:#}"
    );

    // 3. The PARENT must have auto-advanced to its terminal state — re-driven
    //    when the child terminated, the child REUSED (no second child spawned).
    let parent_final = store.load(&parent_id).await.unwrap();
    assert_eq!(
        parent_final.state, "parent_done",
        "parent must auto-advance to terminal once the child terminates; \
         instead it is stuck in {}",
        parent_final.state
    );

    // The wait is cleared once the parent advances past the resolved leaf — a
    // lingering wait would strand `recover_suspended_subworkflows` on a done
    // child and let a later sequential `kind: workflow` leaf reuse it.
    assert!(
        parent_final.context.get("_subworkflow_wait").is_none(),
        "the resolved leaf's _subworkflow_wait must be cleared on advance; \
         instead it lingers: {:#}",
        parent_final.context
    );

    // No second child was spawned: the re-drive REUSED the recorded child. The
    // executor emits `sub_workflow.started` only on the SPAWN path (the reuse
    // path re-checks an already-started child and emits nothing), so exactly one
    // such event proves reuse — and, since the parent advanced and did not
    // re-suspend, there was no re-drive loop (a loop would have hung this test).
    let _ = &child_id;
    let started = audit
        .event_types()
        .into_iter()
        .filter(|t| t == "sub_workflow.started")
        .count();
    assert_eq!(
        started, 1,
        "the re-drive must REUSE the recorded child, not spawn a fresh one \
         (saw {started} sub_workflow.started events)"
    );
}

/// #5 — a parent parked waiting on a child that holds a human gate must surface
/// the CHILD's gate on the PARENT's response (the parent's own `links` are empty
/// there), so a human staring at the parent sees the actionable transition
/// instead of an inscrutable `links: []`.
#[tokio::test]
async fn parent_surfaces_the_childs_human_gate() {
    use praxec_core::model::GetWorkflow;

    let (runtime, _store, _audit) = build_runtime();
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
    runtime
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
        .expect("parent parks on the child");

    // GET the parent while it is parked. Its own state has no human gate, but the
    // child does — the response must surface it.
    let resp = runtime
        .get(GetWorkflow {
            workflow_id: parent_id.clone(),
            principal: Principal::anonymous(),
            trace_id: None,
            run_id: None,
        })
        .await
        .expect("get parent");

    let ph = &resp["pending_human"];
    assert_eq!(
        ph["onChildWorkflow"], true,
        "parent must surface the child's gate: {resp:#}"
    );
    assert_eq!(ph["source"], "human_gate", "resp: {resp:#}");
    // The resolve handle targets the CHILD (not the parent), with the child's
    // human transition — the actionable step.
    assert_eq!(ph["transition"], "approve", "resp: {resp:#}");
    assert_ne!(
        ph["resolve"]["args"]["workflowId"],
        json!(parent_id),
        "resolve must target the child workflow, not the parent: {resp:#}"
    );
    assert_eq!(ph["resolve"]["requiresHuman"], true);
}

/// v0.0.28 dogfood defect 3 — a CANCELLED child must resume its parent like a
/// failed one, and the failure must be RECOVERABLE. Observed live: parent
/// parked on `_subworkflow_wait`; child cancelled via `{"intent":"cancel"}`;
/// the parent stayed `waiting` forever (the dead wait marker lingered), and
/// every re-fire of the spawning transition reused the terminal-cancelled
/// child and permanently rejected `sub-workflow failed (cancelled)` — the only
/// recovery was cancelling the whole tree.
///
/// The contract: cancel(child) re-drives the parent; the re-driven transition
/// observes the terminal child and fails LOUDLY (transition.rejected /
/// EXECUTOR_FAILED) while CONSUMING the dead `_subworkflow_wait` — so the
/// parent is actionable again and a subsequent submit of the same spawning
/// transition spawns a FRESH child rather than permanently rejecting.
#[tokio::test]
async fn a_cancelled_child_fails_the_parent_loudly_and_leaves_it_refireable() {
    let (runtime, store, audit) = build_runtime();

    // 1. Start the parent and park it on the child (same as the happy path).
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
    assert_eq!(suspended["result"]["status"].as_str(), Some("waiting"));
    let child_id = {
        let parent_inst = store.load(&parent_id).await.unwrap();
        parent_inst.context["_subworkflow_wait"]["child_workflow_id"]
            .as_str()
            .expect("parent must record the parked child id")
            .to_string()
    };

    // 2. Cancel the CHILD. The cancel must re-drive the parent (synchronously,
    //    inside `cancel`), whose executor observes the terminal-cancelled
    //    child, fails loudly, and consumes the dead wait.
    runtime
        .cancel(&child_id, "operator abort")
        .await
        .expect("cancel child");

    let parent_after = store.load(&parent_id).await.unwrap();
    assert_eq!(
        parent_after.state, "spawning",
        "the parent must NOT advance past a cancelled child"
    );
    assert!(
        parent_after.cancelled_at.is_none(),
        "cancelling the child must never cancel the parent"
    );
    assert!(
        parent_after.context.get("_subworkflow_wait").is_none(),
        "the dead wait on the terminal-cancelled child must be CONSUMED — a \
         lingering wait leaves the parent `waiting` forever and every re-fire \
         reusing the dead child; context: {:#}",
        parent_after.context
    );

    // The failure was LOUD: the re-driven transition recorded EXECUTOR_FAILED
    // carrying the cancelled reason — never a silent re-park.
    let rejected = audit
        .snapshot()
        .into_iter()
        .find(|e| {
            e.event_type == "transition.rejected"
                && e.payload["code"] == json!("EXECUTOR_FAILED")
                && e.workflow_id.as_deref() == Some(parent_id.as_str())
        })
        .expect("the re-driven parent transition must fail LOUDLY (transition.rejected)");
    assert!(
        rejected.payload["message"]
            .as_str()
            .is_some_and(|m| m.contains("cancelled")),
        "the loud failure must carry the child's cancelled reason: {:#}",
        rejected.payload
    );

    // 3. The parent is re-fireable: a fresh submit of the SAME spawning
    //    transition must spawn a FRESH child (not permanently reject on the
    //    cancelled one).
    let refire = runtime
        .submit(SubmitTransition {
            workflow_id: parent_id.clone(),
            expected_version: parent_after.version,
            transition: "run_child".into(),
            arguments: json!({}),
            principal: Principal::anonymous(),
            summary: None,
            trace_id: None,
            run_id: None,
        })
        .await
        .expect("re-fire of the spawning transition must be accepted");
    assert_eq!(
        refire["result"]["status"].as_str(),
        Some("waiting"),
        "the re-fire must park on a FRESH child, not reject: {refire:#}"
    );
    let fresh_child_id = {
        let parent_inst = store.load(&parent_id).await.unwrap();
        parent_inst.context["_subworkflow_wait"]["child_workflow_id"]
            .as_str()
            .expect("the re-fired parent must record its fresh child")
            .to_string()
    };
    assert_ne!(
        fresh_child_id, child_id,
        "the re-fire must spawn a FRESH child, never reuse the cancelled one"
    );
    let started = audit
        .event_types()
        .into_iter()
        .filter(|t| t == "sub_workflow.started")
        .count();
    assert_eq!(
        started, 2,
        "exactly two spawns: the original child and the post-cancel fresh one"
    );
}
