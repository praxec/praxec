//! Tests for `{{ }}` template interpolation in `goal` / `guidance` strings.
//!
//! SPEC v2 §5.2: placeholders of the form `{{ $.path }}` are resolved against
//! the live workflow instance at render time. Unresolved paths render as a
//! marked stub. Interpolation is single-pass and non-recursive.

use praxec_core::audit::{AuditSink, MemoryAuditSink};
use praxec_core::guards::DefaultGuardEvaluator;
use praxec_core::model::{Principal, StartWorkflow};
use praxec_core::store::{ConfigDefinitionStore, InMemoryWorkflowStore};
use praxec_core::WorkflowRuntime;
use serde_json::json;
use std::sync::Arc;

// ── test harness ─────────────────────────────────────────────────────────────

/// Minimal registry that returns `None` for every executor kind — sufficient
/// for tests that never reach an executor step.
struct NoopRegistry;
impl praxec_core::ExecutorRegistry for NoopRegistry {
    fn get(&self, _kind: &str) -> Option<Arc<dyn praxec_core::Executor>> {
        None
    }
}

fn build_runtime(config: serde_json::Value) -> (WorkflowRuntime, Arc<MemoryAuditSink>) {
    let definitions = Arc::new(ConfigDefinitionStore::from_config(&config));
    let store = Arc::new(InMemoryWorkflowStore::new());
    let executors = Arc::new(NoopRegistry);
    let guards = Arc::new(DefaultGuardEvaluator::new());
    let audit = Arc::new(MemoryAuditSink::new());
    let runtime = WorkflowRuntime::new(
        definitions,
        store,
        executors,
        guards,
        audit.clone() as Arc<dyn AuditSink>,
    );
    (runtime, audit)
}

// ── test 1 ────────────────────────────────────────────────────────────────────
// A resolved placeholder is replaced with the context value.

#[tokio::test]
async fn guidance_string_interpolates_context() {
    let cfg = json!({
        "version": "1.0.0",
        "workflows": {
            "wf": {
                "initialState": "check",
                "initialContext": { "someKey": "hello-world" },
                "states": {
                    "check": {
                        "goal": "Current key is {{ $.context.someKey }}",
                        "guidance": "Value from context: {{ $.context.someKey }}",
                        "transitions": {
                            "proceed": {
                                "target": "done",
                                "actor": "agent"
                            }
                        }
                    },
                    "done": { "terminal": true }
                }
            }
        }
    });

    let (runtime, _) = build_runtime(cfg);
    let resp = runtime
        .start(StartWorkflow {
            definition_id: "wf".into(),
            input: json!({}),
            principal: Principal::anonymous(),
            trace_id: None,
            run_id: None,
            depth: 0,
            parent: None,
        })
        .await
        .unwrap();

    assert_eq!(resp["workflow"]["state"], "check");

    let goal = resp["guidance"]["goal"].as_str().expect("goal present");
    assert!(
        !goal.contains("{{"),
        "goal must not contain raw placeholder, got: {goal}"
    );
    assert!(
        goal.contains("hello-world"),
        "goal must contain interpolated value, got: {goal}"
    );

    let instructions = resp["guidance"]["instructions"]
        .as_str()
        .expect("instructions present");
    assert!(
        !instructions.contains("{{"),
        "instructions must not contain raw placeholder, got: {instructions}"
    );
    assert!(
        instructions.contains("hello-world"),
        "instructions must contain interpolated value, got: {instructions}"
    );
}

// ── test 2 ────────────────────────────────────────────────────────────────────
// An unresolved placeholder renders as a marked stub; response is still produced.

#[tokio::test]
async fn unresolved_placeholder_renders_stub_not_error() {
    let cfg = json!({
        "version": "1.0.0",
        "workflows": {
            "wf": {
                "initialState": "check",
                "states": {
                    "check": {
                        "guidance": "Count is {{ $.context.missingKey }} items",
                        "transitions": {
                            "proceed": {
                                "target": "done",
                                "actor": "agent"
                            }
                        }
                    },
                    "done": { "terminal": true }
                }
            }
        }
    });

    let (runtime, _) = build_runtime(cfg);
    // Must not return an error even though the context key is absent.
    let resp = runtime
        .start(StartWorkflow {
            definition_id: "wf".into(),
            input: json!({}),
            principal: Principal::anonymous(),
            trace_id: None,
            run_id: None,
            depth: 0,
            parent: None,
        })
        .await
        .expect("response must be produced even with unresolved placeholder");

    assert_eq!(resp["workflow"]["state"], "check");

    let instructions = resp["guidance"]["instructions"]
        .as_str()
        .expect("guidance instructions present");

    // Stub format: (lastSegment: unset)
    assert!(
        instructions.contains("(missingKey: unset)"),
        "unresolved placeholder should render as stub, got: {instructions}"
    );
    // The raw placeholder must not appear verbatim.
    assert!(
        !instructions.contains("{{"),
        "raw placeholder must not appear, got: {instructions}"
    );
}

