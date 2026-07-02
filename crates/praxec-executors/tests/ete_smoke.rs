//! Tranche 2a — ETE compositional smoke. Drives `examples/smoke-ete/gateway.yaml`
//! through the REAL workflow runtime + REAL executor registry (parallel,
//! pipeline, noop), proving the v0.4 primitives compose end-to-end.
//!
//! No API key needed. The `delegate:`-style sub-agent paths are NOT
//! exercised here — those are covered by the live smoke at
//! `examples/smoke-ete/walk-live.sh`.

use std::path::PathBuf;
use std::sync::Arc;

use praxec_core::audit::{AuditSink, MemoryAuditSink};
use praxec_core::config::load_resolved;
use praxec_core::guards::DefaultGuardEvaluator;
use praxec_core::model::{Principal, StartWorkflow, SubmitTransition};
use praxec_core::store::{ConfigDefinitionStore, InMemoryWorkflowStore};
use praxec_core::WorkflowRuntime;
use praxec_executors::{default_registry_with_mcp, CliConnections, McpConnections, McpExecutor};
use serde_json::{json, Value};

fn examples_dir() -> PathBuf {
    let mut p = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    p.pop(); // crates/praxec-executors → crates
    p.pop(); // crates → workspace root
    p.push("examples");
    p
}

fn build_runtime() -> (WorkflowRuntime, Arc<MemoryAuditSink>) {
    let path = examples_dir().join("smoke-ete/gateway.yaml");
    let cfg =
        load_resolved(&path).unwrap_or_else(|e| panic!("smoke-ete/gateway.yaml must resolve: {e}"));
    let definitions = Arc::new(ConfigDefinitionStore::from_config(&cfg));
    let store = Arc::new(InMemoryWorkflowStore::new());
    let audit = Arc::new(MemoryAuditSink::new());
    // default_registry_with_mcp lets us share the test's audit sink
    // with the executors (including parallel + pipeline) so their
    // emitted events show up in our snapshot. The plain
    // default_registry() hardcodes NullAuditSink for executors,
    // which silently swallows their audit trail.
    let mcp_conns = McpConnections::from_config(&cfg);
    let cli_conns = Arc::new(CliConnections::from_config(&cfg));
    let executors = default_registry_with_mcp(
        &cfg,
        Arc::new(McpExecutor::new(mcp_conns)),
        cli_conns,
        audit.clone() as Arc<dyn AuditSink>,
    );
    let guards = Arc::new(DefaultGuardEvaluator::new());
    let runtime = WorkflowRuntime::new(
        definitions,
        store,
        executors,
        guards,
        audit.clone() as Arc<dyn AuditSink>,
    );
    (runtime, audit)
}

