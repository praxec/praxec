//! SPEC §30.10.5-7 — resolution handlers for SUBJECT_NEEDS_DEFINITION.
//!
//! Tests the three resolution paths that a resolver calls after receiving a
//! SUBJECT_NEEDS_DEFINITION interaction:
//!
//! A. `link_as_alias`   — `definition.aliases_add` in the define shape
//! B. `define_new`      — normal define shape; upgrades placeholder to real entry
//! C. `cancel`          — drops placeholder via `intent: "cancel_pending_subject"`

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

/// Config with:
/// - a real entry `evidence-pack`
/// - a PENDING_DEFINITION placeholder `evidence-foo` (from a script reference)
/// - a workflow `pending_wf` to trigger SUBJECT_NEEDS_DEFINITION
fn config_with_pending_and_real() -> Value {
    json!({
        "version": "1.0.0",
        "praxec": { "strict_namespacing": false },
        "lexicon": {
            "evidence-pack": {
                "definition_short": "A structured bundle of evidence artifacts.",
                "governance": "human-only"
            }
        },
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

fn build_server(cfg: Value) -> (PraxecServer, Arc<MemoryAuditSink>) {
    let resolved = praxec_core::config::resolve(cfg).expect("resolve");
    let pending = praxec_core::lexicon::pending_subjects_from_resolved(&resolved);
    let lexicon_base = resolved
        .get("lexicon")
        .cloned()
        .unwrap_or_else(|| serde_json::json!({}));
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
    (
        PraxecServer::new(runtime)
            .with_lexicon_writes(true)
            .with_lexicon(lexicon_base)
            .with_pending_subjects(pending),
        audit,
    )
}

// ── A. link_as_alias happy path ───────────────────────────────────────────────

/// SPEC §30.10.7A — alias-add path: unknown placeholder becomes an alias of
/// an existing entry. Placeholder removed; audit event `lexicon.alias_added`.
#[tokio::test]
async fn link_as_alias_happy_path() {
    let (server, audit) = build_server(config_with_pending_and_real());

    let resp = server
        .dispatch_call(call(
            TOOL_COMMAND,
            json!({
                "subject": "lexicon:evidence-pack",
                "definition": { "aliases_add": ["evidence-foo"] }
            }),
        ))
        .await
        .expect("dispatch_call returns Ok");

    // Must not be an error.
    assert!(
        resp.get("error").is_none(),
        "expected success, got error: {resp}"
    );

    // The alias should now appear when looking up evidence-pack.
    let lookup = server
        .dispatch_call(call(
            "praxec.query",
            json!({ "subject": "lexicon:evidence-pack" }),
        ))
        .await
        .expect("lookup");
    let aliases = &lookup["entry"]["aliases"];
    assert!(
        aliases.is_array() && aliases.as_array().unwrap().contains(&json!("evidence-foo")),
        "evidence-foo must appear in evidence-pack's aliases; got entry: {lookup}"
    );

    // The placeholder for evidence-foo must be gone.
    let placeholder_lookup = server
        .dispatch_call(call(
            "praxec.query",
            json!({ "subject": "lexicon:evidence-foo" }),
        ))
        .await
        .expect("lookup");
    // After alias resolution the placeholder entry should be gone — lookup
    // returns null entry (PENDING_DEFINITION removed).
    let entry = &placeholder_lookup["entry"];
    let is_pending = entry.get("state").and_then(Value::as_str) == Some("PENDING_DEFINITION");
    assert!(
        !is_pending,
        "evidence-foo placeholder must be gone after alias resolution; got: {placeholder_lookup}"
    );

    // Audit event lexicon.alias_added must have been emitted.
    let events = audit.snapshot();
    assert!(
        events.iter().any(|e| e.event_type == "lexicon.alias_added"),
        "lexicon.alias_added audit event must be emitted; events: {:?}",
        events.iter().map(|e| &e.event_type).collect::<Vec<_>>()
    );
}

// ── A. link_as_alias collision ────────────────────────────────────────────────

/// SPEC §30.10.7A — alias already claimed in the same bounded context →
/// LEXICON_ALIAS_COLLISION, no mutation, no audit event.
#[tokio::test]
async fn link_as_alias_collision_rejected() {
    // Config: two entries, foo-a already has alias "shared".
    let cfg = json!({
        "version": "1.0.0",
        "lexicon": {
            "foo-a": {
                "definition_short": "First entry.",
                "aliases": ["shared"],
                "governance": "human-only"
            },
            "foo-b": {
                "definition_short": "Second entry.",
                "governance": "human-only"
            }
        },
        "workflows": {
            "bare": {
                "initialState": "idle",
                "states": { "idle": { "terminal": true } }
            }
        }
    });
    let (server, audit) = build_server(cfg);

    let resp = server
        .dispatch_call(call(
            TOOL_COMMAND,
            json!({
                "subject": "lexicon:foo-b",
                "definition": { "aliases_add": ["shared"] }
            }),
        ))
        .await
        .expect("dispatch_call returns Ok");

    // Must return a LEXICON_ALIAS_COLLISION error.
    let code = resp
        .pointer("/error/code")
        .and_then(Value::as_str)
        .unwrap_or("");
    assert_eq!(
        code, "LEXICON_ALIAS_COLLISION",
        "expected LEXICON_ALIAS_COLLISION; got: {resp}"
    );

    // No audit event should have been emitted.
    let events = audit.snapshot();
    assert!(
        !events.iter().any(|e| e.event_type == "lexicon.alias_added"),
        "no audit event must be emitted on collision; events: {:?}",
        events.iter().map(|e| &e.event_type).collect::<Vec<_>>()
    );
}

// ── B. define_new upgrades placeholder ────────────────────────────────────────

/// SPEC §30.10.7B — define_new on a PENDING_DEFINITION placeholder replaces it
/// with a real entry and emits `lexicon.defined`.
#[tokio::test]
async fn define_new_upgrades_placeholder() {
    let (server, audit) = build_server(config_with_pending_and_real());

    let resp = server
        .dispatch_call(call(
            TOOL_COMMAND,
            json!({
                "subject": "lexicon:evidence-foo",
                "definition": {
                    "definition_short": "An evidence artifact for foo.",
                    "governance": "human-only"
                }
            }),
        ))
        .await
        .expect("dispatch_call");

    assert!(resp.get("error").is_none(), "expected success; got: {resp}");

    // Lookup should now return a real entry (no state: PENDING_DEFINITION).
    let lookup = server
        .dispatch_call(call(
            "praxec.query",
            json!({ "subject": "lexicon:evidence-foo" }),
        ))
        .await
        .expect("lookup");
    let state = lookup["entry"].get("state").and_then(Value::as_str);
    assert!(
        state != Some("PENDING_DEFINITION"),
        "entry must not be PENDING_DEFINITION after define_new; got: {lookup}"
    );
    assert_eq!(
        lookup["entry"]["definition_short"],
        json!("An evidence artifact for foo."),
        "definition_short must match; got: {lookup}"
    );

    // Audit: lexicon.defined.
    let events = audit.snapshot();
    assert!(
        events.iter().any(|e| e.event_type == "lexicon.defined"),
        "lexicon.defined must be emitted; events: {:?}",
        events.iter().map(|e| &e.event_type).collect::<Vec<_>>()
    );
}

// ── B. define_new on brand-new (non-placeholder) subject ─────────────────────

/// SPEC §30.10.7B — define_new on a subject that doesn't exist at all creates
/// a fresh entry. Uses a term pre-marked `agent-may-propose` in the lexicon so
/// an anonymous (non-human) agent can write it directly.
#[tokio::test]
async fn define_new_creates_fresh_entry() {
    let cfg = json!({
        "version": "1.0.0",
        // brand-new-term is in the lexicon with agent-may-propose governance,
        // so an agent can update its definition without a human gate.
        "lexicon": {
            "brand-new-term": {
                "definition_short": "placeholder text",
                "governance": "agent-may-propose"
            }
        },
        "workflows": {
            "bare": {
                "initialState": "idle",
                "states": { "idle": { "terminal": true } }
            }
        }
    });
    let (server, audit) = build_server(cfg);

    let resp = server
        .dispatch_call(call(
            TOOL_COMMAND,
            json!({
                "subject": "lexicon:brand-new-term",
                "definition": {
                    "definition_short": "A genuinely new concept.",
                    "governance": "agent-may-propose"
                }
            }),
        ))
        .await
        .expect("dispatch_call");

    assert!(resp.get("error").is_none(), "expected success; got: {resp}");

    // Lookup must find the new entry.
    let lookup = server
        .dispatch_call(call(
            "praxec.query",
            json!({ "subject": "lexicon:brand-new-term" }),
        ))
        .await
        .expect("lookup");
    assert_eq!(
        lookup["entry"]["definition_short"],
        json!("A genuinely new concept."),
        "entry must be findable after define_new; got: {lookup}"
    );

    // Audit: lexicon.defined.
    let events = audit.snapshot();
    assert!(
        events.iter().any(|e| e.event_type == "lexicon.defined"),
        "lexicon.defined must be emitted for fresh entry; events: {:?}",
        events.iter().map(|e| &e.event_type).collect::<Vec<_>>()
    );
}

// ── C. cancel drops placeholder ───────────────────────────────────────────────

/// SPEC §30.10.7C — cancel drops the PENDING_DEFINITION placeholder and emits
/// `lexicon.pending_cancelled`. No entry for the subject should exist after.
#[tokio::test]
async fn cancel_drops_placeholder() {
    let (server, audit) = build_server(config_with_pending_and_real());

    let resp = server
        .dispatch_call(call(
            TOOL_COMMAND,
            json!({
                "intent": "cancel_pending_subject",
                "unknown_subject": "evidence-foo"
            }),
        ))
        .await
        .expect("dispatch_call");

    assert!(resp.get("error").is_none(), "expected success; got: {resp}");

    // The placeholder for evidence-foo must be gone.
    let lookup = server
        .dispatch_call(call(
            "praxec.query",
            json!({ "subject": "lexicon:evidence-foo" }),
        ))
        .await
        .expect("lookup");
    let entry = &lookup["entry"];
    let is_pending = entry.get("state").and_then(Value::as_str) == Some("PENDING_DEFINITION");
    assert!(
        !is_pending,
        "placeholder must be gone after cancel; got: {lookup}"
    );

    // Audit event lexicon.pending_cancelled must have been emitted.
    let events = audit.snapshot();
    assert!(
        events
            .iter()
            .any(|e| e.event_type == "lexicon.pending_cancelled"),
        "lexicon.pending_cancelled must be emitted; events: {:?}",
        events.iter().map(|e| &e.event_type).collect::<Vec<_>>()
    );
}

// ── C. cancel on non-pending subject returns INVALID_RESOLUTION ───────────────

/// SPEC §30.10.9 — cancelling a real (non-placeholder) entry returns
/// INVALID_RESOLUTION, no mutation.
#[tokio::test]
async fn cancel_on_real_entry_returns_invalid_resolution() {
    let (server, audit) = build_server(config_with_pending_and_real());

    let resp = server
        .dispatch_call(call(
            TOOL_COMMAND,
            json!({
                "intent": "cancel_pending_subject",
                "unknown_subject": "evidence-pack"
            }),
        ))
        .await
        .expect("dispatch_call");

    let code = resp
        .pointer("/error/code")
        .and_then(Value::as_str)
        .unwrap_or("");
    assert_eq!(
        code, "INVALID_RESOLUTION",
        "expected INVALID_RESOLUTION for cancel on real entry; got: {resp}"
    );

    // No audit event.
    let events = audit.snapshot();
    assert!(
        !events
            .iter()
            .any(|e| e.event_type == "lexicon.pending_cancelled"),
        "no audit on failed cancel; events: {:?}",
        events.iter().map(|e| &e.event_type).collect::<Vec<_>>()
    );
}