// ── test 3 ────────────────────────────────────────────────────────────────────
// A context value that itself looks like a template is NOT re-expanded.

#[tokio::test]
async fn template_value_not_re_expanded() {
    let cfg = json!({
        "version": "1.0.0",
        "workflows": {
            "wf": {
                "initialState": "check",
                "initialContext": {
                    "x": "42",
                    "tricky": "{{ $.context.x }}"
                },
                "states": {
                    "check": {
                        "guidance": "Tricky value is: {{ $.context.tricky }}",
                        "transitions": {
                            "proceed": {
                                "target": "done",
                                "actor": "agent"
                            }
                        }
                    },
                    "done": { "terminal": true }
                }
            }
        }
    });

    let (runtime, _) = build_runtime(cfg);
    let resp = runtime
        .start(StartWorkflow {
            definition_id: "wf".into(),
            input: json!({}),
            principal: Principal::anonymous(),
            trace_id: None,
            run_id: None,
            depth: 0,
            parent: None,
        })
        .await
        .unwrap();

    let instructions = resp["guidance"]["instructions"]
        .as_str()
        .expect("guidance instructions present");

    // The substituted value "{{ $.context.x }}" must appear literally —
    // it must NOT be recursively expanded to "42".
    assert!(
        instructions.contains("{{ $.context.x }}"),
        "substituted template-like value must appear verbatim, got: {instructions}"
    );
    assert!(
        !instructions.contains("42"),
        "templated value must not be recursively re-expanded: {instructions}"
    );
    // The outer placeholder for 'tricky' must be gone.
    // After substitution the string contains the literal "{{ $.context.x }}"
    // which looks like a placeholder — but that's the VALUE, not a residual
    // unrendered placeholder. We verify the instructions don't contain
    // the outer "$.context.tricky" raw token.
    assert!(
        !instructions.contains("$.context.tricky"),
        "outer placeholder must be consumed, got: {instructions}"
    );
}

// ── test 4 ────────────────────────────────────────────────────────────────────
// SPEC v2 §5.5: a state referencing a `skills:` entry surfaces a
// `guidance.refs` entry `{verb, subject}`; `gateway.describe(subject)` returns
// the body.

#[tokio::test]
async fn response_surfaces_guidance_refs() {
    use praxec_core::discovery::{DiscoveryIndex, InMemoryDiscoveryIndex};

    let cfg = json!({
        "version": "1.0.0",
        // Lexicon entries for the subjects referenced in skills keys so the
        // pre-start walk (SPEC §30.10.4) does not block the workflow start.
        "lexicon": {
            "style.house-voice": { "definition_short": "Voice and tone guidelines." },
            "editorial.checklist": { "definition_short": "Editorial quality checklist." }
        },
        "skills": {
            "review.style.house-voice": {
                "verb": "review",
                "lifecycle": "stable",
                "body": "Lead with the reader's problem. Short sentences."
            },
            "review.editorial.checklist": {
                "verb": "review",
                "lifecycle": "stable",
                "body": "1. Verify facts. 2. Cite sources."
            }
        },
        "workflows": {
            "wf": {
                "initialState": "draft",
                "skills": ["review.style.house-voice"],
                "states": {
                    "draft": {
                        "goal": "Write the draft",
                        "skills": ["review.editorial.checklist"],
                        "transitions": {
                            "submit": { "target": "done", "actor": "agent" }
                        }
                    },
                    "done": { "terminal": true }
                }
            }
        }
    });

    // The runtime needs `skills:` in the snapshot — driven through `resolve` so
    // the resolve-time stamping path is exercised.
    let resolved = praxec_core::config::resolve(cfg.clone()).expect("config should resolve");

    let (runtime, _) = build_runtime(resolved.clone());
    let resp = runtime
        .start(StartWorkflow {
            definition_id: "wf".into(),
            input: json!({}),
            principal: Principal::anonymous(),
            trace_id: None,
            run_id: None,
            depth: 0,
            parent: None,
        })
        .await
        .unwrap();

    // The response carries a refs list that includes both scopes' refs.
    let refs = resp["guidance"]["refs"]
        .as_array()
        .expect("guidance.refs must be present");
    let subjects: Vec<&str> = refs.iter().filter_map(|r| r["subject"].as_str()).collect();
    assert!(
        subjects.contains(&"review.style.house-voice"),
        "workflow-scope ref must be surfaced; got: {subjects:?}"
    );
    assert!(
        subjects.contains(&"review.editorial.checklist"),
        "state-scope ref must be surfaced; got: {subjects:?}"
    );

    // Each ref carries the verb from the top-level library.
    for r in refs {
        let subj = r["subject"].as_str().unwrap();
        let verb = r["verb"]
            .as_str()
            .unwrap_or_else(|| panic!("ref must carry verb; got {r}"));
        match subj {
            "review.style.house-voice" => assert_eq!(verb, "review"),
            "review.editorial.checklist" => assert_eq!(verb, "review"),
            other => panic!("unexpected ref subject: {other}"),
        }
    }

    // `gateway.describe(subject)` returns the body via the discovery layer.
    let discovery = InMemoryDiscoveryIndex::from_config(&resolved).unwrap();
    let item = discovery
        .describe("review.style.house-voice")
        .await
        .unwrap()
        .expect("review.style.house-voice should be discoverable");
    let body = serde_json::to_value(&item).unwrap();
    let body_str = body["body"].as_str().expect("describe must return body");
    assert!(
        body_str.contains("reader's problem"),
        "body should be surfaced; got: {body_str}"
    );
    assert_eq!(body["verb"].as_str(), Some("review"));
}

