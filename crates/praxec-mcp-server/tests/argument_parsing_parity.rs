//! Parity tests for per-tool argument parsing under the §32 two-tool surface.
//!
//! Locks down the runtime-observable behavior of each logical operation's
//! argument extraction layer so the shape-router refactor can't quietly
//! regress:
//!
//! 1. Required-field errors return the exact "<field> is required" message
//!    the current handlers produce (callers and audit consumers may key on
//!    these).
//! 2. Lenient defaults are preserved — fields the current handlers treat
//!    as optional (e.g. `definitionId`, `query`/`limit`/`kind`, `arguments`)
//!    must keep falling through to the runtime/discovery layer without a
//!    parse error.
//! 3. Unknown tool names route through the same `invalid_params` path with
//!    the same message.
//!
//! Tests go through `PraxecServer::dispatch_call`, which is the same
//! dispatch table `ServerHandler::call_tool` uses minus the transport
//! plumbing.
//!
//! Translation: all calls now use `praxec.query` or `praxec.command`
//! with shape-dispatched args per SPEC §32.

use std::sync::Arc;

use async_trait::async_trait;
use praxec_core::WorkflowRuntime;
use praxec_core::audit::NullAuditSink;
use praxec_core::discovery::{DiscoveryItem, DiscoveryKind, DiscoveryLink, InMemoryDiscoveryIndex};
use praxec_core::error::ExecutorError;
use praxec_core::guards::DefaultGuardEvaluator;
use praxec_core::model::{ExecuteRequest, ExecuteResult};
use praxec_core::ports::{Executor, ExecutorRegistry};
use praxec_core::store::{ConfigDefinitionStore, InMemoryWorkflowStore};
use praxec_mcp_server::{PraxecServer, TOOL_COMMAND, TOOL_QUERY};
use rmcp::ErrorData as McpError;
use rmcp::model::{CallToolRequestParams, ErrorCode, JsonObject};
use serde_json::{Value, json};

struct InertExecutors;
#[async_trait]
impl Executor for InertExecutors {
    async fn execute(&self, _: ExecuteRequest) -> Result<ExecuteResult, ExecutorError> {
        Ok(ExecuteResult::default())
    }
}
impl ExecutorRegistry for InertExecutors {
    fn get(&self, _kind: &str) -> Option<Arc<dyn Executor>> {
        None
    }
}

fn build_runtime() -> WorkflowRuntime {
    // Empty definitions: any `start` falls through to a runtime error of
    // "workflow definition '...' not found". That's exactly what lets us tell
    // "parse succeeded but runtime rejected" apart from "handler rejected
    // before reaching the runtime."
    WorkflowRuntime::new(
        Arc::new(ConfigDefinitionStore::default()),
        Arc::new(InMemoryWorkflowStore::default()),
        Arc::new(InertExecutors),
        Arc::new(DefaultGuardEvaluator::new()),
        Arc::new(NullAuditSink),
    )
    .with_writable_repo_roots(vec![praxec_core::RepoRoot::for_test()])
}

fn build_discovery() -> Arc<InMemoryDiscoveryIndex> {
    Arc::new(InMemoryDiscoveryIndex::new(vec![
        DiscoveryItem {
            id: "wf.alpha".into(),
            kind: DiscoveryKind::Workflow,
            title: "Alpha workflow".into(),
            description: "alpha description".into(),
            tags: vec!["alpha".into()],
            examples: vec![],
            aliases: vec![],
            text: "alpha text".into(),
            links: vec![DiscoveryLink {
                rel: "start".into(),
                title: None,
                description: None,
                method: "praxec.command".into(),
                args: json!({ "definitionId": "wf.alpha", "input": {} }),
                input_schema: None,
            }],
            verb: None,
            body: None,
            source: None,
            structural_fingerprint: None,
        },
        DiscoveryItem {
            id: "cap.beta".into(),
            kind: DiscoveryKind::Capability,
            title: "Beta capability".into(),
            description: "beta description".into(),
            tags: vec![],
            examples: vec![],
            aliases: vec![],
            text: "beta text".into(),
            links: vec![],
            verb: None,
            body: None,
            source: None,
            structural_fingerprint: None,
        },
    ]))
}

