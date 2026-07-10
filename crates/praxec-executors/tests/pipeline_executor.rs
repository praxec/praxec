//! SPEC §25 — `pipeline` executor kind tests.
//!
//! Atomic assertions for sequential composition, output threading,
//! on_step_failure bail/continue, and audit event linkage.

use std::sync::Arc;

use chrono::Utc;
use praxec_core::audit::{AuditSink, MemoryAuditSink};
use praxec_core::error::ExecutorError;
use praxec_core::model::{ExecuteRequest, WorkflowInstance};
use praxec_core::ports::ExecutorRegistry;
use praxec_executors::{CliConnections, McpConnections, McpExecutor, default_registry_with_mcp};
use serde_json::{Value, json};

fn instance_stub() -> WorkflowInstance {
    WorkflowInstance {
        id: "wf_pipeline_test".into(),
        definition_id: "demo".into(),
        definition_version: "0".into(),
        definition: json!({}),
        state: "s".into(),
        version: 0,
        input: json!({}),
        context: json!({}),
        started_at: Utc::now(),
        trace_id: None,
        run_id: None,
        cancelled_at: None,
        cancelled_reason: None,
        depth: 0,
        parent: None,
    }
}

fn build_registry(audit: Arc<MemoryAuditSink>) -> Arc<dyn ExecutorRegistry> {
    let mcp_conns = McpConnections::from_config(&json!({}));
    let cli_conns = Arc::new(CliConnections::from_config(&json!({})));
    let mcp_exec = Arc::new(McpExecutor::new(mcp_conns));
    default_registry_with_mcp(&json!({}), mcp_exec, cli_conns, audit as Arc<dyn AuditSink>)
}

fn req(executor_config: Value) -> ExecuteRequest {
    ExecuteRequest {
        workflow: instance_stub(),
        transition: Some("run-pipeline".into()),
        arguments: json!({}),
        executor_config,
        idempotency_key: Some("test-pipeline-key".into()),
        correlation_id: Some("test-pipeline-corr".into()),
    }
}

async fn run_pipeline(
    executor_config: Value,
    audit: Arc<MemoryAuditSink>,
) -> Result<praxec_core::model::ExecuteResult, ExecutorError> {
    let registry = build_registry(audit);
    let pipeline = registry.get("pipeline").expect("pipeline registered");
    pipeline.execute(req(executor_config)).await
}

#[tokio::test]
async fn two_noop_steps_succeed_in_order() {
    let audit = Arc::new(MemoryAuditSink::new());
    let result = run_pipeline(
        json!({
            "kind": "pipeline",
            "steps": [
                { "kind": "noop" },
                { "kind": "noop" },
            ],
        }),
        audit.clone(),
    )
    .await
    .expect("pipeline succeeds");
    assert_eq!(result.output["summary"]["n"], 2);
    assert_eq!(result.output["summary"]["ok_count"], 2);
    assert_eq!(result.output["summary"]["verdict"], "succeeded");
}

#[tokio::test]
async fn first_step_failure_bails_by_default() {
    let audit = Arc::new(MemoryAuditSink::new());
    let result = run_pipeline(
        json!({
            "kind": "pipeline",
            "steps": [
                { "kind": "nonexistent" },
                { "kind": "noop" },
                { "kind": "noop" },
            ],
        }),
        audit.clone(),
    )
    .await;
    let err = result.expect_err("first step fails → bail");
    let s = format!("{err:?}");
    assert!(s.contains("pipeline failed"), "got: {s}");
    // bail = only 1 step.started (the failing one); subsequent steps must not have run.
    let started: Vec<_> = audit
        .snapshot()
        .into_iter()
        .filter(|e| e.event_type == "pipeline.step.started")
        .collect();
    assert_eq!(
        started.len(),
        1,
        "bail must stop after first failure; got {} started events",
        started.len()
    );
}

#[tokio::test]
async fn on_step_failure_continue_runs_all_steps() {
    let audit = Arc::new(MemoryAuditSink::new());
    let _ = run_pipeline(
        json!({
            "kind": "pipeline",
            "steps": [
                { "kind": "noop" },
                { "kind": "nonexistent" },
                { "kind": "noop" },
            ],
            "on_step_failure": "continue",
        }),
        audit.clone(),
    )
    .await;
    let started: Vec<_> = audit
        .snapshot()
        .into_iter()
        .filter(|e| e.event_type == "pipeline.step.started")
        .collect();
    assert_eq!(started.len(), 3, "continue must drain all steps");
}

#[tokio::test]
async fn empty_steps_rejects() {
    let audit = Arc::new(MemoryAuditSink::new());
    let result = run_pipeline(
        json!({
            "kind": "pipeline",
            "steps": [],
        }),
        audit.clone(),
    )
    .await;
    let err = result.expect_err("empty steps must reject");
    let s = format!("{err:?}");
    assert!(s.contains("INVALID_PIPELINE_CONFIG"), "got: {s}");
}

#[tokio::test]
async fn missing_steps_rejects() {
    let audit = Arc::new(MemoryAuditSink::new());
    let result = run_pipeline(
        json!({
            "kind": "pipeline",
        }),
        audit.clone(),
    )
    .await;
    let err = result.expect_err("missing steps must reject");
    let s = format!("{err:?}");
    assert!(
        s.contains("INVALID_PIPELINE_CONFIG") && s.contains("steps"),
        "got: {s}"
    );
}

#[tokio::test]
async fn step_audit_events_share_parent_correlation_id() {
    let audit = Arc::new(MemoryAuditSink::new());
    let _ = run_pipeline(
        json!({
            "kind": "pipeline",
            "steps": [
                { "kind": "noop" },
                { "kind": "noop" },
            ],
        }),
        audit.clone(),
    )
    .await
    .expect("ok");
    let evs: Vec<_> = audit
        .snapshot()
        .into_iter()
        .filter(|e| e.event_type.starts_with("pipeline."))
        .collect();
    assert!(!evs.is_empty());
    for ev in &evs {
        assert_eq!(
            ev.correlation_id, "test-pipeline-corr",
            "every pipeline event must carry the parent's correlation_id"
        );
    }
}

#[tokio::test]
async fn invalid_on_step_failure_rejects() {
    let audit = Arc::new(MemoryAuditSink::new());
    let result = run_pipeline(
        json!({
            "kind": "pipeline",
            "steps": [ { "kind": "noop" } ],
            "on_step_failure": "wat",
        }),
        audit.clone(),
    )
    .await;
    let err = result.expect_err("invalid on_step_failure must reject");
    let s = format!("{err:?}");
    assert!(s.contains("INVALID_PIPELINE_CONFIG"), "got: {s}");
}

// ── config validation: unknown on_step_failure rejects (INVALID_PIPELINE_CONFIG) ──

#[tokio::test]
async fn unknown_on_step_failure_is_permanent_config_error() {
    let audit = Arc::new(MemoryAuditSink::new());
    let err = run_pipeline(
        json!({
            "kind": "pipeline",
            "steps": [ { "kind": "noop" } ],
            "on_step_failure": "bogus",
        }),
        audit,
    )
    .await
    .expect_err("an unknown on_step_failure value must be a config error");
    match err {
        ExecutorError::Permanent(msg) => {
            assert!(msg.contains("INVALID_PIPELINE_CONFIG"), "got: {msg}")
        }
        other => panic!("expected Permanent, got {other:?}"),
    }
}
