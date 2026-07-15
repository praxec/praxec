//! SPEC §5.8 — `praxec.query` with `subject` (describe shape) emits a
//! `guidance.describe_requested` audit record. Non-critical-path: sink
//! failures do not abort the describe (the body is already fetched and
//! returned), but they emit an `audit.write_failed` self-event so loss is
//! observable.
//!
//! Each assertion targets one observable property of the audit shape.
//!
//! Updated from the old TOOL_DESCRIBE constant to the §32 surface
//! (TOOL_QUERY with subject arg → describe shape).

use std::sync::Arc;

use praxec_core::WorkflowRuntime;
use praxec_core::audit::{AuditEvent, AuditSink, MemoryAuditSink};
use praxec_core::discovery::{DiscoveryItem, DiscoveryKind, DiscoveryLink, InMemoryDiscoveryIndex};
use praxec_core::guards::DefaultGuardEvaluator;
use praxec_core::ports::ExecutorRegistry;
use praxec_core::store::{ConfigDefinitionStore, InMemoryWorkflowStore};
use praxec_mcp_server::{PraxecServer, TOOL_QUERY};
use rmcp::model::{CallToolRequestParams, JsonObject};
use serde_json::{Value, json};

struct NoopRegistry;
impl ExecutorRegistry for NoopRegistry {
    fn get(&self, _kind: &str) -> Option<Arc<dyn praxec_core::Executor>> {
        None
    }
}

fn build_runtime_with_audit() -> (WorkflowRuntime, Arc<MemoryAuditSink>) {
    let audit = Arc::new(MemoryAuditSink::new());
    let runtime = WorkflowRuntime::new(
        Arc::new(ConfigDefinitionStore::default()),
        Arc::new(InMemoryWorkflowStore::default()),
        Arc::new(NoopRegistry),
        Arc::new(DefaultGuardEvaluator::new()),
        audit.clone() as Arc<dyn AuditSink>,
    )
    .with_writable_repo_roots(vec![praxec_core::RepoRoot::for_test()]);
    (runtime, audit)
}

fn build_discovery_with_one_skill() -> Arc<InMemoryDiscoveryIndex> {
    Arc::new(InMemoryDiscoveryIndex::new(vec![DiscoveryItem {
        id: "review.style.house-voice".to_string(),
        kind: DiscoveryKind::Guidance,
        title: "House voice".to_string(),
        description: String::new(),
        tags: vec![],
        examples: vec![],
        aliases: vec![],
        text: String::new(),
        links: vec![DiscoveryLink {
            rel: "home".to_string(),
            title: None,
            description: None,
            method: "praxec.query".to_string(),
            args: json!({}),
            input_schema: None,
        }],
        verb: Some("review".to_string()),
        body: Some("Lead with the reader's problem.".to_string()),
        source: Some("config".to_string()),
        structural_fingerprint: None,
    }]))
}

fn build_server() -> (PraxecServer, Arc<MemoryAuditSink>) {
    let (runtime, audit) = build_runtime_with_audit();
    let server = PraxecServer::new(runtime).with_discovery(build_discovery_with_one_skill());
    (server, audit)
}

/// Build a describe call using praxec.query with subject arg (§32).
fn describe_call(subject: &str) -> CallToolRequestParams {
    let map: JsonObject = json!({ "subject": subject })
        .as_object()
        .cloned()
        .expect("object");
    CallToolRequestParams::new(TOOL_QUERY).with_arguments(map)
}

fn find_event(audit: &MemoryAuditSink, event_type: &str) -> Option<AuditEvent> {
    audit
        .snapshot()
        .into_iter()
        .find(|e| e.event_type == event_type)
}

// ── Positive: a successful describe emits the audit record ──────────────────

#[tokio::test]
async fn describe_emits_guidance_describe_requested_event() {
    let (server, audit) = build_server();
    let _ = server
        .dispatch_call(describe_call("review.style.house-voice"))
        .await
        .expect("describe succeeds");
    assert!(
        find_event(&audit, "guidance.describe_requested").is_some(),
        "describe must emit guidance.describe_requested; recorded: {:?}",
        audit.event_types()
    );
}

// ── Positive: the record carries `subject` ─────────────────────────────────

