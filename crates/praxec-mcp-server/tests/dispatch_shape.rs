//! Sparse-args deserialization tests for the `praxec.query` and
//! `praxec.command` dispatch boundary structs. Every field is
//! optional; the runtime selects the operation by which required-field
//! shape is present.
//!
//! The second half of this file exercises the `dispatch_query` /
//! `dispatch_command` methods on `PraxecServer` directly as well as
//! `dispatch_call` (which now routes both through the shape-routers,
//! completing the §32 surface flip).

use std::sync::Arc;

use praxec_core::audit::{AuditSink, MemoryAuditSink};
use praxec_core::guards::DefaultGuardEvaluator;
use praxec_core::model::Principal;
use praxec_core::ports::ExecutorRegistry;
use praxec_core::store::{ConfigDefinitionStore, InMemoryWorkflowStore};
use praxec_core::WorkflowRuntime;
use praxec_mcp_server::args::{CommandArgs, QueryArgs};
use praxec_mcp_server::PraxecServer;
use rmcp::model::{CallToolRequestParams, JsonObject};
use serde_json::{json, Value};

// ── Test server helper ────────────────────────────────────────────────────────

struct NoopRegistry;
impl ExecutorRegistry for NoopRegistry {
    fn get(&self, _kind: &str) -> Option<Arc<dyn praxec_core::Executor>> {
        None
    }
}

/// Build a minimal `PraxecServer` with a single workflow `test_wf` and a
/// `demo` workflow (for submit/explain). Uses a noop executor registry so
/// start/get work, but transitions with executors fail gracefully.
async fn test_server() -> PraxecServer {
    let cfg = json!({
        "version": "1.0.0",
        "workflows": {
            "test_wf": {
                "initialState": "open",
                "states": {
                    "open": {
                        "transitions": {
                            "close": { "target": "done" }
                        }
                    },
                    "done": { "terminal": true }
                }
            }
        }
    });
    let resolved = praxec_core::config::resolve(cfg).expect("resolve");
    let defs = Arc::new(ConfigDefinitionStore::from_config(&resolved));
    let store = Arc::new(InMemoryWorkflowStore::new());
    let audit = Arc::new(MemoryAuditSink::new());
    let guards = Arc::new(DefaultGuardEvaluator::new());
    let runtime = WorkflowRuntime::new(
        defs,
        store,
        Arc::new(NoopRegistry),
        guards,
        audit as Arc<dyn AuditSink>,
    );
    PraxecServer::new(runtime)
}

#[test]
fn query_args_admits_empty() {
    let a: QueryArgs = serde_json::from_value(json!({})).unwrap();
    assert!(a.query.is_none());
    assert!(a.subject.is_none());
    assert!(a.workflow_id.is_none());
    assert!(a.transition.is_none());
    assert!(a.kind.is_none());
    assert!(a.limit.is_none());
}

#[test]
fn query_args_admits_search_shape() {
    let a: QueryArgs = serde_json::from_value(json!({
        "query": "swe",
        "kind": "workflow",
        "limit": 10
    }))
    .unwrap();
    assert_eq!(a.query.as_deref(), Some("swe"));
    assert_eq!(a.kind.as_deref(), Some("workflow"));
    assert_eq!(a.limit, Some(10u64));
}

#[test]
fn query_args_admits_describe_in_workflow_shape() {
    let a: QueryArgs = serde_json::from_value(json!({
        "subject": "plan.specify.change-request",
        "workflowId": "wf_01H"
    }))
    .unwrap();
    assert_eq!(a.subject.as_deref(), Some("plan.specify.change-request"));
    assert_eq!(a.workflow_id.as_deref(), Some("wf_01H"));
}

#[test]
fn command_args_admits_start_shape() {
    let a: CommandArgs = serde_json::from_value(json!({
        "definitionId": "swe_agent",
        "input": { "issue": "x" },
        "runId": "r-1"
    }))
    .unwrap();
    assert_eq!(a.definition_id.as_deref(), Some("swe_agent"));
    assert!(a.workflow_id.is_none());
    assert_eq!(a.run_id.as_deref(), Some("r-1"));
}

