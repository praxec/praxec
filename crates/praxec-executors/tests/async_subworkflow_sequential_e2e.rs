//! Follow-on to async sub-workflow liveness: a parent with SEQUENTIAL
//! `kind: workflow` leaves (leaf A → leaf B).
//!
//! The prior suspend fix only covered parents with a SINGLE sub-workflow leaf.
//! With two sequential leaves, leaf A's `_subworkflow_wait` (written when A's
//! child parked) was never cleared once A resolved and the parent advanced. The
//! executor's reuse path keyed off `_subworkflow_wait/child_workflow_id` WITHOUT
//! checking the wait's `transition` matched the firing leaf, so leaf B reused
//! leaf A's (already-done) child and mapped A's output instead of spawning its
//! own.
//!
//! The fix is twofold:
//!   1. transition-scoped reuse — only reuse the recorded child when the wait's
//!      `transition` equals the firing transition;
//!   2. clear `_subworkflow_wait` when the parent advances past a resolved leaf.
//!
//! This test fails before the fix (B reuses A's child; B's marker is A's, and no
//! fresh child is spawned) and passes after.

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

/// `parent` has TWO sequential `actor: agent` `kind: workflow` transitions:
///   spawning --run_a(→child_a)--> mid --run_b(→child_b)--> parent_done
///
/// Each child parks on a human gate; the gate writes a DISTINCT marker into the
/// child's context (from the submitted arguments). The parent maps the child's
/// returned (legacy full) context `$.output.marker` into a per-leaf parent slot,
/// so we can prove leaf B mapped child_b's marker ("B"), not child_a's ("A").
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
          run_a:
            target: mid
            executor:
              kind: workflow
              definitionId: child_a
            output:
              marker_a: "$.output.marker"
      mid:
        actor: agent
        transitions:
          run_b:
            target: parent_done
            executor:
              kind: workflow
              definitionId: child_b
            output:
              marker_b: "$.output.marker"
      parent_done:
        terminal: true
  child_a:
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
            output:
              marker: "$.arguments.marker"
      child_done:
        terminal: true
  child_b:
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
            output:
              marker: "$.arguments.marker"
      child_done:
        terminal: true
"#,
    )
    .unwrap()
}

fn build_runtime() -> (WorkflowRuntime, Arc<InMemoryWorkflowStore>) {
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
        audit as Arc<dyn AuditSink>,
    );
    workflow_handle.set_runtime(runtime.clone());

    (runtime, store)
}

/// Submit a `kind: workflow` leaf, capture the parked child id, drive the
/// child's human gate to terminal (which re-drives + advances the parent), then
/// return the child id. Asserts the parent parks (`waiting`) and the child
/// terminates (`succeeded`).
async fn fire_leaf_and_resolve(
    runtime: &WorkflowRuntime,
    store: &Arc<InMemoryWorkflowStore>,
    parent_id: &str,
    leaf: &str,
    marker: &str,
) -> String {
    let parent = store.load(parent_id).await.unwrap();
    let suspended = runtime
        .submit(SubmitTransition {
            workflow_id: parent_id.to_string(),
            expected_version: parent.version,
            transition: leaf.into(),
            arguments: json!({}),
            principal: Principal::anonymous(),
            summary: None,
            trace_id: None,
            run_id: None,
        })
        .await
        .expect("leaf submit (parks, not errors)");
    assert_eq!(
        suspended["result"]["status"].as_str(),
        Some("waiting"),
        "parent must report waiting while parked on {leaf}'s child: {suspended:#}"
    );

    let parent_inst = store.load(parent_id).await.unwrap();
    let child_id = parent_inst.context["_subworkflow_wait"]["child_workflow_id"]
        .as_str()
        .unwrap_or_else(|| panic!("parent must record {leaf}'s parked child id"))
        .to_string();
    assert_eq!(
        parent_inst.context["_subworkflow_wait"]["transition"].as_str(),
        Some(leaf),
        "the wait must name the firing leaf {leaf}"
    );

    let child_inst = store.load(&child_id).await.unwrap();
    let child_done = runtime
        .submit(SubmitTransition {
            workflow_id: child_id.clone(),
            expected_version: child_inst.version,
            transition: "approve".into(),
            arguments: json!({ "marker": marker }),
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
        "child for {leaf} must reach terminal: {child_done:#}"
    );
    child_id
}

#[tokio::test]
async fn sequential_subworkflow_leaves_spawn_fresh_children() {
    let (runtime, store) = build_runtime();

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

    // Leaf A: spawn child_a, park, resolve. After A resolves the parent must
    // advance to `mid` AND clear A's `_subworkflow_wait`.
    let child_a = fire_leaf_and_resolve(&runtime, &store, &parent_id, "run_a", "A").await;

    let after_a = store.load(&parent_id).await.unwrap();
    assert_eq!(
        after_a.state, "mid",
        "parent must advance to `mid` once child_a terminates; got {}",
        after_a.state
    );
    assert!(
        after_a.context.get("_subworkflow_wait").is_none(),
        "A's _subworkflow_wait must be cleared once the parent advances past leaf A; \
         instead it lingers: {:#}",
        after_a.context
    );
    assert_eq!(
        after_a.context["marker_a"].as_str(),
        Some("A"),
        "leaf A must have mapped child_a's marker"
    );

    // Leaf B: spawn child_b, park, resolve. B MUST spawn a FRESH child (not
    // reuse child_a) and map child_b's own marker.
    let child_b = fire_leaf_and_resolve(&runtime, &store, &parent_id, "run_b", "B").await;

    // The fresh-child assertion: B's child id differs from A's.
    assert_ne!(
        child_b, child_a,
        "leaf B must SPAWN A FRESH child, not reuse leaf A's already-done child \
         (B reused {child_a})"
    );

    let after_b = store.load(&parent_id).await.unwrap();
    assert_eq!(
        after_b.state, "parent_done",
        "parent must reach terminal once child_b terminates; got {}",
        after_b.state
    );
    // B mapped its OWN child's marker, not A's.
    assert_eq!(
        after_b.context["marker_b"].as_str(),
        Some("B"),
        "leaf B must map child_b's marker (\"B\"), not child_a's (\"A\"): {:#}",
        after_b.context
    );
    assert!(
        after_b.context.get("_subworkflow_wait").is_none(),
        "B's _subworkflow_wait must be cleared once the parent reaches terminal: {:#}",
        after_b.context
    );
}
