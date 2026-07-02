//! SPEC §30.10.4-5 — pre-start subject walk; SUBJECT_NEEDS_DEFINITION
//! interaction protocol.
//!
//! When `praxec.command` is called with `definitionId` for a workflow whose
//! definition references an unresolved subject (one with
//! `state: "PENDING_DEFINITION"` in the `_lexiconLibrary`), the runtime must:
//!
//!   1. NOT create the workflow instance.
//!   2. Return a structured `SUBJECT_NEEDS_DEFINITION` response with HATEOAS
//!      links and the original command echoed back as `queued_command`.
//!
//! When every subject IS resolved, start proceeds normally and returns a
//! workflow instance.

use std::sync::Arc;

use praxec_core::audit::{AuditSink, MemoryAuditSink};
use praxec_core::guards::DefaultGuardEvaluator;
use praxec_core::ports::ExecutorRegistry;
use praxec_core::store::{ConfigDefinitionStore, InMemoryWorkflowStore};
use praxec_core::WorkflowRuntime;
use praxec_mcp_server::{PraxecServer, TOOL_COMMAND};
use rmcp::model::{CallToolRequestParams, JsonObject};
use serde_json::{json, Value};

struct NoopRegistry;
impl ExecutorRegistry for NoopRegistry {
    fn get(&self, _kind: &str) -> Option<Arc<dyn praxec_core::Executor>> {
        None
    }
}

fn call(name: &'static str, args: Value) -> CallToolRequestParams {
    let m: JsonObject = args.as_object().cloned().expect("object");
    CallToolRequestParams::new(name).with_arguments(m)
}

/// Config with a workflow that *references* an unresolved subject — a workflow
/// executor names `build.evidence-foo` but nothing defines it (no script/skill/
/// cap) and it has no lexicon entry, so `evidence-foo` is genuinely-unknown
/// vocabulary and becomes a PENDING_DEFINITION placeholder. (Per the §30.10.3
/// relaxation, a *defined* script subject would resolve itself — only an
/// undefined reference is pending — so the fixture must reference, not define.)
fn config_with_unresolved_subject() -> Value {
    json!({
        "version": "1.0.0",
        "praxec": { "strict_namespacing": false },
        "lexicon": {},
        "workflows": {
            "pending_wf": {
                "initialState": "idle",
                "states": {
                    "idle": { "transitions": { "go": {
                        "target": "done",
                        "executor": { "kind": "script", "subject": "build.evidence-foo" }
                    } } },
                    "done": { "terminal": true }
                }
            }
        }
    })
}

/// Config where every referenced subject has a real lexicon entry.
fn config_with_resolved_subjects() -> Value {
    json!({
        "version": "1.0.0",
        "praxec": { "strict_namespacing": false },
        "lexicon": {
            "evidence-foo": {
                "definition_short": "A real evidence concept.",
                "governance": "human-only"
            }
        },
        "scripts": {
            "build.evidence-foo": {
                "verb": "build",
                "lifecycle": "experimental",
                "body": "#!/usr/bin/env bash\necho evidence-foo\n"
            }
        },
        "workflows": {
            "resolved_wf": {
                "initialState": "idle",
                "states": { "idle": { "terminal": true } }
            }
        }
    })
}

fn build_server(cfg: Value) -> (PraxecServer, Arc<MemoryAuditSink>) {
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
        audit.clone() as Arc<dyn AuditSink>,
    );
    (PraxecServer::new(runtime), audit)
}

// ── §30.10.4 — pre-start walk blocks unresolved subjects ─────────────────────

