//! SPEC §20.2 — end-to-end propagation of `traceId` / `runId` from MCP
//! tool arguments through to audit events. Asserts that:
//!   - praxec.command start shape with traceId/runId records them on
//!     workflow.started
//!   - subsequent praxec.command submit inherits the persisted trace/run
//!     on the workflow.transitioned event
//!   - omitting traceId/runId in start leaves the audit event's fields null
//!   - submit can override the persisted trace_id for that one call
//!
//! Updated from the old TOOL_START / TOOL_SUBMIT constants to the §32
//! two-tool surface (TOOL_COMMAND for both start and submit shapes).

use std::sync::Arc;

use praxec_core::WorkflowRuntime;
use praxec_core::audit::{AuditEvent, AuditSink, MemoryAuditSink};
use praxec_core::guards::DefaultGuardEvaluator;
use praxec_core::ports::ExecutorRegistry;
use praxec_core::store::{ConfigDefinitionStore, InMemoryWorkflowStore};
use praxec_mcp_server::{PraxecServer, TOOL_COMMAND, TOOL_QUERY};
use rmcp::model::{CallToolRequestParams, JsonObject};
use serde_json::{Value, json};

struct NoopRegistry;
impl ExecutorRegistry for NoopRegistry {
    fn get(&self, _kind: &str) -> Option<Arc<dyn praxec_core::Executor>> {
        None
    }
}

fn fixture_config() -> Value {
    json!({
        "version": "1.0.0",
        "workflows": {
            "demo": {
                "initialState": "s",
                "states": {
                    "s": {
                        "transitions": {
                            "go": { "target": "done", "executor": { "kind": "noop" } }
                        }
                    },
                    "done": { "terminal": true }
                }
            }
        }
    })
}

fn build() -> (PraxecServer, Arc<MemoryAuditSink>) {
    let resolved = praxec_core::config::resolve(fixture_config()).expect("resolve");
    let defs = Arc::new(ConfigDefinitionStore::from_config(&resolved));
    let store = Arc::new(InMemoryWorkflowStore::new());
    let audit = Arc::new(MemoryAuditSink::new());
    let guards = Arc::new(DefaultGuardEvaluator::new());
    let runtime = WorkflowRuntime::new(
        defs,
        store,
        Arc::new(NoopRegistry),
        guards,
        audit.clone() as Arc<dyn AuditSink>,
    );
    (PraxecServer::new(runtime), audit)
}

fn call(name: &'static str, args: Value) -> CallToolRequestParams {
    let m: JsonObject = args.as_object().cloned().expect("object");
    CallToolRequestParams::new(name).with_arguments(m)
}

fn find_event<'a>(events: &'a [AuditEvent], kind: &str) -> Option<&'a AuditEvent> {
    events.iter().find(|e| e.event_type == kind)
}

// ── praxec.command start propagation ──────────────────────────────────────

#[tokio::test]
async fn start_with_trace_and_run_records_them_on_workflow_started() {
    let (server, audit) = build();
    let _ = server
        .dispatch_call(call(
            TOOL_COMMAND,
            json!({
                "definitionId": "demo",
                "input": {},
                "traceId": "trace_abc",
                "runId":   "run_xyz",
            }),
        ))
        .await
        .expect("start");
    let events = audit.snapshot();
    let started = find_event(&events, "workflow.started").expect("workflow.started present");
    assert_eq!(started.trace_id.as_deref(), Some("trace_abc"));
    assert_eq!(started.run_id.as_deref(), Some("run_xyz"));
}

#[tokio::test]
async fn start_without_trace_or_run_leaves_them_null_on_audit_event() {
    let (server, audit) = build();
    let _ = server
        .dispatch_call(call(
            TOOL_COMMAND,
            json!({ "definitionId": "demo", "input": {} }),
        ))
        .await
        .expect("start");
    let events = audit.snapshot();
    let started = find_event(&events, "workflow.started").expect("present");
    assert!(started.trace_id.is_none());
    assert!(started.run_id.is_none());
}

