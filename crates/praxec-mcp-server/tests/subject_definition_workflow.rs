//! Comprehensive behavioral test suite for the subject-definition
//! workflow (SPEC §30.10). Each test asserts ONE behavior; names read
//! as a declarative statement of what the system does.
//!
//! Phases covered:
//!   A. Lexicon entry shape — covered by existing tests in
//!      crates/praxec-core/tests/lexicon.rs. This file
//!      focuses on the wire-level (MCP) behaviors.
//!   B. Placeholder creation at config load (Task 3.2).
//!   C. Pre-start subject walk + SUBJECT_NEEDS_DEFINITION response (Task 3.3).
//!   D. Resolution handlers — link_as_alias, define_new, cancel (Task 3.5).
//!   E. End-to-end: placeholder → walk → resolve → retry → success.
//!
//! Each test creates its own fixture via the helpers below. No shared
//! mutable state between tests.

use std::sync::Arc;

use async_trait::async_trait;
use praxec_core::WorkflowRuntime;
use praxec_core::audit::{AuditSink, MemoryAuditSink};
use praxec_core::embeddings::{EmbeddingError, EmbeddingProvider};
use praxec_core::guards::DefaultGuardEvaluator;
use praxec_core::ports::ExecutorRegistry;
use praxec_core::store::{ConfigDefinitionStore, InMemoryWorkflowStore};
use praxec_mcp_server::{PraxecServer, TOOL_COMMAND};
use rmcp::model::{CallToolRequestParams, JsonObject};
use serde_json::{Value, json};

// ── Embedding stubs ───────────────────────────────────────────────────────────

/// Test stub: always returns the same fixed vector regardless of input.
struct FixedVectorEmbedder {
    vector: Vec<f32>,
}

impl FixedVectorEmbedder {
    fn returning(vector: Vec<f32>) -> Arc<Self> {
        Arc::new(Self { vector })
    }
}

#[async_trait]
impl EmbeddingProvider for FixedVectorEmbedder {
    async fn embed(&self, _text: &str) -> Result<Vec<f32>, EmbeddingError> {
        Ok(self.vector.clone())
    }

    fn dimensions(&self) -> usize {
        self.vector.len()
    }

    fn backend_name(&self) -> &'static str {
        "fixed"
    }
}

/// Test stub: always returns an error.
struct FailingEmbedder;

#[async_trait]
impl EmbeddingProvider for FailingEmbedder {
    async fn embed(&self, _text: &str) -> Result<Vec<f32>, EmbeddingError> {
        Err(EmbeddingError::BackendFailed(
            "injected test failure".to_string(),
        ))
    }

    fn dimensions(&self) -> usize {
        0
    }

    fn backend_name(&self) -> &'static str {
        "failing"
    }
}

// ── helpers ───────────────────────────────────────────────────────────────────

struct NoopRegistry;
impl ExecutorRegistry for NoopRegistry {
    fn get(&self, _kind: &str) -> Option<Arc<dyn praxec_core::Executor>> {
        None
    }
}

/// Build a `CallToolRequestParams` for `dispatch_call`.
fn call(tool: &'static str, args: Value) -> CallToolRequestParams {
    let m: JsonObject = args.as_object().cloned().expect("object");
    CallToolRequestParams::new(tool).with_arguments(m)
}

/// Build a server with lexicon writes enabled and the given config.
/// Returns (server, audit_sink) so tests can inspect emitted events.
fn build_server(cfg: Value) -> (PraxecServer, Arc<MemoryAuditSink>) {
    build_server_with_embedder(cfg, None)
}

/// Build a server with lexicon writes enabled, given config, and optional embedder.
fn build_server_with_embedder(
    cfg: Value,
    embedder: Option<Arc<dyn EmbeddingProvider>>,
) -> (PraxecServer, Arc<MemoryAuditSink>) {
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
    );
    let server = PraxecServer::new(runtime)
        .with_lexicon_writes(true)
        .with_lexicon(lexicon_base)
        .with_pending_subjects(pending);
    let server = match embedder {
        Some(e) => server.with_embedder(e),
        None => server,
    };
    (server, audit)
}

