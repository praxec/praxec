//! Tests for SPEC §30.10 / §32 — embedded lexicon in describe/get/explain.
//!
//! Each test asserts ONE behaviour. Names read as declarative statements.
//!
//! Phases:
//!   A. describe response embeds lexicon.
//!   B. get response embeds lexicon.
//!   C. explain response embeds lexicon.
//!   D. Budget enforcement (oversize → lookup_link).
//!   E. No lexicon field when no entries are in scope.

use std::sync::Arc;

use praxec_core::WorkflowRuntime;
use praxec_core::audit::{AuditSink, MemoryAuditSink};
use praxec_core::guards::DefaultGuardEvaluator;
use praxec_core::ports::ExecutorRegistry;
use praxec_core::store::{ConfigDefinitionStore, InMemoryWorkflowStore};
use praxec_mcp_server::{PraxecServer, TOOL_COMMAND, TOOL_QUERY};
use rmcp::model::{CallToolRequestParams, JsonObject};
use serde_json::{Value, json};

// ── helpers ───────────────────────────────────────────────────────────────────

struct NoopRegistry;
impl ExecutorRegistry for NoopRegistry {
    fn get(&self, _kind: &str) -> Option<Arc<dyn praxec_core::Executor>> {
        None
    }
}

fn call(tool: &'static str, args: Value) -> CallToolRequestParams {
    let m: JsonObject = args.as_object().cloned().expect("object");
    CallToolRequestParams::new(tool).with_arguments(m)
}

/// Build a server with lexicon writes enabled and the given config.
fn build_server(cfg: Value) -> (PraxecServer, Arc<MemoryAuditSink>) {
    let resolved = praxec_core::config::resolve(cfg).expect("resolve");
    let pending = praxec_core::lexicon::pending_subjects_from_resolved(&resolved);
    let lexicon_base = resolved
        .get("lexicon")
        .cloned()
        .unwrap_or_else(|| json!({}));
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
    )
    .with_writable_repo_roots(vec![praxec_core::RepoRoot::for_test()]);
    (
        PraxecServer::new(runtime)
            .with_lexicon_writes(true)
            .with_lexicon(lexicon_base)
            .with_pending_subjects(pending),
        audit,
    )
}

/// Config with a lexicon entry that has a short definition and refs.
fn config_with_lexicon_and_skill() -> Value {
    json!({
        "version": "1.0.0",
        "praxec": { "strict_namespacing": false },
        "lexicon": {
            "change-request": {
                "definition_short": "A structured proposal to modify a system.",
                "governance": "human-only",
                "refs": ["acceptance-criteria"]
            },
            "acceptance-criteria": {
                "definition_short": "Pass/fail conditions a change must meet.",
                "governance": "human-only"
            }
        },
        "skills": {
            "plan.change-request": {
                "verb": "plan",
                "lifecycle": "stable",
                "body": "Plan a change request by gathering requirements."
            }
        },
        "workflows": {
            "spec_wf": {
                "initialState": "planning",
                "states": {
                    "planning": {
                        "skills": ["plan.change-request"],
                        "transitions": {
                            "submit": { "target": "done" }
                        }
                    },
                    "done": { "terminal": true }
                }
            }
        }
    })
}

/// Config with a lexicon entry whose definition is over 200 bytes.
fn config_with_oversize_lexicon_entry() -> Value {
    // Make a definition_short that is longer than 200 bytes.
    let long_def = "A".repeat(201);
    json!({
        "version": "1.0.0",
        "praxec": { "strict_namespacing": false },
        "lexicon": {
            "big-term": {
                "definition_short": long_def,
                "governance": "human-only"
            }
        },
        "skills": {
            "plan.big-term": {
                "verb": "plan",
                "lifecycle": "stable",
                "body": "A skill body."
            }
        },
        "workflows": {
            "big_wf": {
                "initialState": "planning",
                "states": {
                    "planning": {
                        "skills": ["plan.big-term"],
                        "transitions": {
                            "submit": { "target": "done" }
                        }
                    },
                    "done": { "terminal": true }
                }
            }
        }
    })
}