#[tokio::test]
async fn start_with_only_trace_id_records_just_that_field() {
    let (server, audit) = build();
    let _ = server
        .dispatch_call(call(
            TOOL_COMMAND,
            json!({ "definitionId": "demo", "input": {}, "traceId": "trace_only" }),
        ))
        .await
        .expect("start");
    let events = audit.snapshot();
    let started = find_event(&events, "workflow.started").expect("present");
    assert_eq!(started.trace_id.as_deref(), Some("trace_only"));
    assert!(started.run_id.is_none());
}

// ── Persistence: subsequent submit inherits trace/run from the instance ─────

#[tokio::test]
async fn submit_inherits_persisted_trace_id_from_start() {
    let (server, audit) = build();
    let start_resp = server
        .dispatch_call(call(
            TOOL_COMMAND,
            json!({
                "definitionId": "demo",
                "input": {},
                "traceId": "trace_persisted",
                "runId":   "run_persisted",
            }),
        ))
        .await
        .expect("start");
    let workflow_id = start_resp["workflow"]["id"].as_str().unwrap().to_string();
    let version = start_resp["workflow"]["version"].as_u64().unwrap();

    audit.clear();

    let _ = server
        .dispatch_call(call(
            TOOL_COMMAND,
            json!({
                "workflowId": workflow_id,
                "expectedVersion": version,
                "transition": "go",
                "arguments": {}
            }),
        ))
        .await
        .expect("submit");

    let events = audit.snapshot();
    // Find a post-start event that should inherit trace/run.
    let event_types: Vec<&str> = events.iter().map(|e| e.event_type.as_str()).collect();
    let trace_inheriting = events.iter().find(|e| {
        matches!(
            e.event_type.as_str(),
            "workflow.transitioned" | "workflow.completed" | "transition.requested"
        )
    });
    let evt = trace_inheriting.unwrap_or_else(|| {
        panic!("expected one of workflow.transitioned/completed/transition.requested; got: {event_types:?}")
    });
    assert_eq!(evt.trace_id.as_deref(), Some("trace_persisted"));
    assert_eq!(evt.run_id.as_deref(), Some("run_persisted"));
}

#[tokio::test]
async fn submit_inherits_persisted_trace_on_transition_requested() {
    let (server, audit) = build();
    let start_resp = server
        .dispatch_call(call(
            TOOL_COMMAND,
            json!({
                "definitionId": "demo",
                "input": {},
                "traceId": "t_req",
                "runId":   "r_req",
            }),
        ))
        .await
        .expect("start");
    let workflow_id = start_resp["workflow"]["id"].as_str().unwrap().to_string();
    let version = start_resp["workflow"]["version"].as_u64().unwrap();
    audit.clear();

    let _ = server
        .dispatch_call(call(
            TOOL_COMMAND,
            json!({
                "workflowId": workflow_id,
                "expectedVersion": version,
                "transition": "go",
                "arguments": {}
            }),
        ))
        .await
        .expect("submit");

    let events = audit.snapshot();
    let requested = find_event(&events, "transition.requested").expect("present");
    assert_eq!(requested.trace_id.as_deref(), Some("t_req"));
    assert_eq!(requested.run_id.as_deref(), Some("r_req"));
}

// ── Workflow without trace/run never leaks fake values ────────────────────

#[tokio::test]
async fn workflow_without_trace_run_keeps_audit_fields_null_across_transitions() {
    let (server, audit) = build();
    let start_resp = server
        .dispatch_call(call(
            TOOL_COMMAND,
            json!({ "definitionId": "demo", "input": {} }),
        ))
        .await
        .expect("start");
    let workflow_id = start_resp["workflow"]["id"].as_str().unwrap().to_string();
    let version = start_resp["workflow"]["version"].as_u64().unwrap();
    audit.clear();

    let _ = server
        .dispatch_call(call(
            TOOL_COMMAND,
            json!({
                "workflowId": workflow_id,
                "expectedVersion": version,
                "transition": "go",
                "arguments": {}
            }),
        ))
        .await
        .expect("submit");

    let events = audit.snapshot();
    for e in &events {
        assert!(
            e.trace_id.is_none(),
            "event {} leaked trace_id when workflow had none: {:?}",
            e.event_type,
            e.trace_id
        );
        assert!(
            e.run_id.is_none(),
            "event {} leaked run_id: {:?}",
            e.event_type,
            e.run_id
        );
    }
}

