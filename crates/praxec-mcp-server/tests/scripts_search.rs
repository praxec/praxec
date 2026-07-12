//! SPEC §22 — scripts search via `praxec.query` with `kind: "script"`.
//! Authoring-time only; advertised conditionally; returns refs only
//! (progressive disclosure). Mirror of the skills_search test file with
//! kind-specific assertions.
//!
//! Updated from the old TOOL_SCRIPTS_SEARCH constant to the §32 surface:
//! `praxec.query` with `kind: "script"` (requires `with_scripts_search(true)`).

use std::sync::Arc;

use praxec_core::WorkflowRuntime;
use praxec_core::audit::{AuditSink, NullAuditSink};
use praxec_core::discovery::{DiscoveryItem, DiscoveryKind, DiscoveryLink, InMemoryDiscoveryIndex};
use praxec_core::guards::DefaultGuardEvaluator;
use praxec_core::ports::ExecutorRegistry;
use praxec_core::store::{ConfigDefinitionStore, InMemoryWorkflowStore};
use praxec_mcp_server::{PraxecServer, TOOL_QUERY};
use rmcp::model::CallToolRequestParams;
use serde_json::{Value, json};

struct NoopRegistry;
impl ExecutorRegistry for NoopRegistry {
    fn get(&self, _kind: &str) -> Option<Arc<dyn praxec_core::Executor>> {
        None
    }
}

fn build_runtime() -> WorkflowRuntime {
    WorkflowRuntime::new(
        Arc::new(ConfigDefinitionStore::default()),
        Arc::new(InMemoryWorkflowStore::default()),
        Arc::new(NoopRegistry),
        Arc::new(DefaultGuardEvaluator::new()),
        Arc::new(NullAuditSink) as Arc<dyn AuditSink>,
    )
}

fn script_item(subject: &str, verb: &str, source: &str) -> DiscoveryItem {
    DiscoveryItem {
        id: subject.into(),
        kind: DiscoveryKind::Script,
        title: format!("title for {subject}"),
        description: String::new(),
        tags: vec![],
        examples: vec![],
        aliases: vec![],
        text: String::new(),
        links: vec![DiscoveryLink {
            rel: "home".into(),
            title: None,
            description: None,
            method: "praxec.query".into(),
            args: json!({}),
            input_schema: None,
        }],
        verb: Some(verb.into()),
        body: Some("script body the test must never see".into()),
        source: Some(source.into()),
        structural_fingerprint: None,
    }
}

fn build_discovery() -> Arc<InMemoryDiscoveryIndex> {
    Arc::new(InMemoryDiscoveryIndex::new(vec![
        script_item("build.cargo.release", "build", "config"),
        script_item("build.cargo.workspace", "build", "config"),
        script_item("test.cargo.workspace", "test", "config"),
        script_item("lint.rust.clippy-strict", "lint", "cognitive-architectures"),
    ]))
}

fn enabled_server() -> PraxecServer {
    PraxecServer::new(build_runtime())
        .with_discovery(build_discovery())
        .with_scripts_search(true)
}

fn disabled_server() -> PraxecServer {
    PraxecServer::new(build_runtime()).with_discovery(build_discovery())
}

/// Build a scripts search call: praxec.query with kind="script" plus extra
/// filter args. Under §32, scripts search is `praxec.query { kind: "script",
/// ... }` when `with_scripts_search(true)` is enabled.
fn call_search(extra_args: Value) -> CallToolRequestParams {
    let mut map = extra_args.as_object().cloned().expect("object");
    map.insert("kind".into(), json!("script"));
    // Also set query="" so the shape is clearly a search dispatch.
    if !map.contains_key("query") {
        map.insert("query".into(), json!(""));
    }
    CallToolRequestParams::new(TOOL_QUERY).with_arguments(map)
}

// ── Flag off: tool absent from list_tools + call rejected ─────────────────

#[tokio::test]
async fn tool_not_advertised_when_flag_off() {
    use praxec_mcp_server::tool_definitions;
    let _server = disabled_server();
    let names: Vec<String> = tool_definitions()
        .into_iter()
        .map(|t| t.name.to_string())
        .collect();
    // §32: only two tools in the surface.
    assert!(names.contains(&"praxec.query".to_string()));
    assert!(names.contains(&"praxec.command".to_string()));
    // No old scripts-named tool.
    assert!(
        !names.iter().any(|n| n.contains("scripts")),
        "scripts.search must NOT appear in tool list; got: {names:?}"
    );
}

