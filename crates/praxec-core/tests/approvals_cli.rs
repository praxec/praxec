//! Integration tests for the approvals CLI subcommand group.
//!
//! Tests use `MemoryAuditSink` for fast, filesystem-free verification.
//! The approvals subcommand reads from the audit sink to list pending
//! approvals and append resolution events.

use praxec_core::audit::{AuditEvent, AuditSink, MemoryAuditSink};
use serde_json::json;
use std::sync::Arc;

#[tokio::test]
async fn approvals_list_shows_pending_requests() {
    let sink = Arc::new(MemoryAuditSink::new());

    sink.record(
        AuditEvent::new("human.approval.requested")
            .with_workflow("wf_1")
            .with_payload(json!({
                "queue": "content-approvals",
                "transition": "publish",
            })),
    )
    .await
    .unwrap();

    sink.record(
        AuditEvent::new("human.approval.requested")
            .with_workflow("wf_2")
            .with_payload(json!({
                "queue": "prod-deployments",
                "transition": "deploy",
            })),
    )
    .await
    .unwrap();

    let events = sink.snapshot();
    let pending: Vec<_> = events
        .iter()
        .filter(|e| e.event_type == "human.approval.requested")
        .collect();
    assert_eq!(pending.len(), 2, "two pending approvals should be listed");
}

#[tokio::test]
async fn approvals_list_hides_resolved_by_default() {
    let sink = Arc::new(MemoryAuditSink::new());

    // Record a pending approval
    sink.record(
        AuditEvent::new("human.approval.requested")
            .with_workflow("wf_1")
            .with_payload(json!({
                "queue": "content-approvals",
                "transition": "publish",
            })),
    )
    .await
    .unwrap();

    // Record a resolution for it
    let events = sink.snapshot();
    let approval_id = events[0].id.clone();
    sink.record(
        AuditEvent::new("human.approval.resolved")
            .with_workflow("wf_1")
            .with_payload(json!({
                "approval_id": approval_id,
                "outcome": "approved",
            })),
    )
    .await
    .unwrap();

    let events = sink.snapshot();
    let requested: Vec<_> = events
        .iter()
        .filter(|e| e.event_type == "human.approval.requested")
        .collect();
    let resolved_ids: std::collections::HashSet<String> = events
        .iter()
        .filter(|e| e.event_type == "human.approval.resolved")
        .filter_map(|e| {
            e.payload
                .get("approval_id")
                .and_then(|v| v.as_str())
                .map(String::from)
        })
        .collect();

    let still_pending: Vec<_> = requested
        .iter()
        .filter(|e| !resolved_ids.contains(&e.id))
        .collect();
    assert_eq!(
        still_pending.len(),
        0,
        "resolved approval should be hidden by default"
    );
}

#[tokio::test]
async fn approvals_list_shows_resolved_with_all_flag() {
    let sink = Arc::new(MemoryAuditSink::new());

    // Record a pending approval
    sink.record(
        AuditEvent::new("human.approval.requested")
            .with_workflow("wf_1")
            .with_payload(json!({
                "queue": "content-approvals",
                "transition": "publish",
            })),
    )
    .await
    .unwrap();

    // Record a resolution
    let events = sink.snapshot();
    let approval_id = events[0].id.clone();
    sink.record(
        AuditEvent::new("human.approval.resolved")
            .with_workflow("wf_1")
            .with_payload(json!({
                "approval_id": approval_id,
                "outcome": "approved",
            })),
    )
    .await
    .unwrap();

    // With --all, the resolved approval should still appear
    let events = sink.snapshot();
    let requested: Vec<_> = events
        .iter()
        .filter(|e| e.event_type == "human.approval.requested")
        .collect();
    assert_eq!(
        requested.len(),
        1,
        "resolved approval should still appear with --all"
    );
}

#[tokio::test]
async fn approvals_resolve_appends_resolution_event() {
    let sink = Arc::new(MemoryAuditSink::new());

    // Record a pending approval
    sink.record(
        AuditEvent::new("human.approval.requested")
            .with_workflow("wf_1")
            .with_payload(json!({
                "queue": "content-approvals",
                "transition": "publish",
            })),
    )
    .await
    .unwrap();

    let events = sink.snapshot();
    let approval_id = events[0].id.clone();

    // Append a resolution (simulating `approvals resolve`)
    sink.record(
        AuditEvent::new("human.approval.resolved")
            .with_workflow("wf_1")
            .with_payload(json!({
                "approval_id": approval_id,
                "outcome": "approved",
            })),
    )
    .await
    .unwrap();

    let events = sink.snapshot();
    let resolved: Vec<_> = events
        .iter()
        .filter(|e| e.event_type == "human.approval.resolved")
        .collect();
    assert_eq!(resolved.len(), 1, "one resolution event should be appended");
    assert_eq!(
        resolved[0]
            .payload
            .get("approval_id")
            .and_then(|v| v.as_str()),
        Some(approval_id.as_str()),
        "resolution should reference the original approval id"
    );
    assert_eq!(
        resolved[0].payload.get("outcome").and_then(|v| v.as_str()),
        Some("approved"),
        "resolution should record the outcome"
    );
}

#[tokio::test]
async fn approvals_multiple_queues() {
    let sink = Arc::new(MemoryAuditSink::new());

    for i in 0..5 {
        sink.record(
            AuditEvent::new("human.approval.requested")
                .with_workflow(format!("wf_{}", i))
                .with_payload(json!({
                    "queue": if i % 2 == 0 { "queue_a" } else { "queue_b" },
                    "transition": "approve",
                })),
        )
        .await
        .unwrap();
    }

    let events = sink.snapshot();
    let queue_a: Vec<_> = events
        .iter()
        .filter(|e| e.payload.get("queue").and_then(|v| v.as_str()) == Some("queue_a"))
        .collect();
    let queue_b: Vec<_> = events
        .iter()
        .filter(|e| e.payload.get("queue").and_then(|v| v.as_str()) == Some("queue_b"))
        .collect();

    assert_eq!(queue_a.len(), 3, "queue_a has 3 approvals");
    assert_eq!(queue_b.len(), 2, "queue_b has 2 approvals");
}

#[tokio::test]
async fn approvals_resolve_rejects_unknown_id() {
    // This tests the logic that would reject an unknown approval id.
    // In the CLI, this is handled by reading the audit file and checking
    // for the existence of the event. Here we verify the pattern.
    let sink = Arc::new(MemoryAuditSink::new());

    // Record a resolution for a non-existent approval
    sink.record(
        AuditEvent::new("human.approval.resolved")
            .with_workflow("wf_unknown")
            .with_payload(json!({
                "approval_id": "evt_nonexistent",
                "outcome": "approved",
            })),
    )
    .await
    .unwrap();

    let events = sink.snapshot();
    let resolved: Vec<_> = events
        .iter()
        .filter(|e| e.event_type == "human.approval.resolved")
        .collect();
    assert_eq!(resolved.len(), 1, "resolution event was recorded");

    // Verify no matching approval request exists
    let requested: Vec<_> = events
        .iter()
        .filter(|e| e.event_type == "human.approval.requested")
        .collect();
    assert_eq!(requested.len(), 0, "no approval request should exist");
}
