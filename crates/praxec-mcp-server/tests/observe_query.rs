//! L1 observability — the `praxec.query { observe: true }` read.
//!
//! Pins the three contract points:
//! - the discovery home ADVERTISES the observe link (HATEOAS discoverability);
//! - with a file audit sink, observe returns the structured events (with the
//!   tree-linkage fields) plus the `next_since` pull-tail cursor;
//! - with any non-file sink, observe fails FAST with the same rich
//!   `audit.sink: file` error as the CLI `observe --follow` (never an empty
//!   stream masquerading as "no activity").

use std::sync::Arc;

use praxec_core::WorkflowRuntime;
use praxec_core::audit::{AuditEvent, AuditSink, FileAuditSink, MemoryAuditSink, RotationInterval};
use praxec_core::guards::DefaultGuardEvaluator;
use praxec_core::ports::ExecutorRegistry;
use praxec_core::store::{ConfigDefinitionStore, InMemoryWorkflowStore};
use praxec_mcp_server::PraxecServer;
use rmcp::model::{CallToolRequestParams, JsonObject};
use serde_json::{Value, json};

struct NoopRegistry;
impl ExecutorRegistry for NoopRegistry {
    fn get(&self, _kind: &str) -> Option<Arc<dyn praxec_core::Executor>> {
        None
    }
}

/// Minimal server over the given audit sink (mirrors dispatch_shape.rs).
fn server_with_sink(audit: Arc<dyn AuditSink>) -> PraxecServer {
    let cfg = json!({
        "version": "1.0.0",
        "workflows": {
            "test_wf": {
                "initialState": "open",
                "states": {
                    "open": { "transitions": { "close": { "target": "done" } } },
                    "done": { "terminal": true }
                }
            }
        }
    });
    let resolved = praxec_core::config::resolve(cfg).expect("resolve");
    let defs = Arc::new(ConfigDefinitionStore::from_config(&resolved));
    let store = Arc::new(InMemoryWorkflowStore::new());
    let guards = Arc::new(DefaultGuardEvaluator::new());
    let runtime = WorkflowRuntime::new(defs, store, Arc::new(NoopRegistry), guards, audit)
        .with_writable_repo_roots(vec![praxec_core::RepoRoot::for_test()]);
    PraxecServer::new(runtime)
}

fn call(args: Value) -> CallToolRequestParams {
    let m: JsonObject = args.as_object().cloned().unwrap_or_default();
    CallToolRequestParams::new("praxec.query").with_arguments(m)
}

async fn query(server: &PraxecServer, args: Value) -> Value {
    server.dispatch_call(call(args)).await.expect("dispatch ok")
}

/// (a) — the discovery home advertises observe as a first-class HATEOAS link,
/// mirroring how search/list are surfaced.
#[tokio::test]
async fn home_advertises_the_observe_link() {
    let server = server_with_sink(Arc::new(MemoryAuditSink::new()));
    let home = query(&server, json!({})).await;
    let links = home["links"].as_array().expect("home has links");
    let observe = links
        .iter()
        .find(|l| l["rel"] == "observe")
        .unwrap_or_else(|| panic!("home must advertise the observe link; got: {links:?}"));
    assert_eq!(observe["method"], "praxec.query");
    assert_eq!(observe["args"]["observe"], true);
}