// ── test 5 ────────────────────────────────────────────────────────────────────
// SPEC v2 §5.5: transition-scope `skills:` refs ride on the link object, not
// on the top-level `guidance.refs`, so the model can tell which fragments are
// tied to taking *this specific* transition.

#[tokio::test]
async fn transition_scope_refs_ride_on_link() {
    let cfg = json!({
        "version": "1.0.0",
        // Lexicon entry for the subject referenced by the skills key so the
        // pre-start walk (SPEC §30.10.4) does not block the workflow start.
        "lexicon": {
            "style.tone-for-review": { "definition_short": "Tone guidelines for review." }
        },
        "skills": {
            "review.style.tone-for-review": {
                "verb": "review",
                "lifecycle": "stable",
                "body": "Be terse. Lead with the change, not the rationale."
            }
        },
        "workflows": {
            "wf": {
                "initialState": "draft",
                "states": {
                    "draft": {
                        "goal": "Write the draft",
                        "transitions": {
                            "submit_draft": {
                                "target": "review",
                                "actor": "agent",
                                "skills": ["review.style.tone-for-review"]
                            }
                        }
                    },
                    "review": { "terminal": true }
                }
            }
        }
    });

    let resolved = praxec_core::config::resolve(cfg).expect("config should resolve");
    let (runtime, _) = build_runtime(resolved);
    let resp = runtime
        .start(StartWorkflow {
            definition_id: "wf".into(),
            input: json!({}),
            principal: Principal::anonymous(),
            trace_id: None,
            run_id: None,
            depth: 0,
            parent: None,
        })
        .await
        .unwrap();

    // Top-level guidance.refs must NOT carry the transition-scope ref.
    let top_refs = resp["guidance"]
        .get("refs")
        .and_then(|r| r.as_array())
        .cloned()
        .unwrap_or_default();
    let top_subjects: Vec<&str> = top_refs
        .iter()
        .filter_map(|r| r["subject"].as_str())
        .collect();
    assert!(
        !top_subjects.contains(&"review.style.tone-for-review"),
        "transition-scope ref leaked into guidance.refs (workflow/state-only): {top_subjects:?}"
    );

    // The submit_draft link itself must carry guidance.refs = [{verb, subject}].
    let links = resp["links"].as_array().expect("links present");
    let submit_link = links
        .iter()
        .find(|l| l["rel"].as_str() == Some("submit_draft"))
        .expect("submit_draft link must be present");
    let link_refs = submit_link
        .get("guidance")
        .and_then(|g| g.get("refs"))
        .and_then(|r| r.as_array())
        .expect("link must carry guidance.refs for transition-scope skills");
    assert_eq!(
        link_refs.len(),
        1,
        "expected one ref on link; got {link_refs:?}"
    );
    assert_eq!(link_refs[0]["verb"].as_str(), Some("review"));
    assert_eq!(
        link_refs[0]["subject"].as_str(),
        Some("review.style.tone-for-review")
    );
}