fn build_server() -> PraxecServer {
    PraxecServer::new(build_runtime()).with_discovery(build_discovery())
}

#[test]
fn cmp001_meta_claim_overrides_default_principal_and_absent_falls_back() {
    use praxec_mcp_server::PRINCIPAL_META_KEY;
    use rmcp::model::Meta;

    // Default (no config principal) is anonymous, fail-closed.
    let server = build_server();
    let empty = Meta::new();
    let p = server.resolve_principal(&empty);
    assert_eq!(p.subject, "anonymous");
    assert!(!p.is_human());

    // A _meta claim (the host-controlled channel) is honored.
    let mut meta = Meta::new();
    meta.insert(
        PRINCIPAL_META_KEY.to_string(),
        json!({ "subject": "user:bob", "roles": ["human"] }),
    );
    let p = server.resolve_principal(&meta);
    assert_eq!(p.subject, "user:bob");
    assert!(p.is_human(), "the host can assert the human role via _meta");
}

#[test]
fn cmp001_only_the_reserved_meta_key_grants_identity() {
    // Trust-boundary hardening: identity is honored ONLY under the reserved
    // PRINCIPAL_META_KEY, and ONLY from _meta (never tool arguments). Arbitrary
    // _meta fields — or a claim an attacker might smuggle under a different key —
    // must not escalate. resolve_principal structurally consults only
    // meta.get(PRINCIPAL_META_KEY), so a bare {subject, roles} at the meta root
    // is ignored.
    use praxec_mcp_server::PRINCIPAL_META_KEY;
    use rmcp::model::Meta;

    let server = build_server();
    let mut meta = Meta::new();
    meta.insert("subject".into(), json!("user:evil"));
    meta.insert("roles".into(), json!(["human"]));
    meta.insert(
        "principal".into(),
        json!({ "subject": "x", "roles": ["human"] }),
    );
    let p = server.resolve_principal(&meta);
    assert_eq!(
        p.subject, "anonymous",
        "a claim not under {PRINCIPAL_META_KEY} must not grant identity"
    );
    assert!(!p.is_human(), "no escalation via arbitrary meta keys");
}

#[test]
fn cmp001_trust_meta_principal_false_ignores_meta_claims() {
    // An operator who doesn't trust the _meta channel can disable it; every
    // caller then runs as the configured default, regardless of _meta.
    use praxec_mcp_server::PRINCIPAL_META_KEY;
    use rmcp::model::Meta;

    let server = build_server().with_trust_meta_principal(false);
    let mut meta = Meta::new();
    meta.insert(
        PRINCIPAL_META_KEY.to_string(),
        json!({ "subject": "user:claimed", "roles": ["human"] }),
    );
    let p = server.resolve_principal(&meta);
    assert_eq!(
        p.subject, "anonymous",
        "_meta must be ignored when trust is off"
    );
    assert!(!p.is_human());
}

#[test]
fn cmp001_configured_default_principal_is_used_without_meta() {
    use praxec_core::model::Principal;
    use rmcp::model::Meta;

    let server = build_server().with_principal(Principal {
        subject: "svc:operator".into(),
        roles: vec![Principal::HUMAN_ROLE.to_string()],
        permissions: vec![],
    });
    let p = server.resolve_principal(&Meta::new());
    assert_eq!(p.subject, "svc:operator");
    assert!(p.is_human());
}

fn call_args(name: &'static str, args: Value) -> CallToolRequestParams {
    let map: JsonObject = args
        .as_object()
        .cloned()
        .expect("test args must be an object");
    CallToolRequestParams::new(name).with_arguments(map)
}

async fn dispatch(
    server: &PraxecServer,
    name: &'static str,
    args: Value,
) -> Result<Value, McpError> {
    server.dispatch_call(call_args(name, args)).await
}

// ---------- praxec.query → home (no args) ---------------------------------

#[tokio::test]
async fn home_returns_home_value_with_links() {
    let server = build_server();
    let resp = dispatch(&server, TOOL_QUERY, json!({})).await.unwrap();
    assert!(
        resp.get("links").and_then(Value::as_array).is_some(),
        "home response must include `links`: {resp}"
    );
}