#[tokio::test]
async fn describe_record_carries_subject() {
    let (server, audit) = build_server();
    let _ = server
        .dispatch_call(describe_call("review.style.house-voice"))
        .await
        .expect("describe succeeds");
    let event = find_event(&audit, "guidance.describe_requested").expect("event present");
    assert_eq!(
        event.payload.get("subject").and_then(Value::as_str),
        Some("review.style.house-voice")
    );
}

// ── Positive: the record carries `verb` from the resolved fragment ─────────

#[tokio::test]
async fn describe_record_carries_verb() {
    let (server, audit) = build_server();
    let _ = server
        .dispatch_call(describe_call("review.style.house-voice"))
        .await
        .expect("describe succeeds");
    let event = find_event(&audit, "guidance.describe_requested").expect("event present");
    assert_eq!(
        event.payload.get("verb").and_then(Value::as_str),
        Some("review")
    );
}

// ── Positive: outcome=ok on success ────────────────────────────────────────

#[tokio::test]
async fn describe_record_outcome_ok_on_success() {
    let (server, audit) = build_server();
    let _ = server
        .dispatch_call(describe_call("review.style.house-voice"))
        .await
        .expect("describe succeeds");
    let event = find_event(&audit, "guidance.describe_requested").expect("event present");
    assert_eq!(
        event.payload.get("outcome").and_then(Value::as_str),
        Some("ok")
    );
}

// ── Positive: errorCode=null on success ────────────────────────────────────

#[tokio::test]
async fn describe_record_error_code_null_on_success() {
    let (server, audit) = build_server();
    let _ = server
        .dispatch_call(describe_call("review.style.house-voice"))
        .await
        .expect("describe succeeds");
    let event = find_event(&audit, "guidance.describe_requested").expect("event present");
    assert!(
        event
            .payload
            .get("errorCode")
            .map(Value::is_null)
            .unwrap_or(true),
        "errorCode must be null on success; got: {:?}",
        event.payload.get("errorCode")
    );
}

// ── Edge: describe with workflowId omitted records workflowId: null ─────────

#[tokio::test]
async fn describe_without_workflow_id_records_null_workflow_id() {
    let (server, audit) = build_server();
    let _ = server
        .dispatch_call(describe_call("review.style.house-voice"))
        .await
        .expect("describe succeeds");
    let event = find_event(&audit, "guidance.describe_requested").expect("event present");
    // The payload includes workflowId: null when caller omits it. Absence
    // would imply the field was dropped — the test guards against that.
    assert!(
        event.payload.get("workflowId").is_some(),
        "workflowId key must be present in payload (null is OK; absent is not)"
    );
    assert!(
        event
            .payload
            .get("workflowId")
            .map(Value::is_null)
            .unwrap_or(false),
        "workflowId must be explicitly null when caller didn't provide one"
    );
}

// ── Edge: concurrent describes of the same subject produce N records ───────

#[tokio::test]
async fn concurrent_describes_produce_distinct_records() {
    let (server, audit) = build_server();
    let s1 = server.clone();
    let s2 = server.clone();
    let h1 = tokio::spawn(async move {
        s1.dispatch_call(describe_call("review.style.house-voice"))
            .await
            .unwrap()
    });
    let h2 = tokio::spawn(async move {
        s2.dispatch_call(describe_call("review.style.house-voice"))
            .await
            .unwrap()
    });
    let _ = h1.await.unwrap();
    let _ = h2.await.unwrap();
    let count = audit
        .snapshot()
        .iter()
        .filter(|e| e.event_type == "guidance.describe_requested")
        .count();
    assert_eq!(count, 2, "expected 2 describe records; got: {count}");
}

// ── Negative: describe of unknown subject still records an audit event ──────

#[tokio::test]
async fn describe_unknown_subject_still_emits_record() {
    let (server, audit) = build_server();
    let _ = server
        .dispatch_call(describe_call("review.style.nonexistent"))
        .await
        .expect("describe of missing subject still succeeds (returns null item)");
    // The describe handler returns `{id, item: null, links: ...}` for an
    // unknown subject (non-guidance flow). The audit event MUST still be
    // emitted so the request itself is observable.
    assert!(
        find_event(&audit, "guidance.describe_requested").is_some(),
        "audit must record the request even when the subject doesn't resolve"
    );
}