/// Config with no lexicon at all.
fn config_without_lexicon() -> Value {
    json!({
        "version": "1.0.0",
        "praxec": { "strict_namespacing": false },
        "workflows": {
            "bare_wf": {
                "initialState": "idle",
                "states": {
                    "idle": {
                        "transitions": {
                            "done": { "target": "finished" }
                        }
                    },
                    "finished": { "terminal": true }
                }
            }
        }
    })
}

/// Start a workflow and return its id.
async fn start_workflow(server: &PraxecServer, definition_id: &str) -> String {
    let resp = server
        .dispatch_call(call(
            TOOL_COMMAND,
            json!({ "definitionId": definition_id, "input": {} }),
        ))
        .await
        .expect("start workflow");
    resp.pointer("/workflow/id")
        .and_then(Value::as_str)
        .expect("workflow.id in start response")
        .to_string()
}

// ─────────────────────────────────────────────────────────────────────────────
// Phase A — describe response embeds lexicon
//
// Tests use the workflow-context describe path (subject + workflowId), which
// resolves via the instance's pinned `_lexiconLibrary` snapshot. This is the
// primary production path; the live-discovery path is exercised separately
// via the DiscoveryIndex integration.
// ─────────────────────────────────────────────────────────────────────────────

/// A describe response for a guidance subject (workflow context) includes a
/// `lexicon` field when the subject has a lexicon entry.
#[tokio::test]
async fn describe_includes_lexicon_field_when_subject_has_entry() {
    let (server, _) = build_server(config_with_lexicon_and_skill());
    let wf_id = start_workflow(&server, "spec_wf").await;
    let resp = server
        .dispatch_call(call(
            TOOL_QUERY,
            json!({ "subject": "plan.change-request", "workflowId": wf_id }),
        ))
        .await
        .expect("dispatch_call");
    assert!(
        resp.get("lexicon").is_some(),
        "describe response must include lexicon field when subject has entry; got: {resp}"
    );
}

/// The embedded lexicon entry for the described subject contains `definition_short`.
#[tokio::test]
async fn describe_lexicon_entry_contains_definition_short() {
    let (server, _) = build_server(config_with_lexicon_and_skill());
    let wf_id = start_workflow(&server, "spec_wf").await;
    let resp = server
        .dispatch_call(call(
            TOOL_QUERY,
            json!({ "subject": "plan.change-request", "workflowId": wf_id }),
        ))
        .await
        .expect("dispatch_call");
    let def = resp
        .pointer("/lexicon/change-request/definition_short")
        .and_then(Value::as_str);
    assert_eq!(
        def,
        Some("A structured proposal to modify a system."),
        "lexicon entry for change-request must carry definition_short; got: {resp}"
    );
}

/// When a lexicon entry has `refs`, the ref terms are also embedded inline.
#[tokio::test]
async fn describe_lexicon_embeds_ref_terms() {
    let (server, _) = build_server(config_with_lexicon_and_skill());
    let wf_id = start_workflow(&server, "spec_wf").await;
    let resp = server
        .dispatch_call(call(
            TOOL_QUERY,
            json!({ "subject": "plan.change-request", "workflowId": wf_id }),
        ))
        .await
        .expect("dispatch_call");
    assert!(
        resp.pointer("/lexicon/acceptance-criteria").is_some(),
        "describe lexicon must embed ref term 'acceptance-criteria'; got: {resp}"
    );
}

/// The ref term's embedded entry contains its `definition_short`.
#[tokio::test]
async fn describe_lexicon_ref_entry_contains_definition_short() {
    let (server, _) = build_server(config_with_lexicon_and_skill());
    let wf_id = start_workflow(&server, "spec_wf").await;
    let resp = server
        .dispatch_call(call(
            TOOL_QUERY,
            json!({ "subject": "plan.change-request", "workflowId": wf_id }),
        ))
        .await
        .expect("dispatch_call");
    let def = resp
        .pointer("/lexicon/acceptance-criteria/definition_short")
        .and_then(Value::as_str);
    assert_eq!(
        def,
        Some("Pass/fail conditions a change must meet."),
        "ref entry must carry definition_short; got: {resp}"
    );
}