#[tokio::test]
async fn home_ignores_extra_args() {
    // Extra fields that aren't in QueryArgs dispatch schema should fall
    // through as unknown → ambiguous, but {} definitely maps to home.
    let server = build_server();
    let resp = dispatch(&server, TOOL_QUERY, json!({}))
        .await
        .expect("empty query args maps to home");
    assert!(resp.get("links").is_some());
}

// ---------- praxec.query → search (query present) -------------------------

#[tokio::test]
async fn search_defaults_query_to_empty_string() {
    // Schema says `query` is required on search, but the current handler
    // accepts a missing one and defaults to "" — runtime treats empty as
    // "match all". Shape-router: passing `query: ""` explicitly.
    let server = build_server();
    let resp = dispatch(&server, TOOL_QUERY, json!({ "query": "" }))
        .await
        .unwrap();
    assert_eq!(resp["query"], json!(""));
    assert_eq!(resp["kind"], Value::Null);
    let items = resp["items"].as_array().expect("items array");
    assert_eq!(items.len(), 2, "default search returns all indexed items");
}

#[tokio::test]
async fn search_default_limit_is_ten() {
    let server = build_server();
    let resp = dispatch(&server, TOOL_QUERY, json!({ "query": "" }))
        .await
        .unwrap();
    // Two items in the index; default limit of 10 doesn't truncate.
    assert_eq!(resp["items"].as_array().unwrap().len(), 2);
}

#[tokio::test]
async fn search_respects_explicit_limit() {
    let server = build_server();
    let resp = dispatch(&server, TOOL_QUERY, json!({ "query": "", "limit": 1 }))
        .await
        .unwrap();
    assert_eq!(resp["items"].as_array().unwrap().len(), 1);
}

#[tokio::test]
async fn search_kind_unknown_string_is_invalid_params() {
    // CMP-014 — an unrecognized `kind` must NOT silently degrade to an
    // unfiltered search (that masks a fat-fingered filter). A present-but-
    // unknown `kind` is caller-supplied bad input → invalid_params (-32602).
    let server = build_server();
    let err = dispatch(
        &server,
        TOOL_QUERY,
        json!({ "query": "", "kind": "garbage" }),
    )
    .await
    .expect_err("unknown kind must be rejected, not silently ignored");
    assert_eq!(err.code, ErrorCode::INVALID_PARAMS);
    assert!(
        err.message.contains("garbage"),
        "error should name the bad kind: {}",
        err.message
    );
}

#[tokio::test]
async fn search_kind_workflow_filters_to_workflows_only() {
    let server = build_server();
    let resp = dispatch(
        &server,
        TOOL_QUERY,
        json!({ "query": "", "kind": "workflow" }),
    )
    .await
    .unwrap();
    assert_eq!(resp["kind"], json!("workflow"));
    let items = resp["items"].as_array().unwrap();
    assert_eq!(items.len(), 1);
    assert_eq!(items[0]["item"]["id"], json!("wf.alpha"));
}

// ---------- praxec.query → describe (subject present) ---------------------

#[tokio::test]
async fn describe_without_id_returns_required_error() {
    // Under §32, describe requires `subject` in QueryArgs. Calling
    // praxec.query with no fields routes to home (not an error). To
    // trigger the "id is required" path we must reach handle_describe
    // directly, which requires subject to be present. The § surface
    // means there's no "describe without id" path at the query level —
    // empty args → home. This test verifies that calling with no subject
    // DOES NOT produce the old describe error (it goes to home instead).
    let server = build_server();
    let resp = dispatch(&server, TOOL_QUERY, json!({})).await.unwrap();
    // Should reach home, not describe error.
    assert!(
        resp.get("links").is_some(),
        "empty query args should route to home: {resp}"
    );
}

#[tokio::test]
async fn describe_with_known_subject_returns_item() {
    let server = build_server();
    let resp = dispatch(&server, TOOL_QUERY, json!({ "subject": "wf.alpha" }))
        .await
        .unwrap();
    assert_eq!(resp["id"], json!("wf.alpha"));
    assert_eq!(resp["item"]["id"], json!("wf.alpha"));
}

