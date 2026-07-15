//! Task C liveness follow-up — a suspended `kind: workflow` parent must also be
//! re-driven when its child reaches terminal/finalized via paths OTHER than the
//! dispatch completion hook:
//!
//!   1. definition-level timeout (`check_and_apply_timeout`), and
//!   2. cancellation (`cancel`).
//!
//! Both previously left the parent suspended forever (fail-stuck). The child's
//! reuse path maps a timed-out/cancelled child to `failed`, so the re-driven
//! parent must fail-propagate (reach a terminal resolution) rather than stick in
//! `waiting`/`spawning`.

use std::sync::Arc;

use praxec_core::audit::{AuditSink, MemoryAuditSink};
use praxec_core::config::resolve_str;
use praxec_core::guards::DefaultGuardEvaluator;
use praxec_core::model::{GetWorkflow, Principal, StartWorkflow, SubmitTransition};
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

fn build_runtime(
    cfg: Value,
) -> (
    WorkflowRuntime,
    Arc<InMemoryWorkflowStore>,
    Arc<MemoryAuditSink>,
) {
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
    workflow_handle.set_runtime(runtime.clone());

    (runtime, store, audit)
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
        "child must park (non-terminal) on its human gate before the terminal trigger"
    );

    (parent_id, child_id)
}

/// `parent` spawns `child` via a `kind: workflow` transition. `child` parks on a
/// human gate (so the parent suspends), but also carries a definition-level
/// `timeoutMs` + `onTimeout.target` that is TERMINAL — so the lazy timeout
/// finalizes the child to a terminal state without ever firing its human gate.
fn timeout_config() -> Value {
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
    timeoutMs: 1
    onTimeout:
      target: child_timed_out
    states:
      pending:
        transitions:
          approve:
            target: child_done
            actor: human
            executor:
              kind: human
      child_timed_out:
        terminal: true
        outcome: failure
      child_done:
        terminal: true
"#,
    )
    .unwrap()
}