/// (b) — with a file sink, observe replays the recorded events with the
/// tree-linkage fields, honors `since`, and returns the pull-tail cursor.
#[tokio::test]
async fn observe_returns_events_from_a_file_sink() {
    let dir = tempfile::tempdir().expect("tempdir");
    let sink = Arc::new(FileAuditSink::new(dir.path(), RotationInterval::Daily));

    sink.record(
        AuditEvent::new("workflow.started")
            .with_workflow("wf_child")
            .with_topology(Some("wf_parent".into()), 1),
    )
    .await
    .expect("record");
    // Heartbeats route to their own stream and are excluded from the read.
    sink.record(AuditEvent::new("agent.heartbeat"))
        .await
        .expect("record heartbeat");

    let server = server_with_sink(sink);
    let resp = query(&server, json!({ "observe": true })).await;

    assert_eq!(resp["result"]["status"], "ok", "got: {resp}");
    let events = resp["events"].as_array().expect("events array");
    assert_eq!(events.len(), 1, "heartbeat excluded; got: {events:?}");
    let event = &events[0];
    assert_eq!(event["event_type"], "workflow.started");
    assert_eq!(event["workflow_id"], "wf_child");
    assert_eq!(
        event["parent_workflow_id"], "wf_parent",
        "tree linkage must survive the observe read"
    );
    assert_eq!(event["depth"], 1);

    // The pull-tail cursor: next_since = last event's timestamp, plus an
    // observe_next link the client can follow to poll forward.
    let cursor = resp["next_since"].as_str().expect("next_since cursor");
    let next_link = resp["links"]
        .as_array()
        .expect("links")
        .iter()
        .find(|l| l["rel"] == "observe_next")
        .expect("observe_next link");
    assert_eq!(next_link["args"]["since"].as_str(), Some(cursor));

    // A `since` floor beyond every event yields an empty (non-error) window.
    let later = query(
        &server,
        json!({ "observe": true, "since": "2099-01-01T00:00:00Z" }),
    )
    .await;
    assert_eq!(later["count"], 0, "got: {later}");
    assert_eq!(later["result"]["status"], "ok");
}

/// (c) — observe on a non-file sink returns the SAME rich fail-fast as the
/// CLI: a structured error naming `audit.sink: file` and the offending sink,
/// with recovery links (never an empty event list).
#[tokio::test]
async fn observe_fails_fast_on_non_file_sink_with_rich_error() {
    let server = server_with_sink(Arc::new(MemoryAuditSink::new()));
    let resp = query(&server, json!({ "observe": true })).await;

    assert_eq!(
        resp["error"]["code"], "OBSERVE_REQUIRES_FILE_SINK",
        "got: {resp}"
    );
    let message = resp["error"]["message"].as_str().expect("message");
    assert!(
        message.contains("audit.sink: file"),
        "message names the required sink: {message}"
    );
    assert!(
        message.contains("memory"),
        "message names the offending sink: {message}"
    );
    assert!(
        resp.get("events").is_none(),
        "a misconfigured sink must never read as an empty stream"
    );
    assert!(
        resp["links"].as_array().is_some_and(|l| !l.is_empty()),
        "structured error keeps HATEOAS recovery links"
    );
}

/// Observe is an exclusive shape: mixed with another intent field it is
/// AMBIGUOUS_INTENT, and `since` without `observe: true` is rejected rather
/// than silently ignored.
#[tokio::test]
async fn observe_shape_is_exclusive_and_since_requires_observe() {
    let server = server_with_sink(Arc::new(MemoryAuditSink::new()));

    let mixed = query(&server, json!({ "observe": true, "workflowId": "wf_1" })).await;
    assert_eq!(mixed["error"]["code"], "AMBIGUOUS_INTENT", "got: {mixed}");

    let stray_since = query(&server, json!({ "since": "2026-07-10T00:00:00Z" })).await;
    assert_eq!(
        stray_since["error"]["code"], "AMBIGUOUS_INTENT",
        "got: {stray_since}"
    );
}

/// A malformed `since` is a caller error (invalid_params), not an internal
/// failure and not an unfiltered replay.
#[tokio::test]
async fn observe_rejects_malformed_since() {
    let dir = tempfile::tempdir().expect("tempdir");
    let sink = Arc::new(FileAuditSink::new(dir.path(), RotationInterval::Daily));
    let server = server_with_sink(sink);

    let err = server
        .dispatch_call(call(json!({ "observe": true, "since": "yesterday-ish" })))
        .await
        .expect_err("malformed since must be rejected");
    assert!(
        err.to_string().contains("since"),
        "error names the bad arg: {err}"
    );
}