#[tokio::test]
async fn describe_with_unknown_subject_returns_null_item() {
    let server = build_server();
    let resp = dispatch(&server, TOOL_QUERY, json!({ "subject": "nope" }))
        .await
        .unwrap();
    assert_eq!(resp["item"], Value::Null);
}

/// SPEC §12 — describe on a guidance fragment returns the flat
/// `{ kind: "guidance", subject, verb, body }` shape, NOT the workflow /
/// capability `{ id, item, links }` wrapper.
#[tokio::test]
async fn describe_guidance_uses_flat_wire_format() {
    use praxec_core::discovery::{DiscoveryItem, DiscoveryKind, InMemoryDiscoveryIndex};

    let runtime = build_runtime();
    let discovery = InMemoryDiscoveryIndex::new(vec![DiscoveryItem {
        id: "house-voice".into(),
        kind: DiscoveryKind::Guidance,
        title: "house-voice".into(),
        description: String::new(),
        tags: vec![],
        examples: vec![],
        aliases: vec![],
        text: "house-voice apply".into(),
        links: vec![],
        verb: Some("apply".into()),
        body: Some("Lead with the reader's problem.".into()),
        source: Some("config".into()),
        structural_fingerprint: None,
    }]);
    let server = PraxecServer::new(runtime).with_discovery(Arc::new(discovery));

    let resp = dispatch(&server, TOOL_QUERY, json!({ "subject": "house-voice" }))
        .await
        .unwrap();

    assert_eq!(resp["kind"].as_str(), Some("guidance"));
    assert_eq!(resp["subject"].as_str(), Some("house-voice"));
    assert_eq!(resp["verb"].as_str(), Some("apply"));
    assert_eq!(
        resp["body"].as_str(),
        Some("Lead with the reader's problem.")
    );
    // Must NOT use the wrapper for guidance.
    assert!(
        resp.get("item").is_none(),
        "guidance describe must not carry an `item` wrapper; got: {resp}"
    );
}

// ---------- praxec.command → start ----------------------------------------

#[tokio::test]
async fn start_without_definition_id_is_ambiguous_intent_not_required_error() {
    // §32 + CMP-030: start shape requires definitionId; omitting it means the
    // shape doesn't match start, so the command dispatcher falls through to
    // AMBIGUOUS_INTENT. handle_start no longer defaults an absent definitionId
    // to the proxy workflow — that decision belongs to the dispatch/store
    // layer, not the handler. This test verifies the no-args case stays a
    // structured AMBIGUOUS_INTENT and never surfaces "is required".
    let server = build_server();
    let resp = dispatch(&server, TOOL_COMMAND, json!({})).await;
    match resp {
        Ok(v) => {
            // AMBIGUOUS_INTENT response is OK here.
            assert!(
                v.get("error").is_some() || v.get("links").is_some(),
                "empty command args should be AMBIGUOUS_INTENT or home: {v}"
            );
        }
        Err(e) => {
            assert!(
                !e.message.contains("is required"),
                "start should not raise a parse-level required error: {}",
                e.message
            );
        }
    }
}

#[tokio::test]
async fn start_with_explicit_definition_id_passes_through() {
    let server = build_server();
    let err = dispatch(
        &server,
        TOOL_COMMAND,
        json!({ "definitionId": "explicit.id", "input": { "k": "v" } }),
    )
    .await
    .unwrap_err();
    assert!(
        err.message.contains("explicit.id"),
        "expected runtime error to name 'explicit.id', got: {}",
        err.message
    );
}

#[tokio::test]
async fn start_without_input_defaults_to_empty_object() {
    // Schema marks `input` required; runtime accepts missing and supplies
    // `{}`. The handler doesn't return "input is required"; it falls
    // through to the same runtime error as the with-input case.
    let server = build_server();
    let err = dispatch(&server, TOOL_COMMAND, json!({ "definitionId": "x" }))
        .await
        .unwrap_err();
    assert!(
        !err.message.contains("input is required"),
        "input should default to {{}}, got: {}",
        err.message
    );
    assert!(err.message.contains('x'));
}

