//! Finding #18 — the parent↔child binding must be persisted at SPAWN time,
//! not only at clean suspend.
//!
//! A `kind: workflow` parent transition spawns a child whose start/auto-drive
//! path ERRORS (its deterministic leaf fails on the first attempt). The
//! parent's submit is rejected wholesale — but the child row persists in the
//! store. Before the fix, no `_subworkflow_wait` record was written on this
//! path, so a child that was later rescued out-of-band was unreachable: every
//! parent re-submit spawned a brand-new child.
//!
//! This test asserts:
//! 1. After the errored submit, the parent's PERSISTED context carries the
//!    `_subworkflow_wait` record naming the persisted child (the record must
//!    survive the rejected submit).
//! 2. When the child is rescued out-of-band (its failed deterministic leaf
//!    re-submitted; the injected failure clears on the second attempt), the
//!    parent REUSES that child — no second child row, exactly one
//!    `sub_workflow.started` — and advances to its own terminal state.

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

/// `parent` has a single `actor: agent` `kind: workflow` transition that
/// spawns `child`. `child`'s initial state auto-fires a deterministic cli
/// leaf that FAILS the first time it runs (no marker file yet) and SUCCEEDS
/// on every later attempt (the first failing run touches the marker) — the
/// easiest reliable "child errors after its row is persisted, then is
/// rescuable out-of-band" injection the cli harness offers.
fn config(marker: &str) -> Value {
    let yaml = r#"
version: "1.0.0"
connections:
  shell:
    kind: cli
    command: bash
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
          auto:
            target: child_done
            actor: deterministic
            executor:
              kind: cli
              connection: shell
              args:
                - "-c"
                - "test -f MARKER && exit 0; touch MARKER; exit 1"
      child_done:
        terminal: true
"#
    .replace("MARKER", marker);
    resolve_str(&yaml).unwrap()
}

fn build_runtime(
    cfg: &Value,
) -> (
    WorkflowRuntime,
    Arc<InMemoryWorkflowStore>,
    Arc<MemoryAuditSink>,
) {
    let audit = Arc::new(MemoryAuditSink::new());
    let definitions = Arc::new(ConfigDefinitionStore::from_config(cfg));
    let store = Arc::new(InMemoryWorkflowStore::new());
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
        store.clone() as Arc<dyn WorkflowStore>,
        registry,
        guards,
        audit.clone() as Arc<dyn AuditSink>,
    )
    .with_writable_repo_roots(vec![praxec_core::RepoRoot::for_test()]);
    workflow_handle.set_runtime(runtime.clone());

    (runtime, store, audit)
}

#[tokio::test]
async fn errored_child_dispatch_persists_binding_and_stays_harvestable() {
    // Skip if bash isn't available in this environment.
    if std::process::Command::new("bash")
        .arg("-c")
        .arg("true")
        .output()
        .is_err()
    {
        eprintln!("skipping: bash not available");
        return;
    }

    let tmp = tempfile::tempdir().expect("tempdir");
    let marker = tmp
        .path()
        .join("first_attempt_done")
        .to_string_lossy()
        .into_owned();
    let cfg = config(&marker);
    let (runtime, store, audit) = build_runtime(&cfg);

    // 1. Start the parent. Its `run_child` transition is agent-dispatchable,
    //    so the parent sits in `spawning` until we submit it.
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

    // 2. Submit the `kind: workflow` transition. The child spawns; its
    //    deterministic leaf fails (first attempt), so the child's start
    //    resolves CHAIN_FAILED and the parent's submit is rejected wholesale.
    let rejected = runtime
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
        .expect("parent submit returns a (failed) response, not a transport error");
    assert_ne!(
        rejected["result"]["status"].as_str(),
        Some("succeeded"),
        "the errored child dispatch must not report success: {rejected:#}"
    );
    // Parent did NOT advance.
    let parent_inst = store.load(&parent_id).await.unwrap();
    assert_eq!(parent_inst.state, "spawning", "parent must stay parked");

    // 3. THE FINDING: the child row persisted, so the parent's persisted
    //    context MUST carry the `_subworkflow_wait` binding naming it —
    //    written at SPAWN time through a channel that survives the rejected
    //    submit. Without it, the rescued child below is unreachable.
    let wait = parent_inst.context.get("_subworkflow_wait").expect(
        "finding #18: the parent context must record the _subworkflow_wait \
         binding for the spawned child even though the child's start errored \
         (the record must be persisted at spawn, not only at clean suspend)",
    );
    assert_eq!(
        wait["transition"].as_str(),
        Some("run_child"),
        "the binding must be keyed by the parent transition name: {wait:#}"
    );
    let child_id = wait["child_workflow_id"]
        .as_str()
        .expect("binding must name the persisted child")
        .to_string();
    // The named child row really does exist and is non-terminal (its
    // deterministic leaf failed, leaving it parked in `pending`).
    let child_inst = store.load(&child_id).await.unwrap();
    assert_eq!(
        child_inst.state, "pending",
        "child must be parked where its leaf failed"
    );

    // 4. Rescue the child OUT-OF-BAND: re-submit its failed deterministic
    //    leaf (the recovery link the runtime itself offers on CHAIN_FAILED).
    //    The marker now exists, so the leaf succeeds and the child reaches
    //    its terminal state — which re-drives the parent.
    let child_done = runtime
        .submit(SubmitTransition {
            workflow_id: child_id.clone(),
            expected_version: child_inst.version,
            transition: "auto".into(),
            arguments: json!({}),
            principal: Principal::anonymous(),
            summary: None,
            trace_id: None,
            run_id: None,
        })
        .await
        .expect("child rescue submit");
    assert_eq!(
        child_done["result"]["status"].as_str(),
        Some("succeeded"),
        "rescued child must reach its terminal state: {child_done:#}"
    );

    // 5. The parent must have REUSED the recorded child (not spawned a fresh
    //    one) and advanced to its own terminal state.
    let parent_final = store.load(&parent_id).await.unwrap();
    assert_eq!(
        parent_final.state, "parent_done",
        "parent must harvest the rescued child and advance; instead it is \
         stuck in {} with context {:#}",
        parent_final.state, parent_final.context
    );
    // The binding is cleared on successful harvest, exactly as on the clean
    // suspend path.
    assert!(
        parent_final.context.get("_subworkflow_wait").is_none(),
        "the resolved leaf's _subworkflow_wait must be cleared on advance; \
         instead it lingers: {:#}",
        parent_final.context
    );
    // Exactly one child was ever spawned: `workflow.started` fires once per
    // instance created by `start` (unlike `sub_workflow.started`, which the
    // executor skips when the child's start errors). Two events = the parent
    // plus ONE child; a third would prove the re-drive minted a duplicate
    // child instead of reusing the recorded one.
    let started = audit
        .event_types()
        .into_iter()
        .filter(|t| t == "workflow.started")
        .count();
    assert_eq!(
        started, 2,
        "exactly one child must ever be spawned (parent + child = 2 \
         workflow.started events); the re-drive must REUSE the recorded \
         child, not mint a fresh one (saw {started})"
    );
}
