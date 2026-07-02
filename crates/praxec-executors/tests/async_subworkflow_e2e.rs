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
use serde_json::{json, Value};

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
    );
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
            trace_id: None,
            run_id: None,
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