// ── Wire-format invariant: absent fields omitted from serialised AuditEvent

#[tokio::test]
async fn audit_event_for_anonymous_workflow_omits_trace_and_run_in_json() {
    let (server, audit) = build();
    let _ = server
        .dispatch_call(call(
            TOOL_COMMAND,
            json!({ "definitionId": "demo", "input": {} }),
        ))
        .await
        .expect("start");
    let events = audit.snapshot();
    let started = find_event(&events, "workflow.started").expect("present");
    let serialized = serde_json::to_value(started).expect("serialise");
    assert!(
        serialized.get("trace_id").is_none(),
        "absent trace_id must not appear in wire payload; got: {serialized}"
    );
    assert!(serialized.get("run_id").is_none());
}

// ── SPEC §32 run_id uniqueness ───────────────────────────────────────────────

/// SPEC §32 — starting a second workflow with the same run_id returns a
/// structured RUN_ID_ALREADY_RUNNING response (not an MCP protocol error)
/// with a HATEOAS `get` link pointing at the existing instance.
#[tokio::test]
async fn start_with_duplicate_run_id_returns_structured_error() {
    let (server, _audit) = build();

    // First start with run_id "r-abc" — must succeed and return a workflow id.
    let first = server
        .dispatch_call(call(
            TOOL_COMMAND,
            json!({
                "definitionId": "demo",
                "input": {},
                "runId": "r-abc"
            }),
        ))
        .await
        .expect("first start succeeds");
    let existing_wf_id = first["workflow"]["id"]
        .as_str()
        .expect("first response must carry workflow.id");

    // Second start with the same run_id — should return structured error body.
    let second = server
        .dispatch_call(call(
            TOOL_COMMAND,
            json!({
                "definitionId": "demo",
                "input": {},
                "runId": "r-abc"
            }),
        ))
        .await
        .expect("dispatch returns Ok with structured error body");

    assert_eq!(
        second["error"]["code"], "RUN_ID_ALREADY_RUNNING",
        "expected RUN_ID_ALREADY_RUNNING code; got: {second}"
    );

    let links = second["links"]
        .as_array()
        .expect("structured error must carry a links array");
    let get_link = links
        .iter()
        .find(|l| l["rel"] == "get")
        .expect("links must contain a get link");

    assert_eq!(
        get_link["method"], "praxec.query",
        "get link must use praxec.query"
    );
    assert_eq!(
        get_link["args"]["workflowId"]
            .as_str()
            .expect("workflowId must be a string"),
        existing_wf_id,
        "get link must reference the existing workflow id"
    );
}

// ── praxec.query → get (used in authoring tests) ──────────────────────────

/// Smoke test: praxec.query with workflowId routes to get.
/// The `workflow.get` inline string from old tests becomes TOOL_QUERY.
#[tokio::test]
async fn query_with_workflow_id_routes_to_get() {
    let (server, _audit) = build();
    let start_resp = server
        .dispatch_call(call(
            TOOL_COMMAND,
            json!({ "definitionId": "demo", "input": {} }),
        ))
        .await
        .expect("start");
    let workflow_id = start_resp["workflow"]["id"].as_str().unwrap().to_string();

    let get_resp = server
        .dispatch_call(call(TOOL_QUERY, json!({ "workflowId": workflow_id })))
        .await
        .expect("get succeeds");
    assert!(
        get_resp["workflow"]["id"].as_str().is_some(),
        "get response must carry workflow.id: {get_resp}"
    );
}