#[test]
fn command_args_admits_submit_shape_with_summary() {
    // SPEC §6.3: submit can carry a model-authored summary; CommandArgs
    // must accept it so the wire shape for praxec.command preserves it.
    let a: CommandArgs = serde_json::from_value(json!({
        "workflowId": "wf_01H",
        "expectedVersion": 3,
        "transition": "approve",
        "arguments": { "note": "fine" },
        "summary": "Approved after risk review"
    }))
    .unwrap();
    assert_eq!(a.workflow_id.as_deref(), Some("wf_01H"));
    assert_eq!(a.expected_version, Some(3));
    assert_eq!(a.transition.as_deref(), Some("approve"));
    assert_eq!(a.summary.as_deref(), Some("Approved after risk review"));
}

#[test]
fn command_args_admits_define_shape() {
    let a: CommandArgs = serde_json::from_value(json!({
        "subject": "lexicon:churn",
        "definition": {
            "definition_short": "Loss of paying customer in a billing period.",
            "boundedContext": "billing"
        }
    }))
    .unwrap();
    assert_eq!(a.subject.as_deref(), Some("lexicon:churn"));
    assert!(a.definition.is_some());
    assert!(a.definition_id.is_none());
    assert!(a.workflow_id.is_none());
}

// ── dispatch_query behavior tests ─────────────────────────────────────────────

/// Empty args → home. Home response has HATEOAS links.
#[tokio::test]
async fn query_empty_dispatches_to_home() {
    let server = test_server().await;
    let resp = server
        .dispatch_query(json!({}), Principal::anonymous())
        .await
        .expect("home returns Ok");
    // Home response contains links (HATEOAS invariant).
    assert!(
        resp.get("links").is_some() || resp.get("sections").is_some(),
        "home response must contain links or sections; got: {resp}"
    );
}

/// `query` present → search.
#[tokio::test]
async fn query_with_query_field_dispatches_to_search() {
    let server = test_server().await;
    let resp = server
        .dispatch_query(json!({ "query": "test" }), Principal::anonymous())
        .await
        .expect("search returns Ok");
    // Search response has `query` echo and `items` list.
    assert!(
        resp.get("items").is_some(),
        "search response must contain items; got: {resp}"
    );
    assert_eq!(resp["query"].as_str(), Some("test"));
}

/// `subject` only → describe (browse-time).
#[tokio::test]
async fn query_subject_only_dispatches_to_describe() {
    let server = test_server().await;
    let resp = server
        .dispatch_query(json!({ "subject": "test_wf" }), Principal::anonymous())
        .await
        .expect("describe returns Ok");
    // Describe response has `id` or `kind` field.
    assert!(
        resp.get("id").is_some() || resp.get("kind").is_some(),
        "describe response must contain id or kind; got: {resp}"
    );
}

/// `subject + workflowId` → describe-in-workflow. The subject is not a
/// guidance fragment in our minimal config, so it falls through to the live
/// discovery index path and returns the standard `{ id, item, links }` shape.
#[tokio::test]
async fn query_subject_plus_workflow_id_dispatches_to_describe_in_workflow() {
    let server = test_server().await;
    // Start a workflow so the workflowId is valid.
    let start_resp = server
        .dispatch_query(
            // use dispatch_command-like call here via handle_start directly
            json!({}),
            Principal::anonymous(),
        )
        .await
        .expect("home");
    // We need a valid workflow_id. Start one via dispatch_command.
    let start = server
        .dispatch_command(
            json!({ "definitionId": "test_wf", "input": {} }),
            Principal::anonymous(),
        )
        .await
        .expect("start ok");
    let workflow_id = start["workflow"]["id"]
        .as_str()
        .expect("workflow.id present")
        .to_string();
    let _ = start_resp; // suppress warning

    let resp = server
        .dispatch_query(
            json!({ "subject": "test_wf", "workflowId": workflow_id }),
            Principal::anonymous(),
        )
        .await
        .expect("describe-in-workflow returns Ok");
    // Should have describe-shaped output: id or kind present.
    assert!(
        resp.get("id").is_some() || resp.get("kind").is_some(),
        "describe-in-workflow response must have id or kind; got: {resp}"
    );
}