/// Config whose workflow *references* an unregistered script subject (the
/// executor names `build.evidence-foo`, which nothing defines and the lexicon
/// doesn't carry) — so `evidence-foo` is genuinely-unknown vocabulary and
/// pending. (Defining the script would resolve it per the §30.10.3 relaxation;
/// the fixture must reference, not define.)
fn config_with_pending_script_subject() -> Value {
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

/// Config whose workflow *references* an unregistered skill subject.
fn config_with_pending_skill_subject() -> Value {
    json!({
        "version": "1.0.0",
        "praxec": { "strict_namespacing": false },
        "lexicon": {},
        "workflows": {
            "skill_pending_wf": {
                "initialState": "idle",
                "states": {
                    "idle": { "transitions": { "go": {
                        "target": "done",
                        "executor": { "kind": "skill", "subject": "plan.my-feature" }
                    } } },
                    "done": { "terminal": true }
                }
            }
        }
    })
}

/// Config with both an authored lexicon entry AND a scripts reference to the
/// same term. The real entry should win — no placeholder created.
fn config_with_registered_script_subject() -> Value {
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
                "body": "#!/usr/bin/env bash\necho hi\n"
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

/// Config with a capabilities block referencing an unregistered subject.
/// The `capabilities:` block subjects are captured before the block is
/// stripped at resolve step 4 and passed into `inject_pending_definitions` —
/// so capability-block subjects ARE visible to the injector. See Gap 1 fix.
fn config_with_pending_capability_subject() -> Value {
    json!({
        "version": "1.0.0",
        "praxec": { "strict_namespacing": false },
        "lexicon": {},
        "capabilities": {
            "invoke.my-cap": {
                "title": "My capability",
                "description": "Does something.",
                "executor": { "kind": "mcp", "connection": "my-svc", "tool": "do_thing" }
            }
        },
        "workflows": {
            "cap_pending_wf": {
                "initialState": "idle",
                "states": { "idle": { "terminal": true } }
            }
        }
    })
}

/// Config with a workflow executor that references an unregistered script subject.
fn config_with_pending_executor_kind_script_subject() -> Value {
    json!({
        "version": "1.0.0",
        "praxec": { "strict_namespacing": false },
        "lexicon": {},
        "workflows": {
            "exec_pending_wf": {
                "initialState": "running",
                "states": {
                    "running": {
                        "onEnter": {
                            "executor": {
                                "kind": "script",
                                "subject": "build.my-script-thing"
                            }
                        },
                        "terminal": true
                    }
                }
            }
        }
    })
}

/// Config with a workflow executor that has a `system:` map key referencing
/// an unregistered subject in verb.subject notation.
fn config_with_pending_executor_map_system_key() -> Value {
    json!({
        "version": "1.0.0",
        "praxec": { "strict_namespacing": false },
        "lexicon": {},
        "workflows": {
            "sysmap_pending_wf": {
                "initialState": "running",
                "states": {
                    "running": {
                        "onEnter": {
                            "map": [
                                {
                                    "system": "invoke.my-system-thing"
                                }
                            ]
                        },
                        "terminal": true
                    }
                }
            }
        }
    })
}

/// Config with both a real entry (`evidence-pack`, lexicon-authored) AND a
/// pending placeholder (`evidence-foo`, *referenced* by a workflow executor but
/// undefined). Two different subjects. (References, not a script definition,
/// per the §30.10.3 relaxation — a defined script subject resolves itself.)
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

// ─────────────────────────────────────────────────────────────────────────────
// Phase B — Placeholder creation at config load
// ─────────────────────────────────────────────────────────────────────────────

/// An unregistered script subject creates a PENDING_DEFINITION placeholder in
/// each workflow's _lexiconLibrary after config resolution.
#[test]
fn placeholder_is_created_when_script_subject_is_unregistered() {
    let cfg = config_with_pending_script_subject();
    let resolved = praxec_core::config::resolve(cfg).expect("resolve");
    let lib = resolved
        .pointer("/workflows/pending_wf/_lexiconLibrary")
        .expect("_lexiconLibrary must be stamped");
    let entry = lib
        .get("evidence-foo")
        .expect("placeholder for evidence-foo must exist");
    assert_eq!(
        entry.get("state").and_then(Value::as_str),
        Some("PENDING_DEFINITION"),
        "entry must have state=PENDING_DEFINITION; got: {entry}"
    );
}

/// An unregistered skill subject creates a PENDING_DEFINITION placeholder.
#[test]
fn placeholder_is_created_when_skill_subject_is_unregistered() {
    let cfg = config_with_pending_skill_subject();
    let resolved = praxec_core::config::resolve(cfg).expect("resolve");
    let lib = resolved
        .pointer("/workflows/skill_pending_wf/_lexiconLibrary")
        .expect("_lexiconLibrary must be stamped");
    let entry = lib
        .get("my-feature")
        .expect("placeholder for my-feature must exist");
    assert_eq!(
        entry.get("state").and_then(Value::as_str),
        Some("PENDING_DEFINITION"),
        "skill subject placeholder must have state=PENDING_DEFINITION; got: {entry}"
    );
}

/// Capability-block subjects are captured before the `capabilities:` block is
/// SPEC §30.10.3 relaxation (commit 900b2f2): a capability defined in the
/// `capabilities:` block resolves its own subject — authoring the capability is
/// the definition, so it does NOT also require a separate lexicon glossary entry
/// and no PENDING_DEFINITION placeholder is created. This supersedes the earlier
/// "Gap 1" behavior, which flagged defined capability subjects as pending.
#[test]
fn a_defined_capability_subject_resolves_without_a_placeholder() {
    let cfg = config_with_pending_capability_subject();
    let resolved = praxec_core::config::resolve(cfg).expect("resolve");
    let state = resolved
        .pointer("/workflows/cap_pending_wf/_lexiconLibrary")
        .and_then(|lib| lib.get("my-cap"))
        .and_then(|entry| entry.get("state"))
        .and_then(Value::as_str);
    assert_ne!(
        state,
        Some("PENDING_DEFINITION"),
        "a defined capability subject must resolve itself, not be flagged pending"
    );
}

/// A workflow executor with kind=script and an unregistered subject creates a
/// PENDING_DEFINITION placeholder.
#[test]
fn placeholder_is_created_when_executor_kind_script_subject_is_unregistered() {
    let cfg = config_with_pending_executor_kind_script_subject();
    let resolved = praxec_core::config::resolve(cfg).expect("resolve");
    let lib = resolved
        .pointer("/workflows/exec_pending_wf/_lexiconLibrary")
        .expect("_lexiconLibrary must be stamped");
    let entry = lib
        .get("my-script-thing")
        .expect("placeholder for my-script-thing must exist");
    assert_eq!(
        entry.get("state").and_then(Value::as_str),
        Some("PENDING_DEFINITION"),
        "executor script subject placeholder must have state=PENDING_DEFINITION; got: {entry}"
    );
}

/// A `system:` key in an executor map that uses verb.subject notation creates a
/// PENDING_DEFINITION placeholder for the unregistered subject portion.
#[test]
fn placeholder_is_created_when_executor_map_system_key_is_unregistered() {
    let cfg = config_with_pending_executor_map_system_key();
    let resolved = praxec_core::config::resolve(cfg).expect("resolve");
    let lib = resolved
        .pointer("/workflows/sysmap_pending_wf/_lexiconLibrary")
        .expect("_lexiconLibrary must be stamped");
    let entry = lib
        .get("my-system-thing")
        .expect("placeholder for my-system-thing must exist");
    assert_eq!(
        entry.get("state").and_then(Value::as_str),
        Some("PENDING_DEFINITION"),
        "system key subject placeholder must have state=PENDING_DEFINITION; got: {entry}"
    );
}

/// A PENDING_DEFINITION placeholder has exactly `state = "PENDING_DEFINITION"`
/// (uppercase, as a string).
#[test]
fn placeholder_has_state_pending_definition_uppercase() {
    let cfg = config_with_pending_script_subject();
    let resolved = praxec_core::config::resolve(cfg).expect("resolve");
    let lib = resolved
        .pointer("/workflows/pending_wf/_lexiconLibrary")
        .expect("library");
    let entry = lib.get("evidence-foo").expect("placeholder");
    // Value must be exactly the string "PENDING_DEFINITION" (uppercase).
    assert_eq!(
        entry["state"],
        json!("PENDING_DEFINITION"),
        "state must be the uppercase string literal PENDING_DEFINITION; got: {entry}"
    );
}

/// A PENDING_DEFINITION placeholder defaults to `governance: "human-only"`.
#[test]
fn placeholder_has_governance_human_only_by_default() {
    let cfg = config_with_pending_script_subject();
    let resolved = praxec_core::config::resolve(cfg).expect("resolve");
    let lib = resolved
        .pointer("/workflows/pending_wf/_lexiconLibrary")
        .expect("library");
    let entry = lib.get("evidence-foo").expect("placeholder");
    assert_eq!(
        entry.get("governance").and_then(Value::as_str),
        Some("human-only"),
        "placeholder governance must default to human-only; got: {entry}"
    );
}

/// When a subject is both referenced in scripts AND in the lexicon, no
/// placeholder is created — the authored entry wins.
#[test]
fn no_placeholder_is_created_when_subject_is_registered() {
    let cfg = config_with_registered_script_subject();
    let resolved = praxec_core::config::resolve(cfg).expect("resolve");
    let lib = resolved
        .pointer("/workflows/resolved_wf/_lexiconLibrary")
        .expect("library");
    let entry = lib.get("evidence-foo").expect("entry must exist");
    assert_ne!(
        entry.get("state").and_then(Value::as_str),
        Some("PENDING_DEFINITION"),
        "registered subject must not have PENDING_DEFINITION state; got: {entry}"
    );
}

/// When a subject is both referenced and registered, the authored entry carries
/// `definition_short` (it is a full lexicon entry, not a placeholder).
#[test]
fn authored_entry_wins_when_subject_is_both_referenced_and_registered() {
    let cfg = config_with_registered_script_subject();
    let resolved = praxec_core::config::resolve(cfg).expect("resolve");
    let lib = resolved
        .pointer("/workflows/resolved_wf/_lexiconLibrary")
        .expect("library");
    let entry = lib.get("evidence-foo").expect("entry must exist");
    assert!(
        entry.get("definition_short").is_some(),
        "authored entry must carry definition_short; got: {entry}"
    );
}

// ─────────────────────────────────────────────────────────────────────────────
// Phase C — Pre-start subject walk + SUBJECT_NEEDS_DEFINITION response
// ─────────────────────────────────────────────────────────────────────────────

/// Starting a workflow whose subjects are all resolved creates a workflow
/// instance (no SUBJECT_NEEDS_DEFINITION interaction).
#[tokio::test]
async fn start_with_resolved_subjects_creates_workflow_instance() {
    let (server, _) = build_server(config_with_registered_script_subject());
    let resp = server
        .dispatch_call(call(
            TOOL_COMMAND,
            json!({ "definitionId": "resolved_wf", "input": {} }),
        ))
        .await
        .expect("dispatch_call");
    assert!(
        resp.pointer("/workflow/id").is_some(),
        "resolved workflow must return instance with workflow.id; got: {resp}"
    );
}

/// Starting a workflow with one pending subject returns the SUBJECT_NEEDS_DEFINITION
/// interaction kind.
#[tokio::test]
async fn start_with_one_pending_subject_returns_subject_needs_definition() {
    let (server, _) = build_server(config_with_pending_script_subject());
    let resp = server
        .dispatch_call(call(
            TOOL_COMMAND,
            json!({ "definitionId": "pending_wf", "input": {} }),
        ))
        .await
        .expect("dispatch_call returns Ok — SUBJECT_NEEDS_DEFINITION is structured");
    assert_eq!(
        resp["interaction"]["kind"], "SUBJECT_NEEDS_DEFINITION",
        "expected SUBJECT_NEEDS_DEFINITION; got: {resp}"
    );
}

/// Starting a workflow with a pending subject does NOT create a workflow
/// instance (no `workflow.started` audit event).
#[tokio::test]
async fn start_with_pending_subject_does_not_create_workflow_instance() {
    let (server, audit) = build_server(config_with_pending_script_subject());
    let _ = server
        .dispatch_call(call(
            TOOL_COMMAND,
            json!({ "definitionId": "pending_wf", "input": {} }),
        ))
        .await
        .expect("dispatch_call");
    let events = audit.snapshot();
    assert!(
        !events.iter().any(|e| e.event_type == "workflow.started"),
        "workflow.started must NOT fire when subject is pending; events: {:?}",
        events.iter().map(|e| &e.event_type).collect::<Vec<_>>()
    );
}

/// The SUBJECT_NEEDS_DEFINITION response includes an `unknown_subject` string
/// field naming the placeholder term.
#[tokio::test]
async fn subject_needs_definition_response_includes_unknown_subject_field() {
    let (server, _) = build_server(config_with_pending_script_subject());
    let resp = server
        .dispatch_call(call(
            TOOL_COMMAND,
            json!({ "definitionId": "pending_wf", "input": {} }),
        ))
        .await
        .expect("dispatch_call");
    assert!(
        resp["interaction"]["unknown_subject"].is_string(),
        "unknown_subject must be a string; got: {resp}"
    );
}

/// The SUBJECT_NEEDS_DEFINITION response includes `context.encountered_in`
/// prefixed with `workflow:`.
#[tokio::test]
async fn subject_needs_definition_response_includes_encountered_in_workflow_id() {
    let (server, _) = build_server(config_with_pending_script_subject());
    let resp = server
        .dispatch_call(call(
            TOOL_COMMAND,
            json!({ "definitionId": "pending_wf", "input": {} }),
        ))
        .await
        .expect("dispatch_call");
    let encountered_in = resp["interaction"]["context"]["encountered_in"]
        .as_str()
        .expect("encountered_in must be a string");
    assert!(
        encountered_in.starts_with("workflow:"),
        "encountered_in must start with 'workflow:'; got: {encountered_in}"
    );
}

/// The SUBJECT_NEEDS_DEFINITION response includes a `bounded_context` field
/// (may be null/missing, but the context object is present).
#[tokio::test]
async fn subject_needs_definition_response_includes_bounded_context_field() {
    let (server, _) = build_server(config_with_pending_script_subject());
    let resp = server
        .dispatch_call(call(
            TOOL_COMMAND,
            json!({ "definitionId": "pending_wf", "input": {} }),
        ))
        .await
        .expect("dispatch_call");
    // `context` object must be present; bounded_context inside it may be null.
    assert!(
        resp["interaction"]["context"].is_object(),
        "interaction.context must be an object; got: {resp}"
    );
}

/// The SUBJECT_NEEDS_DEFINITION response includes an empty `candidates` array.
#[tokio::test]
async fn subject_needs_definition_response_includes_empty_candidates_array() {
    let (server, _) = build_server(config_with_pending_script_subject());
    let resp = server
        .dispatch_call(call(
            TOOL_COMMAND,
            json!({ "definitionId": "pending_wf", "input": {} }),
        ))
        .await
        .expect("dispatch_call");
    assert!(
        resp["interaction"]["candidates"].is_array(),
        "candidates must be an array; got: {resp}"
    );
}

/// The SUBJECT_NEEDS_DEFINITION response echoes the original command args
/// verbatim in `queued_command.args`.
#[tokio::test]
async fn subject_needs_definition_response_echoes_queued_command_args_verbatim() {
    let (server, _) = build_server(config_with_pending_script_subject());
    let start_args = json!({ "definitionId": "pending_wf", "input": { "key": "val" } });
    let resp = server
        .dispatch_call(call(TOOL_COMMAND, start_args.clone()))
        .await
        .expect("dispatch_call");
    assert_eq!(
        resp["queued_command"]["args"], start_args,
        "queued_command.args must echo original args verbatim; got: {resp}"
    );
}

/// The queued command args preserve camelCase field names (e.g. `definitionId`,
/// `traceId`, `runId`) — JSON serde must not snake_case them.
#[tokio::test]
async fn subject_needs_definition_response_preserves_camel_case_in_queued_args() {
    let (server, _) = build_server(config_with_pending_script_subject());
    let start_args = json!({
        "definitionId": "pending_wf",
        "input": {},
        "traceId": "trace-abc",
        "runId": "run-xyz"
    });
    let resp = server
        .dispatch_call(call(TOOL_COMMAND, start_args.clone()))
        .await
        .expect("dispatch_call");
    // The keys must appear exactly as supplied (camelCase).
    let queued = &resp["queued_command"]["args"];
    assert!(
        queued.get("definitionId").is_some(),
        "queued_command.args must contain camelCase 'definitionId'; got: {resp}"
    );
    assert!(
        queued.get("traceId").is_some(),
        "queued_command.args must contain camelCase 'traceId'; got: {resp}"
    );
    assert!(
        queued.get("runId").is_some(),
        "queued_command.args must contain camelCase 'runId'; got: {resp}"
    );
}

/// The SUBJECT_NEEDS_DEFINITION response includes a `link_as_alias` HATEOAS link.
#[tokio::test]
async fn subject_needs_definition_response_includes_link_as_alias_link() {
    let (server, _) = build_server(config_with_pending_script_subject());
    let resp = server
        .dispatch_call(call(TOOL_COMMAND, json!({ "definitionId": "pending_wf" })))
        .await
        .expect("dispatch_call");
    let links = resp["links"].as_array().expect("links must be an array");
    assert!(
        links.iter().any(|l| l["rel"] == "link_as_alias"),
        "links must contain link_as_alias; got: {links:?}"
    );
}

/// The SUBJECT_NEEDS_DEFINITION response includes a `define_new` HATEOAS link.
#[tokio::test]
async fn subject_needs_definition_response_includes_define_new_link() {
    let (server, _) = build_server(config_with_pending_script_subject());
    let resp = server
        .dispatch_call(call(TOOL_COMMAND, json!({ "definitionId": "pending_wf" })))
        .await
        .expect("dispatch_call");
    let links = resp["links"].as_array().expect("links");
    assert!(
        links.iter().any(|l| l["rel"] == "define_new"),
        "links must contain define_new; got: {links:?}"
    );
}

/// The SUBJECT_NEEDS_DEFINITION response includes a `cancel` HATEOAS link.
#[tokio::test]
async fn subject_needs_definition_response_includes_cancel_link() {
    let (server, _) = build_server(config_with_pending_script_subject());
    let resp = server
        .dispatch_call(call(TOOL_COMMAND, json!({ "definitionId": "pending_wf" })))
        .await
        .expect("dispatch_call");
    let links = resp["links"].as_array().expect("links");
    assert!(
        links.iter().any(|l| l["rel"] == "cancel"),
        "links must contain cancel; got: {links:?}"
    );
}

/// Every resolution link points at `praxec.command` as its method.
#[tokio::test]
async fn subject_needs_definition_link_args_use_praxec_command_method() {
    let (server, _) = build_server(config_with_pending_script_subject());
    let resp = server
        .dispatch_call(call(TOOL_COMMAND, json!({ "definitionId": "pending_wf" })))
        .await
        .expect("dispatch_call");
    let links = resp["links"].as_array().expect("links");
    for link in links {
        assert_eq!(
            link["method"], "praxec.command",
            "all resolution links must use praxec.command method; got: {link}"
        );
    }
}

/// The `link_as_alias` link args use a `lexicon:` namespaced subject.
#[tokio::test]
async fn link_as_alias_args_use_lexicon_namespace_subject() {
    let (server, _) = build_server(config_with_pending_script_subject());
    let resp = server
        .dispatch_call(call(TOOL_COMMAND, json!({ "definitionId": "pending_wf" })))
        .await
        .expect("dispatch_call");
    let links = resp["links"].as_array().expect("links");
    let alias_link = links
        .iter()
        .find(|l| l["rel"] == "link_as_alias")
        .expect("link_as_alias present");
    let subject = alias_link["args"]["subject"]
        .as_str()
        .expect("link_as_alias.args.subject must be a string");
    assert!(
        subject.starts_with("lexicon:"),
        "link_as_alias.args.subject must be namespaced 'lexicon:<term>'; got: {subject}"
    );
}

/// The `define_new` link args use a `lexicon:` namespaced subject.
#[tokio::test]
async fn define_new_args_use_lexicon_namespace_subject() {
    let (server, _) = build_server(config_with_pending_script_subject());
    let resp = server
        .dispatch_call(call(TOOL_COMMAND, json!({ "definitionId": "pending_wf" })))
        .await
        .expect("dispatch_call");
    let links = resp["links"].as_array().expect("links");
    let define_link = links
        .iter()
        .find(|l| l["rel"] == "define_new")
        .expect("define_new present");
    let subject = define_link["args"]["subject"]
        .as_str()
        .expect("define_new.args.subject must be a string");
    assert!(
        subject.starts_with("lexicon:"),
        "define_new.args.subject must be namespaced 'lexicon:<term>'; got: {subject}"
    );
}

/// The `cancel` link args include `unknown_subject` matching the placeholder term.
#[tokio::test]
async fn cancel_args_include_unknown_subject_matching_placeholder() {
    let (server, _) = build_server(config_with_pending_script_subject());
    let resp = server
        .dispatch_call(call(TOOL_COMMAND, json!({ "definitionId": "pending_wf" })))
        .await
        .expect("dispatch_call");
    let links = resp["links"].as_array().expect("links");
    let cancel_link = links
        .iter()
        .find(|l| l["rel"] == "cancel")
        .expect("cancel present");
    let unknown = cancel_link["args"]["unknown_subject"]
        .as_str()
        .expect("cancel.args.unknown_subject must be a string");
    // Must match the placeholder term that was detected.
    let detected = resp["interaction"]["unknown_subject"]
        .as_str()
        .expect("unknown_subject in interaction");
    assert_eq!(
        unknown, detected,
        "cancel.args.unknown_subject must match interaction.unknown_subject"
    );
}

/// `dispatch_call` returns `Ok(Value)` (not `Err`) for a SUBJECT_NEEDS_DEFINITION
/// response — it is a structured response, not a protocol error.
#[tokio::test]
async fn dispatch_returns_ok_not_err_for_subject_needs_definition() {
    let (server, _) = build_server(config_with_pending_script_subject());
    let result = server
        .dispatch_call(call(
            TOOL_COMMAND,
            json!({ "definitionId": "pending_wf", "input": {} }),
        ))
        .await;
    // Must be Ok(...), not Err(...).
    assert!(
        result.is_ok(),
        "dispatch_call must return Ok for SUBJECT_NEEDS_DEFINITION, not a protocol error; got: {result:?}"
    );
}

// ─────────────────────────────────────────────────────────────────────────────
// Phase D — Resolution handlers
// ─────────────────────────────────────────────────────────────────────────────

/// link_as_alias adds the unknown subject to the target entry's aliases array.
#[tokio::test]
async fn link_as_alias_adds_unknown_subject_to_existing_entry_aliases() {
    let (server, _) = build_server(config_with_pending_and_real());
    // Link evidence-foo as an alias of evidence-pack.
    let _ = server
        .dispatch_call(call(
            TOOL_COMMAND,
            json!({
                "subject": "lexicon:evidence-pack",
                "definition": { "aliases_add": ["evidence-foo"] }
            }),
        ))
        .await
        .expect("dispatch_call");
    // Look up evidence-pack to verify aliases contain evidence-foo.
    let lookup = server
        .dispatch_call(call(
            "praxec.query",
            json!({ "subject": "lexicon:evidence-pack" }),
        ))
        .await
        .expect("lookup");
    let aliases = lookup["entry"]["aliases"]
        .as_array()
        .expect("aliases must be an array");
    assert!(
        aliases.contains(&json!("evidence-foo")),
        "evidence-foo must appear in evidence-pack aliases; got: {aliases:?}"
    );
}

/// link_as_alias removes the placeholder entry for the added alias subject.
#[tokio::test]
async fn link_as_alias_removes_placeholder_when_attached_to_existing_entry() {
    let (server, _) = build_server(config_with_pending_and_real());
    let _ = server
        .dispatch_call(call(
            TOOL_COMMAND,
            json!({
                "subject": "lexicon:evidence-pack",
                "definition": { "aliases_add": ["evidence-foo"] }
            }),
        ))
        .await
        .expect("dispatch_call");
    // Lookup the former placeholder — it must no longer be PENDING_DEFINITION.
    let lookup = server
        .dispatch_call(call(
            "praxec.query",
            json!({ "subject": "lexicon:evidence-foo" }),
        ))
        .await
        .expect("lookup");
    let is_pending =
        lookup["entry"].get("state").and_then(Value::as_str) == Some("PENDING_DEFINITION");
    assert!(
        !is_pending,
        "evidence-foo placeholder must be gone after alias resolution; got: {lookup}"
    );
}

/// link_as_alias emits a `lexicon.alias_added` audit event.
#[tokio::test]
async fn link_as_alias_emits_lexicon_alias_added_audit_event() {
    let (server, audit) = build_server(config_with_pending_and_real());
    let _ = server
        .dispatch_call(call(
            TOOL_COMMAND,
            json!({
                "subject": "lexicon:evidence-pack",
                "definition": { "aliases_add": ["evidence-foo"] }
            }),
        ))
        .await
        .expect("dispatch_call");
    let events = audit.snapshot();
    assert!(
        events.iter().any(|e| e.event_type == "lexicon.alias_added"),
        "lexicon.alias_added audit event must be emitted; events: {:?}",
        events.iter().map(|e| &e.event_type).collect::<Vec<_>>()
    );
}

/// link_as_alias rejects a collision with an existing alias (same bounded
/// context; alias already belongs to another entry).
#[tokio::test]
async fn link_as_alias_rejects_collision_with_existing_alias() {
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
    let (server, _) = build_server(cfg);
    let resp = server
        .dispatch_call(call(
            TOOL_COMMAND,
            json!({
                "subject": "lexicon:foo-b",
                "definition": { "aliases_add": ["shared"] }
            }),
        ))
        .await
        .expect("dispatch_call");
    assert_eq!(
        resp.pointer("/error/code").and_then(Value::as_str),
        Some("LEXICON_ALIAS_COLLISION"),
        "expected LEXICON_ALIAS_COLLISION; got: {resp}"
    );
}

/// link_as_alias rejects a collision where the new alias matches the canonical
/// term name of another entry in the same bounded context.
#[tokio::test]
async fn link_as_alias_rejects_collision_with_canonical_term_of_another_entry() {
    // evidence-pack is a real canonical term; trying to add it as an alias
    // to connector (same no-context group) must collide.
    let cfg = json!({
        "version": "1.0.0",
        "lexicon": {
            "evidence-pack": {
                "definition_short": "A bundle of artefacts.",
                "governance": "human-only"
            },
            "connector": {
                "definition_short": "Integration unit.",
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
    let (server, _) = build_server(cfg);
    // Try to add "evidence-pack" as an alias of connector — collision with canonical.
    let resp = server
        .dispatch_call(call(
            TOOL_COMMAND,
            json!({
                "subject": "lexicon:connector",
                "definition": { "aliases_add": ["evidence-pack"] }
            }),
        ))
        .await
        .expect("dispatch_call");
    assert_eq!(
        resp.pointer("/error/code").and_then(Value::as_str),
        Some("LEXICON_ALIAS_COLLISION"),
        "expected LEXICON_ALIAS_COLLISION when alias matches canonical of another entry; got: {resp}"
    );
}

/// link_as_alias allows an alias to overlap with an alias of a term in a
/// DIFFERENT bounded context (cross-context overlap is permitted).
#[tokio::test]
async fn link_as_alias_allows_alias_overlap_across_bounded_contexts() {
    // risk-ctx-a has alias "risk" in bounded_context "deployment".
    // risk-ctx-b is in bounded_context "billing".
    // Adding alias "risk" to risk-ctx-b must succeed (different contexts).
    let cfg = json!({
        "version": "1.0.0",
        "lexicon": {
            "risk-ctx-a": {
                "definition_short": "Risk in deployment.",
                "bounded_context": "deployment",
                "aliases": ["risk"],
                "governance": "human-only"
            },
            "risk-ctx-b": {
                "definition_short": "Risk in billing.",
                "bounded_context": "billing",
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
    let (server, _) = build_server(cfg);
    let resp = server
        .dispatch_call(call(
            TOOL_COMMAND,
            json!({
                "subject": "lexicon:risk-ctx-b",
                "definition": { "aliases_add": ["risk"] }
            }),
        ))
        .await
        .expect("dispatch_call");
    // No collision error — cross-context alias overlap is allowed.
    assert!(
        resp.get("error").is_none(),
        "cross-context alias overlap must NOT produce an error; got: {resp}"
    );
}

/// define_new upgrades a PENDING_DEFINITION placeholder to a real lexicon entry.
#[tokio::test]
async fn define_new_upgrades_placeholder_to_real_entry() {
    let (server, _) = build_server(config_with_pending_and_real());
    let _ = server
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
    // After define_new, lookup must return a real entry (not PENDING_DEFINITION).
    let lookup = server
        .dispatch_call(call(
            "praxec.query",
            json!({ "subject": "lexicon:evidence-foo" }),
        ))
        .await
        .expect("lookup");
    assert_eq!(
        lookup["entry"]["definition_short"],
        json!("An evidence artifact for foo."),
        "define_new must replace placeholder with real entry; got: {lookup}"
    );
}

/// After define_new resolves a pending subject, a retry of the original start
/// SUCCEEDS. The runtime's pre-start walk now consults the live `pending_subjects`
/// set (shared between PraxecServer and WorkflowRuntime via the same Arc),
/// so removing the subject from pending_subjects immediately lifts the block —
/// no config reload needed. (Gap 2 fix — SPEC §30.10.4.)
#[tokio::test]
async fn after_define_new_resolution_retry_of_original_start_succeeds() {
    let (server, _) = build_server(config_with_pending_and_real());
    // upgrade evidence-foo from placeholder to real
    let _ = server
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
    // Retry: evidence-foo is now resolved — the start must succeed.
    let resp = server
        .dispatch_call(call(
            TOOL_COMMAND,
            json!({ "definitionId": "pending_wf", "input": {} }),
        ))
        .await
        .expect("retry dispatch_call");
    assert!(
        resp.pointer("/workflow/id").is_some(),
        "retry after define_new must create a workflow instance (Gap 2 fix); got: {resp}"
    );
}

/// define_new on a subject that doesn't exist as a placeholder creates a fresh
/// entry in the overlay. (Tests the non-placeholder path — `agent-may-propose`
/// governance so an anonymous principal can write it.)
#[tokio::test]
async fn define_new_creates_fresh_entry_when_subject_was_not_pending() {
    let cfg = json!({
        "version": "1.0.0",
        "lexicon": {
            "fresh-term": {
                "definition_short": "placeholder",
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
    let (server, _) = build_server(cfg);
    let resp = server
        .dispatch_call(call(
            TOOL_COMMAND,
            json!({
                "subject": "lexicon:fresh-term",
                "definition": {
                    "definition_short": "A genuinely new concept.",
                    "governance": "agent-may-propose"
                }
            }),
        ))
        .await
        .expect("dispatch_call");
    assert!(
        resp.get("error").is_none(),
        "define_new on non-pending subject with agent-may-propose must succeed; got: {resp}"
    );
    // Verify the entry is findable.
    let lookup = server
        .dispatch_call(call(
            "praxec.query",
            json!({ "subject": "lexicon:fresh-term" }),
        ))
        .await
        .expect("lookup");
    assert_eq!(
        lookup["entry"]["definition_short"],
        json!("A genuinely new concept."),
        "fresh entry must be findable after define_new; got: {lookup}"
    );
}

/// define_new emits a `lexicon.defined` audit event.
#[tokio::test]
async fn define_new_emits_lexicon_defined_audit_event() {
    let (server, audit) = build_server(config_with_pending_and_real());
    let _ = server
        .dispatch_call(call(
            TOOL_COMMAND,
            json!({
                "subject": "lexicon:evidence-foo",
                "definition": {
                    "definition_short": "An evidence artifact.",
                    "governance": "human-only"
                }
            }),
        ))
        .await
        .expect("dispatch_call");
    let events = audit.snapshot();
    assert!(
        events.iter().any(|e| e.event_type == "lexicon.defined"),
        "lexicon.defined must be emitted by define_new; events: {:?}",
        events.iter().map(|e| &e.event_type).collect::<Vec<_>>()
    );
}

/// define_new bypasses the governance gate for a PENDING_DEFINITION placeholder:
/// an anonymous (agent) principal can upgrade a placeholder even though the
/// default governance is `human-only`.
#[tokio::test]
async fn define_new_bypasses_governance_gate_for_placeholder_upgrade() {
    // config_with_pending_and_real puts evidence-foo as PENDING_DEFINITION
    // with default governance = human-only. An anonymous (agent) call to
    // define_new must succeed without a LEXICON_DEFINE_REQUIRES_HUMAN error.
    let (server, _) = build_server(config_with_pending_and_real());
    // dispatch_call uses the anonymous principal (Principal::anonymous()).
    let resp = server
        .dispatch_call(call(
            TOOL_COMMAND,
            json!({
                "subject": "lexicon:evidence-foo",
                "definition": {
                    "definition_short": "Foo evidence artifact — agent-authored.",
                    "governance": "human-only"
                }
            }),
        ))
        .await
        .expect("dispatch_call");
    // Must not return a governance gate error.
    let code = resp
        .pointer("/error/code")
        .and_then(Value::as_str)
        .unwrap_or("");
    assert_ne!(
        code, "LEXICON_DEFINE_REQUIRES_HUMAN",
        "define_new on placeholder must bypass governance gate; got: {resp}"
    );
    assert!(
        resp.get("error").is_none(),
        "define_new on placeholder must succeed for anonymous principal; got: {resp}"
    );
}

/// cancel removes the PENDING_DEFINITION placeholder from the pending-subjects
/// set.
#[tokio::test]
async fn cancel_removes_pending_definition_placeholder() {
    let (server, _) = build_server(config_with_pending_and_real());
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
    assert!(
        resp.get("error").is_none(),
        "cancel must succeed for a known placeholder; got: {resp}"
    );
    // Lookup must no longer show PENDING_DEFINITION state.
    let lookup = server
        .dispatch_call(call(
            "praxec.query",
            json!({ "subject": "lexicon:evidence-foo" }),
        ))
        .await
        .expect("lookup");
    let is_pending =
        lookup["entry"].get("state").and_then(Value::as_str) == Some("PENDING_DEFINITION");
    assert!(
        !is_pending,
        "placeholder must be gone after cancel; got: {lookup}"
    );
}

/// cancel emits a `lexicon.pending_cancelled` audit event.
#[tokio::test]
async fn cancel_emits_lexicon_pending_cancelled_audit_event() {
    let (server, audit) = build_server(config_with_pending_and_real());
    let _ = server
        .dispatch_call(call(
            TOOL_COMMAND,
            json!({
                "intent": "cancel_pending_subject",
                "unknown_subject": "evidence-foo"
            }),
        ))
        .await
        .expect("dispatch_call");
    let events = audit.snapshot();
    assert!(
        events
            .iter()
            .any(|e| e.event_type == "lexicon.pending_cancelled"),
        "lexicon.pending_cancelled must be emitted by cancel; events: {:?}",
        events.iter().map(|e| &e.event_type).collect::<Vec<_>>()
    );
}

/// cancel on a real (non-placeholder) entry returns INVALID_RESOLUTION.
#[tokio::test]
async fn cancel_returns_invalid_resolution_when_subject_is_not_pending() {
    let (server, _) = build_server(config_with_pending_and_real());
    // evidence-pack is a real authored entry, not a placeholder.
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
    assert_eq!(
        resp.pointer("/error/code").and_then(Value::as_str),
        Some("INVALID_RESOLUTION"),
        "cancel on real entry must return INVALID_RESOLUTION; got: {resp}"
    );
}

/// cancel on a subject that does not exist in the lexicon at all returns
/// INVALID_RESOLUTION (the cancel gate distinguishes "is a placeholder" from
/// "unknown subject").
#[tokio::test]
async fn cancel_returns_invalid_resolution_when_subject_does_not_exist() {
    let (server, _) = build_server(config_with_pending_and_real());
    let resp = server
        .dispatch_call(call(
            TOOL_COMMAND,
            json!({
                "intent": "cancel_pending_subject",
                "unknown_subject": "totally-unknown-subject"
            }),
        ))
        .await
        .expect("dispatch_call");
    assert_eq!(
        resp.pointer("/error/code").and_then(Value::as_str),
        Some("INVALID_RESOLUTION"),
        "cancel on non-existent subject must return INVALID_RESOLUTION; got: {resp}"
    );
}

// ─────────────────────────────────────────────────────────────────────────────
// Phase E — End-to-end: placeholder → resolve → retry → success
// ─────────────────────────────────────────────────────────────────────────────

/// Full happy path via define_new: placeholder → SUBJECT_NEEDS_DEFINITION →
/// define_new → retry succeeds. The runtime consults the live pending_subjects
/// set (not the baked-in snapshot), so resolution is immediately reflected.
/// (Gap 2 fix — SPEC §30.10.4.)
#[tokio::test]
async fn after_define_new_resolution_retry_of_original_start_succeeds_e2e() {
    let (server, _) = build_server(config_with_pending_and_real());

    // Step 1: attempt start — must get SUBJECT_NEEDS_DEFINITION.
    let pause_resp = server
        .dispatch_call(call(
            TOOL_COMMAND,
            json!({ "definitionId": "pending_wf", "input": {} }),
        ))
        .await
        .expect("dispatch_call step 1");
    assert_eq!(
        pause_resp["interaction"]["kind"], "SUBJECT_NEEDS_DEFINITION",
        "step 1 must pause with SUBJECT_NEEDS_DEFINITION; got: {pause_resp}"
    );

    // Step 2: define_new resolution — this succeeds.
    let define_resp = server
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
        .expect("dispatch_call step 2");
    assert!(
        define_resp.get("error").is_none(),
        "define_new resolution must succeed; got: {define_resp}"
    );

    // Step 3: retry original start — must now succeed (Gap 2 fix).
    let retry_resp = server
        .dispatch_call(call(
            TOOL_COMMAND,
            json!({ "definitionId": "pending_wf", "input": {} }),
        ))
        .await
        .expect("dispatch_call step 3");
    assert!(
        retry_resp.pointer("/workflow/id").is_some(),
        "retry after define_new must create workflow instance (Gap 2 fix); got: {retry_resp}"
    );
}

/// Full path via link_as_alias: placeholder → SUBJECT_NEEDS_DEFINITION →
/// link_as_alias → retry succeeds. The alias resolution removes evidence-foo
/// from pending_subjects; the runtime's live-set check sees it as resolved.
/// (Gap 2 fix — SPEC §30.10.4.)
#[tokio::test]
async fn after_link_as_alias_resolution_retry_of_original_start_succeeds() {
    let (server, _) = build_server(config_with_pending_and_real());

    // Step 1: attempt start — pauses.
    let pause_resp = server
        .dispatch_call(call(
            TOOL_COMMAND,
            json!({ "definitionId": "pending_wf", "input": {} }),
        ))
        .await
        .expect("dispatch_call step 1");
    assert_eq!(
        pause_resp["interaction"]["kind"], "SUBJECT_NEEDS_DEFINITION",
        "step 1 must pause; got: {pause_resp}"
    );

    // Step 2: link_as_alias resolution — this succeeds.
    let alias_resp = server
        .dispatch_call(call(
            TOOL_COMMAND,
            json!({
                "subject": "lexicon:evidence-pack",
                "definition": { "aliases_add": ["evidence-foo"] }
            }),
        ))
        .await
        .expect("dispatch_call step 2");
    assert!(
        alias_resp.get("error").is_none(),
        "link_as_alias must succeed; got: {alias_resp}"
    );

    // Step 3: retry — must now succeed (Gap 2 fix).
    let retry_resp = server
        .dispatch_call(call(
            TOOL_COMMAND,
            json!({ "definitionId": "pending_wf", "input": {} }),
        ))
        .await
        .expect("dispatch_call step 3");
    assert!(
        retry_resp.pointer("/workflow/id").is_some(),
        "retry after link_as_alias must create workflow instance (Gap 2 fix); got: {retry_resp}"
    );
}

/// After cancel, the subject is removed from `PraxecServer.pending_subjects`
/// (and therefore also from the shared `WorkflowRuntime.pending_subjects` Arc).
/// The retry of the original start SUCCEEDS because the runtime's live-set check
/// no longer finds the subject as pending.
///
/// Semantic choice: cancel says "operator acknowledges this subject is unresolved
/// and explicitly lifts the block for this server's lifetime". The config still
/// references the unregistered subject (the workflow's `_lexiconLibrary` snapshot
/// still carries PENDING_DEFINITION), but the live set is the source of truth for
/// blocking. A fresh server from the same config would re-create the placeholder.
/// (Gap 2 fix — SPEC §30.10.4.)
#[tokio::test]
async fn after_cancel_retry_of_original_start_succeeds_because_subject_removed_from_pending_set() {
    let (server, _) = build_server(config_with_pending_and_real());

    // Step 1: pause on SUBJECT_NEEDS_DEFINITION.
    let pause_resp = server
        .dispatch_call(call(
            TOOL_COMMAND,
            json!({ "definitionId": "pending_wf", "input": {} }),
        ))
        .await
        .expect("dispatch_call step 1");
    assert_eq!(
        pause_resp["interaction"]["kind"], "SUBJECT_NEEDS_DEFINITION",
        "step 1 must pause; got: {pause_resp}"
    );

    // Step 2: cancel — removes placeholder from pending set in MCP server layer
    // (and therefore from the runtime's live set too, via the shared Arc).
    let cancel_resp = server
        .dispatch_call(call(
            TOOL_COMMAND,
            json!({
                "intent": "cancel_pending_subject",
                "unknown_subject": "evidence-foo"
            }),
        ))
        .await
        .expect("dispatch_call step 2");
    assert!(
        cancel_resp.get("error").is_none(),
        "cancel must succeed; got: {cancel_resp}"
    );

    // Step 3: retry — must now succeed because cancel lifted the block by
    // removing the subject from pending_subjects (Gap 2 fix).
    let retry_resp = server
        .dispatch_call(call(
            TOOL_COMMAND,
            json!({ "definitionId": "pending_wf", "input": {} }),
        ))
        .await
        .expect("dispatch_call step 3");
    assert!(
        retry_resp.pointer("/workflow/id").is_some(),
        "retry after cancel must create workflow instance — cancel lifts the block \
         by removing the subject from the live pending set (Gap 2 fix); got: {retry_resp}"
    );
}

// ─────────────────────────────────────────────────────────────────────────────
// Phase F — EmbeddingProvider wiring (SPEC §30.10.10)
// ─────────────────────────────────────────────────────────────────────────────

/// define_new with a configured embedder stores `_embedding` on the entry.
#[tokio::test]
async fn define_with_embedder_stores_embedding_on_entry() {
    let fixed_vec = vec![1.0_f32, 0.0, 0.0];
    let embedder = FixedVectorEmbedder::returning(fixed_vec.clone());
    let (server, _) = build_server_with_embedder(
        config_with_pending_and_real(),
        Some(embedder as Arc<dyn EmbeddingProvider>),
    );

    // Define the pending subject.
    let _ = server
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

    // Lookup and verify _embedding is stored.
    let lookup = server
        .dispatch_call(call(
            "praxec.query",
            json!({ "subject": "lexicon:evidence-foo" }),
        ))
        .await
        .expect("lookup");

    let stored = lookup
        .pointer("/entry/_embedding")
        .and_then(Value::as_array)
        .expect("_embedding must be present on the entry after define with embedder");

    let parsed: Vec<f32> = stored
        .iter()
        .map(|v| v.as_f64().expect("f64") as f32)
        .collect();
    assert_eq!(
        parsed, fixed_vec,
        "stored _embedding must match the fixed-vector embedder output; got: {lookup}"
    );
}

/// define_new with an embedder that fails returns EMBEDDING_BACKEND_FAILED structured error.
#[tokio::test]
async fn define_with_failing_embedder_returns_embedding_backend_failed() {
    let failing: Arc<dyn EmbeddingProvider> = Arc::new(FailingEmbedder);
    let (server, _) = build_server_with_embedder(config_with_pending_and_real(), Some(failing));

    let resp = server
        .dispatch_call(call(
            TOOL_COMMAND,
            json!({
                "subject": "lexicon:evidence-foo",
                "definition": {
                    "definition_short": "An evidence artifact.",
                    "governance": "human-only"
                }
            }),
        ))
        .await
        .expect("dispatch_call returns Ok — error is structured");

    assert_eq!(
        resp.pointer("/error/code").and_then(Value::as_str),
        Some("EMBEDDING_BACKEND_FAILED"),
        "define with failing embedder must return EMBEDDING_BACKEND_FAILED; got: {resp}"
    );
}

/// After alias_add with an embedder, the entry's `_embedding` reflects the
/// new text (canonical + aliases). The vector changes between define and alias_add.
#[tokio::test]
async fn alias_add_with_embedder_reembeds_entry() {
    // Use a counter-based embedder: returns a distinct vector on each call
    // so we can verify a second embed call happened.
    use std::sync::atomic::{AtomicUsize, Ordering};

    struct CountingEmbedder {
        call_count: AtomicUsize,
    }

    #[async_trait]
    impl EmbeddingProvider for CountingEmbedder {
        async fn embed(&self, _text: &str) -> Result<Vec<f32>, EmbeddingError> {
            let n = self.call_count.fetch_add(1, Ordering::SeqCst);
            // Each call returns a distinct vector so we can tell them apart.
            Ok(vec![n as f32, 0.0, 0.0])
        }
        fn dimensions(&self) -> usize {
            3
        }
        fn backend_name(&self) -> &'static str {
            "counting"
        }
    }

    let embedder: Arc<dyn EmbeddingProvider> = Arc::new(CountingEmbedder {
        call_count: AtomicUsize::new(0),
    });
    let (server, _) = build_server_with_embedder(config_with_pending_and_real(), Some(embedder));

    // Step 1: define evidence-pack (already in base, use evidence-foo as a new term).
    let _ = server
        .dispatch_call(call(
            TOOL_COMMAND,
            json!({
                "subject": "lexicon:evidence-foo",
                "definition": {
                    "definition_short": "Initial definition.",
                    "governance": "human-only"
                }
            }),
        ))
        .await
        .expect("define");

    // Capture embedding after define (call_count was 0 → embedding = [0.0, 0.0, 0.0]).
    let lookup_after_define = server
        .dispatch_call(call(
            "praxec.query",
            json!({ "subject": "lexicon:evidence-foo" }),
        ))
        .await
        .expect("lookup after define");
    let emb_after_define = lookup_after_define
        .pointer("/entry/_embedding/0")
        .and_then(Value::as_f64)
        .expect("_embedding[0] after define") as f32;

    // Step 2: alias_add → triggers re-embed (call_count becomes 1 → embedding = [1.0, 0.0, 0.0]).
    let _ = server
        .dispatch_call(call(
            TOOL_COMMAND,
            json!({
                "subject": "lexicon:evidence-foo",
                "definition": { "aliases_add": ["ef"] }
            }),
        ))
        .await
        .expect("alias_add");

    let lookup_after_alias = server
        .dispatch_call(call(
            "praxec.query",
            json!({ "subject": "lexicon:evidence-foo" }),
        ))
        .await
        .expect("lookup after alias_add");
    let emb_after_alias = lookup_after_alias
        .pointer("/entry/_embedding/0")
        .and_then(Value::as_f64)
        .expect("_embedding[0] after alias_add") as f32;

    assert!(
        (emb_after_alias - emb_after_define).abs() > 0.5,
        "embedding must change after alias_add (re-embed fires); \
         before={emb_after_define}, after={emb_after_alias}"
    );
}

/// Full end-to-end: configure embedder; trigger SUBJECT_NEEDS_DEFINITION;
/// assert a candidate has `match_kind: "semantic"`.
///
/// The lexicon has an entry with a stored `_embedding` that is near-identical
/// to the vector the embedder returns for the unknown subject — so Tier 3 fires
/// and the candidate shows up as `match_kind: "semantic"`.
#[tokio::test]
async fn subject_needs_definition_includes_semantic_candidate_when_embedder_configured() {
    // near_x has cosine ≈ 0.9994 with unit_x — well above the 0.85 threshold.
    let unit_x = vec![1.0_f32, 0.0, 0.0];
    let near_x = vec![0.9994_f32, 0.035, 0.0];

    // Config: evidence-candidate has a stored _embedding (near_x).
    // evidence-foo is pending (no definition). The embedder will return
    // unit_x for the unknown subject, giving cosine ≥ 0.85 → semantic hit.
    let cfg = json!({
        "version": "1.0.0",
        "praxec": { "strict_namespacing": false },
        "lexicon": {
            "evidence-candidate": {
                "definition_short": "A semantically similar evidence concept.",
                "governance": "human-only",
                "_embedding": near_x
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
    });

    // Embedder always returns unit_x — the unknown subject's query vector.
    let embedder = FixedVectorEmbedder::returning(unit_x);
    let (server, _) = build_server_with_embedder(cfg, Some(embedder as Arc<dyn EmbeddingProvider>));

    // Trigger SUBJECT_NEEDS_DEFINITION.
    let resp = server
        .dispatch_call(call(
            TOOL_COMMAND,
            json!({ "definitionId": "pending_wf", "input": {} }),
        ))
        .await
        .expect("dispatch_call");

    assert_eq!(
        resp["interaction"]["kind"], "SUBJECT_NEEDS_DEFINITION",
        "expected SUBJECT_NEEDS_DEFINITION interaction; got: {resp}"
    );

    let candidates = resp["interaction"]["candidates"]
        .as_array()
        .expect("candidates must be an array");

    let semantic_candidate = candidates.iter().find(|c| c["match_kind"] == "semantic");

    assert!(
        semantic_candidate.is_some(),
        "candidates must include at least one semantic match when embedder is configured \
         and a stored vector matches; candidates: {candidates:?}"
    );
    assert_eq!(
        semantic_candidate.unwrap()["term"],
        "evidence-candidate",
        "semantic candidate must identify evidence-candidate; candidates: {candidates:?}"
    );
}