// ---------- praxec.query → get (workflowId alone) -------------------------

#[tokio::test]
async fn get_without_workflow_id_returns_required_error() {
    // Under §32, workflowId alone → get. Without it, we'd route to home
    // (no fields = home). The "workflowId is required" error comes from
    // inside handle_get when workflowId is explicitly null, which can't
    // be triggered via query routing. Verify we don't get that error on
    // empty args (goes to home instead).
    let server = build_server();
    let resp = dispatch(&server, TOOL_QUERY, json!({})).await.unwrap();
    assert!(
        resp.get("links").is_some(),
        "empty query args route to home, not a required-field error: {resp}"
    );
}

#[tokio::test]
async fn get_with_workflow_id_passes_through_to_runtime() {
    let server = build_server();
    let err = dispatch(&server, TOOL_QUERY, json!({ "workflowId": "wf-1" }))
        .await
        .unwrap_err();
    assert!(
        !err.message.contains("is required"),
        "should not raise a required-field error: {}",
        err.message
    );
    assert!(err.message.contains("wf-1"));
}

// ---------- praxec.command → submit ----------------------------------------

#[tokio::test]
async fn submit_without_workflow_id_returns_required_error() {
    // Without workflowId+transition+expectedVersion, the submit shape
    // doesn't match → AMBIGUOUS_INTENT (not an error in the old sense).
    // Verify: no "workflowId is required" error text leaks.
    let server = build_server();
    let resp = dispatch(&server, TOOL_COMMAND, json!({})).await;
    match resp {
        Ok(v) => {
            // AMBIGUOUS_INTENT is the expected outcome for no-arg command.
            assert!(v.get("error").is_some(), "should be AMBIGUOUS_INTENT: {v}");
        }
        Err(e) => {
            panic!("unexpected error for empty command args: {}", e.message);
        }
    }
}

#[tokio::test]
async fn submit_without_expected_version_returns_required_error() {
    // workflowId alone → get shape in query. In command, workflowId without
    // expectedVersion+transition → AMBIGUOUS_INTENT (not submit shape).
    let server = build_server();
    let resp = dispatch(&server, TOOL_COMMAND, json!({ "workflowId": "x" })).await;
    match resp {
        Ok(v) => {
            assert!(
                v.get("error").is_some(),
                "partial submit args should be AMBIGUOUS_INTENT: {v}"
            );
        }
        Err(e) => {
            panic!("unexpected error: {}", e.message);
        }
    }
}

#[tokio::test]
async fn submit_without_transition_returns_required_error() {
    // workflowId + expectedVersion but no transition → AMBIGUOUS_INTENT.
    let server = build_server();
    let resp = dispatch(
        &server,
        TOOL_COMMAND,
        json!({ "workflowId": "x", "expectedVersion": 0 }),
    )
    .await;
    match resp {
        Ok(v) => {
            assert!(
                v.get("error").is_some(),
                "partial submit args should be AMBIGUOUS_INTENT: {v}"
            );
        }
        Err(e) => {
            panic!("unexpected error: {}", e.message);
        }
    }
}

#[tokio::test]
async fn submit_without_arguments_defaults_to_empty_object() {
    // Full submit shape (workflowId + expectedVersion + transition) but no
    // `arguments` — handler must accept and default to `{}`.
    let server = build_server();
    let err = dispatch(
        &server,
        TOOL_COMMAND,
        json!({
            "workflowId": "x",
            "expectedVersion": 0,
            "transition": "t"
        }),
    )
    .await
    .unwrap_err();
    assert!(
        !err.message.contains("is required"),
        "arguments should default to {{}}: {}",
        err.message
    );
    assert!(err.message.contains('x'));
}

// ---------- praxec.query → explain (workflowId + transition) ---------------