/// SPEC §30.10.4-5 — starting a workflow whose definition has unresolved
/// subjects returns a structured SUBJECT_NEEDS_DEFINITION response, does NOT
/// create a workflow instance, and echoes the original args back verbatim.
#[tokio::test]
async fn praxec_command_start_with_unresolved_subject_returns_interaction() {
    let (server, audit) = build_server(config_with_unresolved_subject());

    let start_args = json!({
        "definitionId": "pending_wf",
        "input": {}
    });

    let resp = server
        .dispatch_call(call(TOOL_COMMAND, start_args.clone()))
        .await
        .expect("dispatch_call returns Ok — SUBJECT_NEEDS_DEFINITION is a structured response");

    // Must carry the interaction kind.
    assert_eq!(
        resp["interaction"]["kind"], "SUBJECT_NEEDS_DEFINITION",
        "expected SUBJECT_NEEDS_DEFINITION interaction; got: {resp}"
    );

    // unknown_subject must be a string (the placeholder term).
    assert!(
        resp["interaction"]["unknown_subject"].is_string(),
        "unknown_subject must be a string; got: {resp}"
    );

    // context.encountered_in must be prefixed "workflow:".
    let encountered_in = resp["interaction"]["context"]["encountered_in"]
        .as_str()
        .expect("encountered_in must be a string");
    assert!(
        encountered_in.starts_with("workflow:"),
        "encountered_in must start with 'workflow:'; got: {encountered_in}"
    );

    // queued_command echoes the original command method and args.
    assert_eq!(
        resp["queued_command"]["method"], "praxec.command",
        "queued_command.method must be praxec.command"
    );
    assert_eq!(
        resp["queued_command"]["args"], start_args,
        "queued_command.args must echo the original start args verbatim"
    );

    // links array must have all three resolution paths.
    let links = resp["links"].as_array().expect("links must be an array");
    assert!(
        links.iter().any(|l| l["rel"] == "link_as_alias"),
        "links must contain link_as_alias; got: {links:?}"
    );
    assert!(
        links.iter().any(|l| l["rel"] == "define_new"),
        "links must contain define_new; got: {links:?}"
    );
    assert!(
        links.iter().any(|l| l["rel"] == "cancel"),
        "links must contain cancel; got: {links:?}"
    );

    // candidates field must be present (even if empty).
    assert!(
        resp["interaction"]["candidates"].is_array(),
        "candidates must be an empty array; got: {resp}"
    );

    // No workflow instance should have been created.
    let events = audit.snapshot();
    assert!(
        !events.iter().any(|e| e.event_type == "workflow.started"),
        "workflow.started must NOT have been emitted; got events: {:?}",
        events.iter().map(|e| &e.event_type).collect::<Vec<_>>()
    );
}

/// SPEC §30.10.4 — start with traceId + runId in the original args must echo
/// those fields back verbatim in queued_command.args.
#[tokio::test]
async fn queued_command_preserves_trace_and_run_ids() {
    let (server, _audit) = build_server(config_with_unresolved_subject());

    let start_args = json!({
        "definitionId": "pending_wf",
        "input": { "key": "val" },
        "traceId": "trace-abc",
        "runId": "run-xyz"
    });

    let resp = server
        .dispatch_call(call(TOOL_COMMAND, start_args.clone()))
        .await
        .expect("dispatch_call");

    assert_eq!(
        resp["queued_command"]["args"], start_args,
        "queued_command.args must echo ALL original args including traceId/runId"
    );
}

/// SPEC §30.10.5 — link_as_alias must point at `lexicon:<subject>` as its
/// subject, with `aliases_add` carrying the placeholder term.
#[tokio::test]
async fn link_as_alias_has_correct_shape() {
    let (server, _audit) = build_server(config_with_unresolved_subject());

    let resp = server
        .dispatch_call(call(
            TOOL_COMMAND,
            json!({ "definitionId": "pending_wf", "input": {} }),
        ))
        .await
        .expect("dispatch_call");

    let links = resp["links"].as_array().expect("links");
    let alias_link = links
        .iter()
        .find(|l| l["rel"] == "link_as_alias")
        .expect("link_as_alias must be present");

    assert_eq!(alias_link["method"], "praxec.command");
    let subject = alias_link["args"]["subject"]
        .as_str()
        .expect("link_as_alias.args.subject must be a string");
    assert!(
        subject.starts_with("lexicon:"),
        "link_as_alias.args.subject must be namespaced 'lexicon:<term>'; got: {subject}"
    );
    assert!(
        alias_link["args"]["definition"]["aliases_add"].is_array(),
        "link_as_alias.args.definition.aliases_add must be an array"
    );
}