#[tokio::test]
async fn smoke_ete_walks_to_ship_via_v04_primitives() {
    let (runtime, audit) = build_runtime();
    let start = runtime
        .start(StartWorkflow {
            definition_id: "smoke_ete".into(),
            input: json!({ "queries": ["alpha", "beta", "gamma"] }),
            principal: Principal::anonymous(),
            trace_id: None,
            run_id: None,
            depth: 0,
            parent: None,
        })
        .await
        .expect("start must succeed");

    let workflow_id = start
        .pointer("/workflow/id")
        .and_then(Value::as_str)
        .expect("start response carries workflow id")
        .to_string();

    let mut version = start
        .pointer("/workflow/version")
        .and_then(Value::as_u64)
        .expect("start response carries version");

    // Walk: drive every non-decision-point transition until terminal.
    let mut steps = 0;
    let mut resp = start;
    while steps < 20 {
        if resp.pointer("/result/status").and_then(Value::as_str) == Some("succeeded") {
            break;
        }
        if let Some(failed) = resp.pointer("/result/status").and_then(Value::as_str) {
            if failed == "failed" || failed == "rejected" {
                panic!(
                    "smoke walk failed at step {steps}: {}",
                    serde_json::to_string_pretty(&resp).unwrap()
                );
            }
        }
        let links = resp
            .get("links")
            .and_then(Value::as_array)
            .cloned()
            .unwrap_or_default();
        // Walker simulates an agent; skip transitions that require a
        // human principal (e.g. auto-injected `ask_human` from
        // enable_human_ask). HITL paths are exercised by dedicated
        // tests against the real spawner, not this composition smoke.
        let rel = links.iter().find_map(|l: &Value| {
            if l.get("actor").and_then(Value::as_str) == Some("human") {
                return None;
            }
            l.get("rel").and_then(Value::as_str).map(String::from)
        });
        let Some(transition) = rel else {
            break;
        };
        resp = runtime
            .submit(SubmitTransition {
                workflow_id: workflow_id.clone(),
                transition: transition.clone(),
                expected_version: version,
                arguments: json!({}),
                principal: Principal::anonymous(),
                summary: None,
                trace_id: None,
                run_id: None,
            })
            .await
            .unwrap_or_else(|e| panic!("submit({transition}) failed: {e}"));
        version = resp
            .pointer("/workflow/version")
            .and_then(Value::as_u64)
            .unwrap_or(version);
        steps += 1;
    }

    let final_state = resp
        .pointer("/workflow/state")
        .and_then(Value::as_str)
        .unwrap_or("?");
    assert_eq!(
        final_state, "ship",
        "smoke walk must reach `ship` terminal; final state: {final_state}, steps: {steps}"
    );

    // Composition assertions — every v0.4 primitive should have left a trace.
    let events: Vec<String> = audit.snapshot().into_iter().map(|e| e.event_type).collect();
    assert!(
        events.iter().any(|t| t == "parallel.fanout.completed"),
        "parallel executor must have emitted fanout.completed; got: {events:?}"
    );
    assert!(
        events.iter().any(|t| t == "pipeline.completed"),
        "pipeline executor must have emitted pipeline.completed; got: {events:?}"
    );
}

#[tokio::test]
async fn smoke_ete_path_allowlist_rejects_disallowed_path() {
    // Sanity: the path_allowlist constraint actually fires when violated.
    // We bypass the workflow's hardcoded `set: ["allowed/..."]` and
    // directly probe the constraint evaluator against a bad value.
    use praxec_core::slot_constraint::evaluate_constraints;
    let path = examples_dir().join("smoke-ete/gateway.yaml");
    let cfg = load_resolved(&path).unwrap();
    let definition = cfg.pointer("/workflows/smoke_ete").unwrap().clone();
    let bad_context = json!({
        "validated_paths": ["allowed/auth/login.rs", "secrets/key.pem"]
    });
    let v = evaluate_constraints(&definition, "scan", &bad_context)
        .expect_err("disallowed path must be rejected");
    assert_eq!(v.slot, "validated_paths");
    assert_eq!(v.constraint_kind, "path_allowlist");
    assert!(
        v.message.contains("secrets/key.pem"),
        "violation must name the offending path; got: {}",
        v.message
    );
}

#[tokio::test]
async fn smoke_ete_enable_human_ask_injected_into_states() {
    // §29.3 — the workflow declared `enable_human_ask: true`. After
    // config resolution, every non-terminal state should carry a
    // self-loop ask_human transition.
    let path = examples_dir().join("smoke-ete/gateway.yaml");
    let cfg = load_resolved(&path).unwrap();
    for state in &["scan", "verify", "validate_paths"] {
        let ask = cfg.pointer(&format!(
            "/workflows/smoke_ete/states/{state}/transitions/ask_human"
        ));
        assert!(
            ask.is_some(),
            "ask_human must be injected into non-terminal state '{state}'"
        );
    }
    assert!(
        cfg.pointer("/workflows/smoke_ete/states/ship/transitions/ask_human")
            .is_none(),
        "ask_human must NOT be injected into terminal state 'ship'"
    );
}