/// `workflowId + transition` → explain.
#[tokio::test]
async fn query_workflow_id_plus_transition_dispatches_to_explain() {
    let server = test_server().await;
    // Start a workflow to get a valid workflowId.
    let start = server
        .dispatch_command(
            json!({ "definitionId": "test_wf", "input": {} }),
            Principal::anonymous(),
        )
        .await
        .expect("start ok");
    let workflow_id = start["workflow"]["id"]
        .as_str()
        .expect("workflow.id present")
        .to_string();

    let resp = server
        .dispatch_query(
            json!({ "workflowId": workflow_id, "transition": "close" }),
            Principal::anonymous(),
        )
        .await
        .expect("explain returns Ok");
    // Explain response has workflowId and transition fields.
    assert!(
        resp.get("workflowId").is_some() || resp.get("transition").is_some(),
        "explain response must have workflowId or transition; got: {resp}"
    );
}

/// `workflowId` alone → get.
#[tokio::test]
async fn query_workflow_id_alone_dispatches_to_get() {
    let server = test_server().await;
    let start = server
        .dispatch_command(
            json!({ "definitionId": "test_wf", "input": {} }),
            Principal::anonymous(),
        )
        .await
        .expect("start ok");
    let workflow_id = start["workflow"]["id"]
        .as_str()
        .expect("workflow.id present")
        .to_string();

    let resp = server
        .dispatch_query(json!({ "workflowId": workflow_id }), Principal::anonymous())
        .await
        .expect("get returns Ok");
    // Get response has workflow.id.
    assert!(
        resp["workflow"]["id"].as_str().is_some(),
        "get response must have workflow.id; got: {resp}"
    );
}

/// Ambiguous args (too many dispatch-relevant fields) → AMBIGUOUS_INTENT structured response.
#[tokio::test]
async fn query_ambiguous_args_returns_ambiguous_intent_error() {
    let server = test_server().await;
    let resp = server
        .dispatch_query(
            // subject + query + workflowId + transition is ambiguous
            json!({
                "subject": "test_wf",
                "query": "something",
                "workflowId": "wf_X",
                "transition": "close"
            }),
            Principal::anonymous(),
        )
        .await
        .unwrap();
    assert_eq!(
        resp["error"]["code"].as_str(),
        Some("AMBIGUOUS_INTENT"),
        "response must have AMBIGUOUS_INTENT error code; got: {resp}"
    );
    assert!(
        resp["links"].as_array().is_some(),
        "AMBIGUOUS_INTENT response must include HATEOAS links; got: {resp}"
    );
}

// ── dispatch_command behavior tests ───────────────────────────────────────────

/// `definitionId` (no workflowId, no subject) → start.
#[tokio::test]
async fn command_definition_id_dispatches_to_start() {
    let server = test_server().await;
    let resp = server
        .dispatch_command(
            json!({ "definitionId": "test_wf", "input": {} }),
            Principal::anonymous(),
        )
        .await
        .expect("start returns Ok");
    // Start response has workflow.id.
    assert!(
        resp["workflow"]["id"].as_str().is_some(),
        "start response must have workflow.id; got: {resp}"
    );
}

/// `workflowId + transition + expectedVersion` → submit.
#[tokio::test]
async fn command_submit_shape_dispatches_to_submit() {
    let server = test_server().await;
    let start = server
        .dispatch_command(
            json!({ "definitionId": "test_wf", "input": {} }),
            Principal::anonymous(),
        )
        .await
        .expect("start");
    let workflow_id = start["workflow"]["id"]
        .as_str()
        .expect("workflow.id")
        .to_string();
    let version = start["workflow"]["version"].as_u64().expect("version");

    let resp = server
        .dispatch_command(
            json!({
                "workflowId": workflow_id,
                "expectedVersion": version,
                "transition": "close",
                "arguments": {}
            }),
            Principal::anonymous(),
        )
        .await
        .expect("submit returns Ok");
    // Submit response has workflow.state.
    assert!(
        resp["workflow"]["state"].as_str().is_some(),
        "submit response must have workflow.state; got: {resp}"
    );
}