/// SPEC §30.10.5 — define_new link must carry definition_short placeholder.
#[tokio::test]
async fn define_new_link_has_correct_shape() {
    let (server, _audit) = build_server(config_with_unresolved_subject());

    let resp = server
        .dispatch_call(call(
            TOOL_COMMAND,
            json!({ "definitionId": "pending_wf", "input": {} }),
        ))
        .await
        .expect("dispatch_call");

    let links = resp["links"].as_array().expect("links");
    let define_link = links
        .iter()
        .find(|l| l["rel"] == "define_new")
        .expect("define_new must be present");

    assert_eq!(define_link["method"], "praxec.command");
    assert!(
        define_link["args"]["subject"]
            .as_str()
            .is_some_and(|s| s.starts_with("lexicon:")),
        "define_new.args.subject must be 'lexicon:<term>'"
    );
    assert!(
        define_link["args"]["definition"]["definition_short"].is_string(),
        "define_new.args.definition.definition_short must be present"
    );
}

/// SPEC §30.10.4 — cancel link must carry intent + unknown_subject.
#[tokio::test]
async fn cancel_link_has_correct_shape() {
    let (server, _audit) = build_server(config_with_unresolved_subject());

    let resp = server
        .dispatch_call(call(
            TOOL_COMMAND,
            json!({ "definitionId": "pending_wf", "input": {} }),
        ))
        .await
        .expect("dispatch_call");

    let links = resp["links"].as_array().expect("links");
    let cancel_link = links
        .iter()
        .find(|l| l["rel"] == "cancel")
        .expect("cancel must be present");

    assert_eq!(cancel_link["method"], "praxec.command");
    assert_eq!(
        cancel_link["args"]["intent"], "cancel_pending_subject",
        "cancel link must carry intent=cancel_pending_subject"
    );
    assert!(
        cancel_link["args"]["unknown_subject"].is_string(),
        "cancel link must carry unknown_subject"
    );
}

// ── §30.10.4 — resolved subjects proceed normally ────────────────────────────

/// SPEC §30.10.4 — when every subject in the workflow definition IS in the
/// lexicon (no PENDING_DEFINITION placeholder), start proceeds and returns
/// a workflow instance (no interaction field).
#[tokio::test]
async fn praxec_command_start_with_resolved_subjects_proceeds_normally() {
    let (server, _audit) = build_server(config_with_resolved_subjects());

    let resp = server
        .dispatch_call(call(
            TOOL_COMMAND,
            json!({ "definitionId": "resolved_wf", "input": {} }),
        ))
        .await
        .expect("dispatch_call");

    assert!(
        resp.get("interaction").is_none(),
        "resolved workflow must NOT pause with interaction; got: {resp}"
    );
    assert!(
        resp.pointer("/workflow/id").is_some(),
        "resolved workflow must return instance with workflow.id; got: {resp}"
    );
}

/// Workflow with NO scripts/skills/capabilities at all — lexicon is empty,
/// nothing to resolve. Start proceeds normally.
#[tokio::test]
async fn workflow_with_no_subjects_proceeds_normally() {
    let cfg = json!({
        "version": "1.0.0",
        "lexicon": {},
        "workflows": {
            "bare_wf": {
                "initialState": "idle",
                "states": { "idle": { "terminal": true } }
            }
        }
    });
    let (server, _audit) = build_server(cfg);

    let resp = server
        .dispatch_call(call(
            TOOL_COMMAND,
            json!({ "definitionId": "bare_wf", "input": {} }),
        ))
        .await
        .expect("dispatch_call");

    assert!(
        resp.get("interaction").is_none(),
        "bare workflow with no subjects must proceed normally; got: {resp}"
    );
    assert!(
        resp.pointer("/workflow/id").is_some(),
        "bare workflow must return instance; got: {resp}"
    );
}