/// `parent` spawns `child`; `child` parks on a single human gate. The child is
/// then `cancel`led — `cancelled_at` is set while `state` stays non-terminal.
fn cancel_config() -> Value {
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

fn audit_has(audit: &MemoryAuditSink, event_type: &str) -> bool {
    audit.snapshot().iter().any(|e| e.event_type == event_type)
}

/// A BOUNDED CONVERGENCE LOOP over a `kind: workflow` leaf: state `looping`
/// carries a `while:` guard (`iter < 2`) and an `actor: deterministic`
/// `run_child` transition that spawns `child`. The child parks on a human gate
/// (so the parent suspends each pass); approving it drives the child terminal,
/// which re-drives the parent — and because `run_child` is deterministic, the
/// re-drive auto-chains the NEXT spawn, entering the loop's 2nd iteration. The
/// `iter` counter is bumped only when the child RESOLVES (the suspend path
/// short-circuits before the output write), so two resolved children leave
/// `iter == 2` and the while-guard goes false → terminal.
fn loop_over_subworkflow_config() -> Value {
    resolve_str(
        r#"
version: "1.0.0"
workflows:
  parent:
    version: "1.0.0"
    initialState: looping
    initialContext:
      iter: 0
    states:
      looping:
        actor: agent
        while: { kind: expr, expr: "$.context.iter < 2" }
        max_iterations: 5
        transitions:
          run_child:
            actor: deterministic
            target: parent_done
            executor:
              kind: workflow
              definitionId: child
            output:
              iter: { add: ["$.context.iter", 1] }
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

/// Regression: a bounded convergence loop (while: back-edge over a kind:workflow
/// leaf) must survive its 2nd iteration. The existing e2e tests cover a SINGLE
/// spawn + resume; this drives TWO passes end-to-end. The reported symptom was a
/// store rejection — `stale workflow version (expected 0 ...)` — when the loop
/// re-entered the same `kind: workflow` transition on pass 2.
#[tokio::test]
async fn bounded_loop_over_subworkflow_survives_second_iteration() {
    let (runtime, store, _audit) = build_runtime(loop_over_subworkflow_config());

    // `start` auto-fires the deterministic `run_child`, spawning child #1, which
    // parks on its human gate — so the parent suspends (`waiting`).
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
        .expect("parent start (parks on child #1)");
    let parent_id = start["workflow"]["id"].as_str().unwrap().to_string();
    assert_eq!(
        start["result"]["status"].as_str(),
        Some("waiting"),
        "parent must park on child #1: {start:#}"
    );

    // Approve each parked child in turn. Approving child #N drives it terminal,
    // which re-drives the parent; on pass 1 that auto-chains the spawn of child
    // #2 (the 2nd iteration). The bug would surface HERE as a stale-version
    // rejection of the parent's suspend save.
    for pass in 1..=2 {
        let parent_inst = store.load(&parent_id).await.unwrap();
        let child_id = parent_inst.context["_subworkflow_wait"]["child_workflow_id"]
            .as_str()
            .unwrap_or_else(|| {
                panic!(
                    "pass {pass}: parent must be parked on a child; state={} ctx={}",
                    parent_inst.state, parent_inst.context
                )
            })
            .to_string();
        let child_ver = store.load(&child_id).await.unwrap().version;
        runtime
            .submit(SubmitTransition {
                workflow_id: child_id.clone(),
                expected_version: child_ver,
                transition: "approve".into(),
                arguments: json!({}),
                principal: human(),
                summary: None,
                trace_id: None,
                run_id: None,
            })
            .await
            .unwrap_or_else(|e| {
                panic!("pass {pass}: approving child {child_id} (re-drives parent) failed: {e:?}")
            });
    }

    // After two resolved children the while-guard (`iter < 2`) is false, so the
    // parent leaves `looping` for its terminal target.
    let parent_inst = store.load(&parent_id).await.unwrap();
    assert_eq!(
        parent_inst.state, "parent_done",
        "parent must reach terminal after 2 loop iterations; state={} ctx={}",
        parent_inst.state, parent_inst.context
    );
}

/// Faithful to flow.harden.fmeca-converge: a guarded BACK-EDGE loop across TWO
/// states, each firing a DIFFERENT `kind: workflow` leaf — `assessing.fmeca`
/// → `gate` → `mitigating.remediate` → back to `assessing` — bounded by a
/// `round` counter. Each leaf's child parks on a human gate (so the parent
/// suspends + resumes per leaf). This is the structure the simple `while:`
/// self-loop above does NOT exercise: the back-edge re-enters `assessing` and
/// re-fires `fmeca` while the most recent `_subworkflow_wait` belonged to the
/// OTHER transition (`remediate`).
fn back_edge_two_leaf_config() -> Value {
    resolve_str(
        r#"
version: "1.0.0"
workflows:
  parent:
    version: "1.0.0"
    initialState: assessing
    initialContext:
      round: 0
    states:
      assessing:
        actor: agent
        transitions:
          fmeca:
            actor: deterministic
            target: gate
            executor:
              kind: workflow
              definitionId: child
      gate:
        transitions:
          converged:
            target: parent_done
            actor: deterministic
            guards:
              - { kind: expr, expr: "$.context.round >= 2" }
          mitigate:
            target: mitigating
            actor: deterministic
            guards:
              - { kind: expr, expr: "$.context.round < 2" }
            output:
              round: { add: ["$.context.round", 1] }
      mitigating:
        actor: agent
        transitions:
          remediate:
            actor: deterministic
            target: assessing
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

/// Regression (faithful topology): the two-leaf back-edge convergence loop must
/// drive to terminal. Approve each parked child in turn; each approval resumes
/// the parent, which auto-chains to the next leaf's spawn. The reported symptom
/// was `stale workflow version (expected 0 ...)` on a back-edge re-entry.
#[tokio::test]
async fn back_edge_two_leaf_convergence_loop_drives_to_terminal() {
    let (runtime, store, _audit) = build_runtime(back_edge_two_leaf_config());

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
        .expect("parent start (parks on the first fmeca child)");
    let parent_id = start["workflow"]["id"].as_str().unwrap().to_string();

    // Drive the loop: approve whatever child the parent is currently parked on,
    // until the parent reaches its terminal state (bounded so a stuck loop fails
    // loudly instead of hanging).
    let mut approvals = 0;
    loop {
        let parent_inst = store.load(&parent_id).await.unwrap();
        if parent_inst.state == "parent_done" {
            break;
        }
        let child_id = parent_inst.context["_subworkflow_wait"]["child_workflow_id"]
            .as_str()
            .unwrap_or_else(|| {
                panic!(
                    "approval {approvals}: parent neither terminal nor parked; state={} ctx={}",
                    parent_inst.state, parent_inst.context
                )
            })
            .to_string();
        let child_ver = store.load(&child_id).await.unwrap().version;
        runtime
            .submit(SubmitTransition {
                workflow_id: child_id.clone(),
                expected_version: child_ver,
                transition: "approve".into(),
                arguments: json!({}),
                principal: human(),
                summary: None,
                trace_id: None,
                run_id: None,
            })
            .await
            .unwrap_or_else(|e| {
                panic!("approval {approvals}: approving child {child_id} (resumes parent) failed: {e:?}")
            });
        approvals += 1;
        assert!(approvals <= 8, "loop did not converge within 8 approvals");
    }

    let parent_inst = store.load(&parent_id).await.unwrap();
    assert_eq!(parent_inst.state, "parent_done");
}

#[tokio::test]
async fn child_timeout_resolves_parent_not_stuck() {
    let (runtime, store, audit) = build_runtime(timeout_config());
    let (parent_id, child_id) = start_and_suspend(&runtime, &store).await;

    // Sleep past the child's 1ms timeout deadline, then fire the lazy timeout
    // via a `get` on the child (mirrors the existing timeout tests). The T25
    // watchdog may also fire it first — either way the SAME timeout path runs
    // and must re-drive the parent; the timeout is idempotent once terminal,
    // so a double-fire (watchdog + this get) does NOT re-resume in a loop
    // (this test would hang or stack-overflow if it did).
    tokio::time::sleep(std::time::Duration::from_millis(20)).await;
    runtime
        .get(GetWorkflow {
            workflow_id: child_id.clone(),
            principal: human(),
            trace_id: None,
            run_id: None,
        })
        .await
        .expect("child get fires the lazy timeout (or observes it already fired)");

    // The child finalized at its terminal onTimeout target.
    let child_inst = store.load(&child_id).await.unwrap();
    assert_eq!(
        child_inst.state, "child_timed_out",
        "child must finalize at its terminal onTimeout target"
    );

    // The suspended parent MUST have been re-driven (not silently abandoned).
    // The re-drive re-fires `run_child`, whose reuse path `get(child)` sees the
    // timed-out child resolve as `failed`, so the sub-workflow fails and the
    // parent fail-propagates rather than sticking in its suspend.
    assert!(
        audit_has(&audit, "sub_workflow.parent.resumed"),
        "the timed-out child must re-drive its suspended parent; \
         events: {:?}",
        audit
            .snapshot()
            .iter()
            .map(|e| e.event_type.clone())
            .collect::<Vec<_>>()
    );
    assert!(
        audit_has(&audit, "sub_workflow.failed"),
        "the re-driven parent must observe its timed-out child as failed (fail-propagate)"
    );

    // The parent is no longer parked succeeding-or-waiting on the child: its
    // last action resolved as `failed`. (A failed transition leaves the parent
    // at its source `spawning` state — recoverable — but the mission's last
    // action is failed, which is the non-stuck signal the operator/agent sees.)
    let parent_inst = store.load(&parent_id).await.unwrap();
    assert_eq!(
        parent_inst.state, "spawning",
        "a failed sub-workflow transition leaves the parent at its source state"
    );
    assert!(
        parent_inst.cancelled_at.is_none(),
        "parent is not cancelled — it fail-propagated the child timeout"
    );
}

#[tokio::test]
async fn child_cancel_resolves_parent_not_stuck() {
    let (runtime, store, audit) = build_runtime(cancel_config());
    let (parent_id, child_id) = start_and_suspend(&runtime, &store).await;

    // Cancel the parked child. `cancelled_at` is set; `state` stays `pending`.
    // This must re-drive the parent (no recursion: the parent submit does not
    // re-cancel the child, and a re-cancel here would idempotent-no-op).
    runtime
        .cancel(&child_id, "operator aborted the sub-workflow")
        .await
        .expect("child cancel");

    let child_inst = store.load(&child_id).await.unwrap();
    assert!(
        child_inst.cancelled_at.is_some(),
        "child must be marked cancelled"
    );

    // The suspended parent MUST have been re-driven. The reuse path `get(child)`
    // short-circuits on `cancelled_at` → `failed`, so the parent fail-propagates.
    assert!(
        audit_has(&audit, "sub_workflow.parent.resumed"),
        "the cancelled child must re-drive its suspended parent; \
         events: {:?}",
        audit
            .snapshot()
            .iter()
            .map(|e| e.event_type.clone())
            .collect::<Vec<_>>()
    );
    assert!(
        audit_has(&audit, "sub_workflow.failed"),
        "the re-driven parent must observe its cancelled child as failed (fail-propagate)"
    );

    let parent_inst = store.load(&parent_id).await.unwrap();
    assert_eq!(
        parent_inst.state, "spawning",
        "a failed sub-workflow transition leaves the parent at its source state"
    );
    assert!(
        parent_inst.cancelled_at.is_none(),
        "parent itself is not cancelled — it fail-propagated the child cancel"
    );
}