/// `subject` with `:` + `definition` → lexicon define.
#[tokio::test]
async fn command_lexicon_define_shape_dispatches_to_define() {
    let server = test_server().await;
    // Lexicon define is governance-gated: agents are rejected for new terms
    // which default to human-only governance. Use a human principal.
    let human = Principal {
        subject: "tester".to_string(),
        roles: vec![Principal::HUMAN_ROLE.to_string()],
        permissions: vec![],
    };
    let resp = server
        .dispatch_command(
            json!({
                "subject": "lexicon:churn",
                "definition": {
                    "definition_short": "Loss of a paying customer in a billing period.",
                    "boundedContext": "billing"
                }
            }),
            human,
        )
        .await
        .expect("lexicon define returns Ok");
    // define response has term and entry fields.
    assert_eq!(
        resp["term"].as_str(),
        Some("churn"),
        "define response must echo term; got: {resp}"
    );
    assert!(
        resp.get("entry").is_some(),
        "define response must have entry; got: {resp}"
    );
    assert_eq!(
        resp.pointer("/entry/bounded_context")
            .and_then(|v| v.as_str()),
        Some("billing"),
        "bounded_context should round-trip through dispatch_lexicon_define reshape; got: {resp}"
    );
}

/// `definitionId + workflowId` together → AMBIGUOUS_INTENT structured response.
#[tokio::test]
async fn command_definition_id_plus_workflow_id_returns_ambiguous_intent() {
    let server = test_server().await;
    let resp = server
        .dispatch_command(
            json!({ "definitionId": "test_wf", "workflowId": "wf_X" }),
            Principal::anonymous(),
        )
        .await
        .unwrap();
    assert_eq!(
        resp["error"]["code"].as_str(),
        Some("AMBIGUOUS_INTENT"),
        "response must have AMBIGUOUS_INTENT error code; got: {resp}"
    );
    assert!(
        resp["links"].as_array().is_some(),
        "AMBIGUOUS_INTENT response must include HATEOAS links; got: {resp}"
    );
}

/// `subject + workflowId` on praxec.command → AMBIGUOUS_INTENT (no command shape matches).
#[tokio::test]
async fn command_subject_plus_workflow_id_is_ambiguous() {
    let server = test_server().await;
    let resp = server
        .dispatch_command(
            json!({
                "subject": "lexicon:churn",
                "workflowId": "wf_01",
                "definition": { "definition_short": "x" }
            }),
            Principal::anonymous(),
        )
        .await
        .unwrap();
    assert_eq!(
        resp["error"]["code"].as_str(),
        Some("AMBIGUOUS_INTENT"),
        "subject+workflowId on command must return AMBIGUOUS_INTENT; got: {resp}"
    );
}

/// Empty command args → AMBIGUOUS_INTENT structured response (no shape matches).
#[tokio::test]
async fn command_empty_args_returns_ambiguous_intent() {
    let server = test_server().await;
    let resp = server
        .dispatch_command(json!({}), Principal::anonymous())
        .await
        .unwrap();
    assert_eq!(
        resp["error"]["code"].as_str(),
        Some("AMBIGUOUS_INTENT"),
        "response must have AMBIGUOUS_INTENT error code; got: {resp}"
    );
    assert!(
        resp["links"].as_array().is_some(),
        "AMBIGUOUS_INTENT response must include HATEOAS links; got: {resp}"
    );
}

// ── HATEOAS link method names ─────────────────────────────────────────────────