#[tokio::test]
async fn call_rejected_when_flag_off() {
    let server = disabled_server();
    let err = server
        .dispatch_call(call_search(json!({})))
        .await
        .expect_err("call must be rejected when scripts flag off");
    assert!(
        format!("{err:?}").contains("disabled"),
        "error should mention disabled: {err:?}"
    );
}

// ── Flag on: returns refs (NO body) ───────────────────────────────────────

#[tokio::test]
async fn returns_items_array_when_flag_on() {
    let server = enabled_server();
    let resp = server
        .dispatch_call(call_search(json!({})))
        .await
        .expect("succeeds");
    assert!(resp["items"].is_array());
}

#[tokio::test]
async fn response_items_carry_no_body_field() {
    let server = enabled_server();
    let resp = server
        .dispatch_call(call_search(json!({})))
        .await
        .expect("succeeds");
    let items = resp["items"].as_array().expect("items array");
    for item in items {
        assert!(
            item.get("body").is_none(),
            "progressive disclosure violation: ref {item} carries body"
        );
    }
}

// ── Verb filter ────────────────────────────────────────────────────────────

#[tokio::test]
async fn verb_filter_includes_only_matching_verb() {
    let server = enabled_server();
    let resp = server
        .dispatch_call(call_search(json!({ "verb": "build" })))
        .await
        .expect("succeeds");
    let items = resp["items"].as_array().unwrap();
    assert!(!items.is_empty(), "expected build-tagged items");
    for item in items {
        assert_eq!(item["verb"].as_str(), Some("build"));
    }
}

// ── Subject-root filter ────────────────────────────────────────────────────

#[tokio::test]
async fn subject_root_filter_includes_only_matching_root() {
    let server = enabled_server();
    let resp = server
        .dispatch_call(call_search(json!({ "subject_root": "build" })))
        .await
        .expect("succeeds");
    let items = resp["items"].as_array().unwrap();
    assert!(!items.is_empty());
    for item in items {
        let subj = item["subject"].as_str().unwrap_or("");
        assert!(subj.starts_with("build."), "got: {subj}");
    }
}

// ── Source filter ──────────────────────────────────────────────────────────

#[tokio::test]
async fn source_filter_matches_exact_source() {
    let server = enabled_server();
    let resp = server
        .dispatch_call(call_search(json!({ "source": "cognitive-architectures" })))
        .await
        .expect("succeeds");
    let items = resp["items"].as_array().unwrap();
    assert!(!items.is_empty());
    for item in items {
        assert_eq!(item["source"].as_str(), Some("cognitive-architectures"));
    }
}

// ── Limit ─────────────────────────────────────────────────────────────────

#[tokio::test]
async fn limit_caps_result_count() {
    let server = enabled_server();
    let resp = server
        .dispatch_call(call_search(json!({ "limit": 2 })))
        .await
        .expect("succeeds");
    let items = resp["items"].as_array().unwrap();
    assert!(items.len() <= 2);
}

// ── Empty match returns [] not error ──────────────────────────────────────

#[tokio::test]
async fn empty_filter_match_returns_empty_array() {
    let server = enabled_server();
    let resp = server
        .dispatch_call(call_search(json!({ "verb": "compose" })))
        .await
        .expect("empty result is OK");
    let items = resp["items"].as_array().expect("items array present");
    assert!(items.is_empty());
}

// ── Listings exclude DiscoveryKind::Guidance items ────────────────────────

#[tokio::test]
async fn scripts_search_excludes_guidance_items() {
    // Add a guidance item to the index alongside scripts; scripts.search
    // must NOT return it (the listing is kind-filtered).
    let mut items: Vec<DiscoveryItem> = vec![script_item("build.cargo.release", "build", "config")];
    items.push(DiscoveryItem {
        id: "review.code.adversarial".into(),
        kind: DiscoveryKind::Guidance,
        title: "guidance".into(),
        description: String::new(),
        tags: vec![],
        examples: vec![],
        aliases: vec![],
        text: String::new(),
        links: vec![],
        verb: Some("review".into()),
        body: Some("should NOT leak through scripts.search".into()),
        source: Some("config".into()),
        structural_fingerprint: None,
    });
    let discovery = Arc::new(InMemoryDiscoveryIndex::new(items));
    let server = PraxecServer::new(build_runtime())
        .with_discovery(discovery)
        .with_scripts_search(true);

    let resp = server
        .dispatch_call(call_search(json!({})))
        .await
        .expect("succeeds");
    let items = resp["items"].as_array().unwrap();
    for item in items {
        let subj = item["subject"].as_str().unwrap_or("");
        assert!(
            !subj.starts_with("review."),
            "scripts.search must NOT return guidance items; got: {subj}"
        );
    }
}
