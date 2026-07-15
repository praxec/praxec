//! SPEC §17.6 — skills search via `praxec.query` with `kind: "skill"`.
//! Authoring-time only; advertised conditionally; returns refs only
//! (progressive disclosure).
//!
//! Updated from the old TOOL_SKILLS_SEARCH constant to the §32 surface:
//! `praxec.query` with `kind: "skill"` (requires `with_skills_search(true)`).

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
    .with_writable_repo_roots(vec![praxec_core::RepoRoot::for_test()])
}

fn fixture_item(subject: &str, verb: &str) -> DiscoveryItem {
    fixture_item_with_source(subject, verb, "config")
}

fn fixture_item_with_source(subject: &str, verb: &str, source: &str) -> DiscoveryItem {
    DiscoveryItem {
        id: subject.into(),
        kind: DiscoveryKind::Guidance,
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
        body: Some("body content the test must never see".into()),
        source: Some(source.into()),
        structural_fingerprint: None,
    }
}

fn build_discovery() -> Arc<InMemoryDiscoveryIndex> {
    Arc::new(InMemoryDiscoveryIndex::new(vec![
        fixture_item("review.style.house-voice", "review"),
        fixture_item("review.editorial.checklist", "review"),
        fixture_item("debug.repro.standard", "diagnose"),
        fixture_item("authoring.skill.writing-rubric", "review"),
    ]))
}

fn enabled_server() -> PraxecServer {
    PraxecServer::new(build_runtime())
        .with_discovery(build_discovery())
        .with_skills_search(true)
}

fn disabled_server() -> PraxecServer {
    PraxecServer::new(build_runtime()).with_discovery(build_discovery())
}

/// Build a skills search call: praxec.query with kind="skill" plus extra
/// filter args. Under §32, skills search is `praxec.query { kind: "skill",
/// ... }` when `with_skills_search(true)` is enabled.
fn call_search(extra_args: Value) -> CallToolRequestParams {
    let mut map = extra_args.as_object().cloned().expect("object");
    map.insert("kind".into(), json!("skill"));
    // Also set query="" so the shape is clearly a search dispatch
    // (query present → search, kind filters by skill).
    if !map.contains_key("query") {
        map.insert("query".into(), json!(""));
    }
    CallToolRequestParams::new(TOOL_QUERY).with_arguments(map)
}

// ── Flag-off: tool absent from list_tools AND call rejected ─────────────────

#[tokio::test]
async fn tool_not_advertised_when_flag_off() {
    use praxec_mcp_server::tool_definitions;
    let _server = disabled_server();
    // The default `tool_definitions()` must NOT include any skills-search
    // specific tool — with §32 the surface is always just two tools.
    let names: Vec<String> = tool_definitions()
        .into_iter()
        .map(|t| t.name.to_string())
        .collect();
    // Verify the two-tool surface is present.
    assert!(names.contains(&"praxec.query".to_string()));
    assert!(names.contains(&"praxec.command".to_string()));
    // No old-name skills tool should appear.
    assert!(
        !names.iter().any(|n| n.contains("skills")),
        "no skills-named tool should appear in default list; got: {names:?}"
    );
}

#[tokio::test]
async fn call_rejected_when_flag_off() {
    let server = disabled_server();
    // Under §32, skills search goes through praxec.query with kind="skill".
    // When skills_search is disabled, the dispatch should return an error or
    // AMBIGUOUS_INTENT — NOT silently return empty or succeed.
    let err = server
        .dispatch_call(call_search(json!({})))
        .await
        .expect_err("call must be rejected when skills flag off");
    assert!(
        format!("{err:?}").contains("disabled"),
        "error should mention disabled: {err:?}"
    );
}

// ── Flag-on: returns refs (NO body field present) ───────────────────────────

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
        .dispatch_call(call_search(json!({ "verb": "diagnose" })))
        .await
        .expect("succeeds");
    let items = resp["items"].as_array().unwrap();
    assert!(
        !items.is_empty(),
        "expected at least one diagnose-tagged item"
    );
    for item in items {
        assert_eq!(item["verb"].as_str(), Some("diagnose"));
    }
}