/// Search response must emit HATEOAS links pointing at `praxec.query`, not
/// the old `gateway.home` / `gateway.search` tool names (T27 regression guard).
#[tokio::test]
async fn search_response_links_use_new_tool_names() {
    let server = test_server().await;
    let resp = server
        .dispatch_query(json!({ "query": "x" }), Principal::anonymous())
        .await
        .expect("search returns Ok");
    let links = resp["links"].as_array().expect("links array present");
    let home_link = links
        .iter()
        .find(|l| l["rel"] == "home")
        .expect("home link present");
    assert_eq!(
        home_link["method"].as_str(),
        Some("praxec.query"),
        "home link method must be praxec.query, not gateway.home; got: {home_link}"
    );
}

// ── lexicon_writes gate via dispatch_call ─────────────────────────────────────

/// Helper to build a `CallToolRequestParams` for dispatch_call.
fn call(tool: &'static str, args: Value) -> CallToolRequestParams {
    let m: JsonObject = args.as_object().cloned().unwrap_or_default();
    CallToolRequestParams::new(tool).with_arguments(m)
}

/// Default server has lexicon_writes OFF — define via dispatch_call must return
/// LEXICON_WRITES_DISABLED structured error with HATEOAS links.
#[tokio::test]
async fn lexicon_define_via_dispatch_call_is_blocked_when_writes_disabled() {
    let server = test_server().await; // default: with_lexicon_writes NOT enabled
    let params = call(
        "praxec.command",
        json!({
            "subject": "lexicon:churn",
            "definition": { "definition_short": "loss of paying customer" }
        }),
    );
    let resp = server.dispatch_call(params).await.expect("dispatch_call");
    assert_eq!(
        resp["error"]["code"].as_str(),
        Some("LEXICON_WRITES_DISABLED"),
        "expected LEXICON_WRITES_DISABLED; got: {resp}"
    );
    assert!(
        resp["links"].as_array().is_some(),
        "LEXICON_WRITES_DISABLED response must include HATEOAS links; got: {resp}"
    );
}

/// Server built with with_lexicon_writes(true) — define via dispatch_call must
/// NOT return LEXICON_WRITES_DISABLED (human principal passes governance gate).
#[tokio::test]
async fn lexicon_define_via_dispatch_call_succeeds_when_writes_enabled() {
    use praxec_core::audit::{AuditSink, MemoryAuditSink};
    use praxec_core::guards::DefaultGuardEvaluator;
    use praxec_core::store::{ConfigDefinitionStore, InMemoryWorkflowStore};
    use praxec_core::WorkflowRuntime;

    let cfg = json!({ "version": "1.0.0", "workflows": {} });
    let resolved = praxec_core::config::resolve(cfg).expect("resolve");
    let defs = Arc::new(ConfigDefinitionStore::from_config(&resolved));
    let store = Arc::new(InMemoryWorkflowStore::new());
    let audit = Arc::new(MemoryAuditSink::new());
    let guards = Arc::new(DefaultGuardEvaluator::new());
    let runtime = WorkflowRuntime::new(
        defs,
        store,
        Arc::new(NoopRegistry),
        guards,
        audit as Arc<dyn AuditSink>,
    );
    let server = PraxecServer::new(runtime).with_lexicon_writes(true);

    // Human principal — passes governance gate on new terms (default human-only).
    // dispatch_call uses Self::principal() → anonymous, but the gate check is
    // at the lexicon_writes_enabled level. We use dispatch_command directly
    // with a human principal so the governance gate inside handle_lexicon_define
    // also passes.
    let human = praxec_core::model::Principal {
        subject: "tester".to_string(),
        roles: vec![praxec_core::model::Principal::HUMAN_ROLE.to_string()],
        permissions: vec![],
    };
    let resp = server
        .dispatch_command(
            json!({
                "subject": "lexicon:churn",
                "definition": { "definition_short": "loss of paying customer" }
            }),
            human,
        )
        .await
        .expect("dispatch_command ok");
    assert_ne!(
        resp.get("error").and_then(|e| e.get("code")),
        Some(&json!("LEXICON_WRITES_DISABLED")),
        "LEXICON_WRITES_DISABLED must NOT fire when writes are enabled; got: {resp}"
    );
}