/// A describe response has no `lexicon` field when the subject has no lexicon entry.
#[tokio::test]
async fn describe_has_no_lexicon_field_when_no_entry() {
    let (server, _) = build_server(config_without_lexicon());
    // Describe a workflow (not a guidance subject) — no lexicon entry.
    let resp = server
        .dispatch_call(call(TOOL_QUERY, json!({ "subject": "bare_wf" })))
        .await
        .expect("dispatch_call");
    assert!(
        resp.get("lexicon").is_none(),
        "describe response must NOT include lexicon field when no entry; got: {resp}"
    );
}

// ─────────────────────────────────────────────────────────────────────────────
// Phase B — get response embeds lexicon
// ─────────────────────────────────────────────────────────────────────────────

/// A get response for a workflow whose current state references a skill with
/// a lexicon entry includes a `lexicon` field.
#[tokio::test]
async fn get_includes_lexicon_field_when_current_state_has_skill_with_entry() {
    let (server, _) = build_server(config_with_lexicon_and_skill());
    let wf_id = start_workflow(&server, "spec_wf").await;
    let resp = server
        .dispatch_call(call(TOOL_QUERY, json!({ "workflowId": wf_id })))
        .await
        .expect("dispatch_call");
    assert!(
        resp.get("lexicon").is_some(),
        "get response must include lexicon field when state has skill with entry; got: {resp}"
    );
}

/// The get response's embedded lexicon entry contains `definition_short` for
/// the skill subject referenced in the current state.
#[tokio::test]
async fn get_lexicon_entry_contains_definition_short() {
    let (server, _) = build_server(config_with_lexicon_and_skill());
    let wf_id = start_workflow(&server, "spec_wf").await;
    let resp = server
        .dispatch_call(call(TOOL_QUERY, json!({ "workflowId": wf_id })))
        .await
        .expect("dispatch_call");
    let def = resp
        .pointer("/lexicon/change-request/definition_short")
        .and_then(Value::as_str);
    assert_eq!(
        def,
        Some("A structured proposal to modify a system."),
        "get lexicon must embed change-request definition_short; got: {resp}"
    );
}

/// A get response has no `lexicon` field when the workflow's lexicon is empty.
#[tokio::test]
async fn get_has_no_lexicon_field_when_workflow_has_no_lexicon() {
    let (server, _) = build_server(config_without_lexicon());
    let wf_id = start_workflow(&server, "bare_wf").await;
    let resp = server
        .dispatch_call(call(TOOL_QUERY, json!({ "workflowId": wf_id })))
        .await
        .expect("dispatch_call");
    assert!(
        resp.get("lexicon").is_none(),
        "get response must NOT include lexicon field when no entries; got: {resp}"
    );
}

// ─────────────────────────────────────────────────────────────────────────────
// Phase C — explain response embeds lexicon
// ─────────────────────────────────────────────────────────────────────────────

/// An explain response for a transition whose source state references a skill
/// with a lexicon entry includes a `lexicon` field.
#[tokio::test]
async fn explain_includes_lexicon_field_when_state_has_skill_with_entry() {
    let (server, _) = build_server(config_with_lexicon_and_skill());
    let wf_id = start_workflow(&server, "spec_wf").await;
    let resp = server
        .dispatch_call(call(
            TOOL_QUERY,
            json!({ "workflowId": wf_id, "transition": "submit" }),
        ))
        .await
        .expect("dispatch_call");
    assert!(
        resp.get("lexicon").is_some(),
        "explain response must include lexicon field when state has skill with entry; got: {resp}"
    );
}

/// The explain response's embedded lexicon entry contains `definition_short`.
#[tokio::test]
async fn explain_lexicon_entry_contains_definition_short() {
    let (server, _) = build_server(config_with_lexicon_and_skill());
    let wf_id = start_workflow(&server, "spec_wf").await;
    let resp = server
        .dispatch_call(call(
            TOOL_QUERY,
            json!({ "workflowId": wf_id, "transition": "submit" }),
        ))
        .await
        .expect("dispatch_call");
    let def = resp
        .pointer("/lexicon/change-request/definition_short")
        .and_then(Value::as_str);
    assert_eq!(
        def,
        Some("A structured proposal to modify a system."),
        "explain lexicon must embed change-request definition_short; got: {resp}"
    );
}