// ── Subject-root filter ────────────────────────────────────────────────────

#[tokio::test]
async fn subject_root_filter_includes_only_matching_root() {
    let server = enabled_server();
    let resp = server
        .dispatch_call(call_search(json!({ "subject_root": "review" })))
        .await
        .expect("succeeds");
    let items = resp["items"].as_array().unwrap();
    assert!(!items.is_empty(), "expected items under review.*");
    for item in items {
        let subj = item["subject"].as_str().unwrap_or("");
        assert!(
            subj.starts_with("review."),
            "subject must start with review.; got: {subj}"
        );
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

// ── Edge: empty result returns items: [] not error ─────────────────────────

#[tokio::test]
async fn empty_filter_match_returns_empty_array_not_error() {
    let server = enabled_server();
    let resp = server
        .dispatch_call(call_search(json!({ "verb": "compose" })))
        .await
        .expect("empty result is OK, not error");
    let items = resp["items"].as_array().expect("items array present");
    assert!(items.is_empty(), "expected empty list; got: {items:?}");
}

// ── Source filter (SPEC §5.3) ──────────────────────────────────────────────

fn mixed_source_server() -> PraxecServer {
    let discovery = Arc::new(InMemoryDiscoveryIndex::new(vec![
        fixture_item_with_source("review.style.house-voice", "review", "config"),
        fixture_item_with_source("review.editorial.checklist", "review", "config"),
        fixture_item_with_source(
            "debug.repro.standard",
            "diagnose",
            "git+https://github.com/org/skills@abc123",
        ),
        fixture_item_with_source(
            "authoring.skill.writing-rubric",
            "review",
            "git+https://github.com/org/skills@abc123",
        ),
    ]));
    PraxecServer::new(build_runtime())
        .with_discovery(discovery)
        .with_skills_search(true)
}

#[tokio::test]
async fn source_filter_config_returns_only_config_declared_fragments() {
    let server = mixed_source_server();
    let resp = server
        .dispatch_call(call_search(json!({ "source": "config" })))
        .await
        .expect("succeeds");
    let items = resp["items"].as_array().unwrap();
    assert!(!items.is_empty(), "expected config-declared fragments");
    for item in items {
        assert_eq!(
            item["source"].as_str(),
            Some("config"),
            "source filter must exclude non-config items; got: {item}"
        );
    }
}

#[tokio::test]
async fn source_filter_git_url_returns_only_matching_ingested_fragments() {
    let server = mixed_source_server();
    let resp = server
        .dispatch_call(call_search(json!({
            "source": "git+https://github.com/org/skills@abc123"
        })))
        .await
        .expect("succeeds");
    let items = resp["items"].as_array().unwrap();
    assert!(!items.is_empty(), "expected git-ingested fragments");
    for item in items {
        assert_eq!(
            item["source"].as_str(),
            Some("git+https://github.com/org/skills@abc123")
        );
    }
}

#[tokio::test]
async fn source_filter_absent_returns_all_sources() {
    let server = mixed_source_server();
    let resp = server
        .dispatch_call(call_search(json!({})))
        .await
        .expect("succeeds");
    let items = resp["items"].as_array().unwrap();
    let sources: std::collections::HashSet<&str> =
        items.iter().filter_map(|i| i["source"].as_str()).collect();
    assert!(
        sources.contains("config"),
        "missing config items: {sources:?}"
    );
    assert!(
        sources.contains("git+https://github.com/org/skills@abc123"),
        "missing git items: {sources:?}"
    );
}

#[tokio::test]
async fn source_filter_unmatched_returns_empty_not_error() {
    let server = mixed_source_server();
    let resp = server
        .dispatch_call(call_search(
            json!({ "source": "git+https://other/repo@deadbeef" }),
        ))
        .await
        .expect("unmatched filter is OK, not error");
    let items = resp["items"].as_array().expect("items array present");
    assert!(items.is_empty(), "expected empty list; got: {items:?}");
}