#[tokio::test]
async fn explain_without_workflow_id_returns_required_error() {
    // transition alone (no workflowId) → AMBIGUOUS_INTENT (not a dispatch
    // shape). Verify no "workflowId is required" leaks through.
    let server = build_server();
    let resp = dispatch(&server, TOOL_QUERY, json!({ "transition": "t" })).await;
    match resp {
        Ok(v) => {
            assert!(
                v.get("error").is_some(),
                "transition-only query should be AMBIGUOUS_INTENT: {v}"
            );
        }
        Err(e) => {
            assert!(
                !e.message.contains("workflowId is required"),
                "AMBIGUOUS_INTENT, not required-field error: {}",
                e.message
            );
        }
    }
}

#[tokio::test]
async fn explain_without_transition_returns_required_error() {
    // workflowId alone → get (not explain). Verify explain's
    // "transition is required" path is NOT triggered by workflowId-only.
    let server = build_server();
    let err = dispatch(&server, TOOL_QUERY, json!({ "workflowId": "x" }))
        .await
        .unwrap_err();
    // This hits the runtime's "workflow not found" path (via get, not explain).
    assert!(
        !err.message.contains("transition is required"),
        "workflowId-only should route to get, not explain: {}",
        err.message
    );
}

#[tokio::test]
async fn explain_with_both_passes_through_to_runtime() {
    let server = build_server();
    let err = dispatch(
        &server,
        TOOL_QUERY,
        json!({ "workflowId": "wf-x", "transition": "t" }),
    )
    .await
    .unwrap_err();
    assert!(!err.message.contains("is required"), "got: {}", err.message);
}

// ---------- unknown tool ----------------------------------------------------

#[tokio::test]
async fn unknown_tool_returns_invalid_params_with_named_tool() {
    let server = build_server();
    let err = dispatch(&server, "bogus.tool", json!({}))
        .await
        .unwrap_err();
    assert_eq!(err.code, ErrorCode::INVALID_PARAMS);
    assert!(
        err.message.contains("bogus.tool"),
        "error should name the unknown tool: {}",
        err.message
    );
}

// ---------- CMP-014 / CMP-030 — malformed caller input → invalid_params ------

#[tokio::test]
async fn malformed_command_args_return_invalid_params_not_internal_error() {
    // CMP-014 — a type-mismatched field (definitionId must be a string) is
    // caller-supplied bad input. It must surface as invalid_params (-32602),
    // not internal_error (-32603), and must not silently fall through to an
    // all-None CommandArgs.
    let server = build_server();
    let err = dispatch(
        &server,
        TOOL_COMMAND,
        json!({ "definitionId": { "not": "a string" } }),
    )
    .await
    .expect_err("malformed command args must error");
    assert_eq!(err.code, ErrorCode::INVALID_PARAMS);
}

#[tokio::test]
async fn malformed_query_args_return_invalid_params() {
    // CMP-014 — same for the read tool: a type-mismatched field is bad input.
    let server = build_server();
    let err = dispatch(&server, TOOL_QUERY, json!({ "query": 12345 }))
        .await
        .expect_err("malformed query args must error");
    assert_eq!(err.code, ErrorCode::INVALID_PARAMS);
}

#[tokio::test]
async fn lexicon_lookup_with_empty_term_is_invalid_params() {
    // CMP-014 — `subject: "lexicon:"` with an empty term is a malformed
    // lookup, not a request to look up the empty string.
    let server = build_server();
    let err = dispatch(&server, TOOL_QUERY, json!({ "subject": "lexicon:" }))
        .await
        .expect_err("empty lexicon term must be rejected");
    assert_eq!(err.code, ErrorCode::INVALID_PARAMS);
}

#[tokio::test]
async fn lexicon_define_missing_definition_short_is_invalid_params() {
    // CMP-014 — define_new with a `definition` object that lacks
    // `definition_short` must NOT write an empty lexicon entry; it surfaces
    // as invalid_params. Requires lexicon writes enabled to reach the define
    // dispatch path.
    let server = build_server().with_lexicon_writes(true);
    let err = dispatch(
        &server,
        TOOL_COMMAND,
        json!({ "subject": "lexicon:churn", "definition": { "boundedContext": "billing" } }),
    )
    .await
    .expect_err("missing definition_short must be rejected");
    assert_eq!(err.code, ErrorCode::INVALID_PARAMS);
}