/// An explain response has no `lexicon` field when no lexicon entries are in scope.
#[tokio::test]
async fn explain_has_no_lexicon_field_when_no_entries_in_scope() {
    let (server, _) = build_server(config_without_lexicon());
    let wf_id = start_workflow(&server, "bare_wf").await;
    let resp = server
        .dispatch_call(call(
            TOOL_QUERY,
            json!({ "workflowId": wf_id, "transition": "done" }),
        ))
        .await
        .expect("dispatch_call");
    assert!(
        resp.get("lexicon").is_none(),
        "explain response must NOT include lexicon field when no entries in scope; got: {resp}"
    );
}

// ─────────────────────────────────────────────────────────────────────────────
// Phase D — Budget enforcement (oversize → lookup_link)
// ─────────────────────────────────────────────────────────────────────────────

/// When a `definition_short` exceeds 200 bytes, the lexicon entry is embedded
/// as a `lookup_link` object rather than inline. Uses a workflow-context
/// describe so the `_lexiconLibrary` snapshot is consulted.
#[tokio::test]
async fn oversize_definition_is_embedded_as_lookup_link() {
    let (server, _) = build_server(config_with_oversize_lexicon_entry());
    let wf_id = start_workflow(&server, "big_wf").await;
    let resp = server
        .dispatch_call(call(
            TOOL_QUERY,
            json!({ "subject": "plan.big-term", "workflowId": wf_id }),
        ))
        .await
        .expect("dispatch_call");
    // The entry should be present as a lookup_link (not inline definition_short).
    let entry = resp
        .pointer("/lexicon/big-term")
        .expect("lexicon/big-term must be present");
    assert!(
        entry.get("lookup_link").is_some(),
        "oversize entry must be embedded as lookup_link; got: {entry}"
    );
    assert!(
        entry.get("definition_short").is_none(),
        "oversize entry must NOT embed definition_short inline; got: {entry}"
    );
}

/// The `lookup_link` for an oversize entry uses `praxec.query` as the method.
#[tokio::test]
async fn oversize_lookup_link_uses_praxec_query_method() {
    let (server, _) = build_server(config_with_oversize_lexicon_entry());
    let wf_id = start_workflow(&server, "big_wf").await;
    let resp = server
        .dispatch_call(call(
            TOOL_QUERY,
            json!({ "subject": "plan.big-term", "workflowId": wf_id }),
        ))
        .await
        .expect("dispatch_call");
    let method = resp
        .pointer("/lexicon/big-term/lookup_link/method")
        .and_then(Value::as_str);
    assert_eq!(
        method,
        Some("praxec.query"),
        "lookup_link method must be praxec.query; got: {resp}"
    );
}

/// The `lookup_link` args use `lexicon:<term>` as the subject.
#[tokio::test]
async fn oversize_lookup_link_args_use_lexicon_namespace() {
    let (server, _) = build_server(config_with_oversize_lexicon_entry());
    let wf_id = start_workflow(&server, "big_wf").await;
    let resp = server
        .dispatch_call(call(
            TOOL_QUERY,
            json!({ "subject": "plan.big-term", "workflowId": wf_id }),
        ))
        .await
        .expect("dispatch_call");
    let subject = resp
        .pointer("/lexicon/big-term/lookup_link/args/subject")
        .and_then(Value::as_str);
    assert_eq!(
        subject,
        Some("lexicon:big-term"),
        "lookup_link args subject must be 'lexicon:big-term'; got: {resp}"
    );
}

/// A `hash` field is included alongside the `lookup_link` so callers can
/// cache-bust stale lookups.
#[tokio::test]
async fn oversize_lookup_link_includes_hash_field() {
    let (server, _) = build_server(config_with_oversize_lexicon_entry());
    let wf_id = start_workflow(&server, "big_wf").await;
    let resp = server
        .dispatch_call(call(
            TOOL_QUERY,
            json!({ "subject": "plan.big-term", "workflowId": wf_id }),
        ))
        .await
        .expect("dispatch_call");
    let entry = resp.pointer("/lexicon/big-term").expect("entry present");
    assert!(
        entry.get("hash").is_some(),
        "oversize entry must carry hash alongside lookup_link; got: {entry}"
    );
}
