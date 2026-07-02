//! Tests for inspect and audit subcommands.

use praxec_core::audit::{AuditEvent, AuditSink, MemoryAuditSink};
use serde_json::json;

#[tokio::test]
async fn audit_tail_filters_by_event_type() {
    let sink = MemoryAuditSink::new();

    sink.record(
        AuditEvent::new("workflow.started")
            .with_workflow("wf_1")
            .with_payload(json!({"definition": "demo"})),
    )
    .await
    .unwrap();

    sink.record(
        AuditEvent::new("human.approval.requested")
            .with_workflow("wf_1")
            .with_payload(json!({"queue": "prod"})),
    )
    .await
    .unwrap();

    sink.record(
        AuditEvent::new("workflow.completed")
            .with_workflow("wf_1")
            .with_payload(json!({"state": "done"})),
    )
    .await
    .unwrap();

    let events = sink.snapshot();
    let approval_events: Vec<_> = events
        .iter()
        .filter(|e| e.event_type == "human.approval.requested")
        .collect();
    assert_eq!(approval_events.len(), 1);
    assert_eq!(
        approval_events[0]
            .payload
            .get("queue")
            .and_then(|v| v.as_str()),
        Some("prod")
    );
}

#[tokio::test]
async fn inspect_workflow_shows_state() {
    use praxec_core::model::WorkflowInstance;
    use praxec_core::ports::WorkflowStore;
    use praxec_core::store::InMemoryWorkflowStore;

    let store = InMemoryWorkflowStore::new();
    let instance = WorkflowInstance {
        id: "wf_test".into(),
        definition_id: "demo".into(),
        definition_version: "1.0.0".into(),
        definition: json!({"initialState": "running", "states": {}}),
        state: "running".into(),
        version: 3,
        input: json!({"key": "value"}),
        context: json!({"count": 42}),
        started_at: chrono::Utc::now(),
        trace_id: None,
        run_id: None,
        cancelled_at: None,
        cancelled_reason: None,
        depth: 0,
        parent: None,
    };

    store.create(instance).await.unwrap();
    let loaded = store.load("wf_test").await.unwrap();
    assert_eq!(loaded.state, "running");
    assert_eq!(loaded.version, 3);
    assert_eq!(
        loaded.context.get("count").and_then(|v| v.as_u64()),
        Some(42)
    );
}
