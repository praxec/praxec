//! Skill → system-message injection for the `kind: llm` executor.
//!
//! The agent/skill/prompt contract: a `kind: llm` step's model call is
//! `Agent` (model, from affinity) + `Skill` (the in-scope skill bodies,
//! as the SYSTEM message) + `Prompt` (the rendered `prompt_template`, as
//! the USER message). Skills are declared by scope (`skills:` at workflow
//! / state / transition level) and resolved from the snapshot-stamped
//! `_skillsLibrary`. These tests drive the executor with the capturing
//! `MockProviderFactory` and assert on the exact `Context` it streams.

mod common;

use std::sync::Arc;

use async_trait::async_trait;
use common::mock_provider::{CapturedTurn, MockProviderFactory, MockProviderScenarios};
use praxec_core::audit::NullAuditSink;
use praxec_core::error::{ExecutorError, LlmErrorCode};
use praxec_core::model::{ExecuteRequest, ExecuteResult, Principal, WorkflowInstance};
use praxec_core::ports::{Executor, TransitionResolver};
use praxec_llm_executor::LlmExecutor;
use serde_json::{Value, json};

/// Resolver offering exactly the `advance` transition — matches the tool
/// call the `happy_path` mock scenario emits, so execution reaches the
/// provider call (and thus builds + streams a `Context`).
struct AdvanceResolver;

#[async_trait]
impl TransitionResolver for AdvanceResolver {
    async fn available_transitions(
        &self,
        _instance: &WorkflowInstance,
        _principal: &Principal,
    ) -> anyhow::Result<Vec<Value>> {
        Ok(vec![json!({ "rel": "advance" })])
    }
}

const TONE_BODY: &str = "Use active voice and second person throughout.";

/// An instance whose definition declares a workflow-scope skill and carries
/// the matching snapshot-stamped `_skillsLibrary` entry.
fn instance_with_workflow_skill() -> WorkflowInstance {
    WorkflowInstance {
        id: "wf_skill".into(),
        definition_id: "demo".into(),
        definition_version: "1.0.0".into(),
        definition: json!({
            "initialState": "drafting",
            "skills": ["review.style.tone"],
            "states": { "drafting": {} },
            "_skillsLibrary": {
                "review.style.tone": {
                    "verb": "review",
                    "lifecycle": "stable",
                    "body": TONE_BODY,
                    "hash": "sha256:deadbeef",
                    "source": "config"
                }
            }
        }),
        state: "drafting".into(),
        version: 0,
        input: json!({}),
        context: json!({}),
        started_at: chrono::Utc::now(),
        trace_id: None,
        run_id: None,
        cancelled_at: None,
        cancelled_reason: None,
        depth: 0,
        parent: None,
    }
}

fn request(instance: WorkflowInstance) -> ExecuteRequest {
    ExecuteRequest {
        workflow: instance,
        transition: None,
        arguments: json!({}),
        executor_config: json!({
            "model": "anthropic:claude-sonnet-4-6",
            "prompt_template": "Draft the release notes."
        }),
        idempotency_key: None,
        correlation_id: None,
    }
}

/// The system (skill) preamble of a captured turn, if any.
fn system_content(turn: &CapturedTurn) -> Option<String> {
    turn.system.clone()
}

/// The user (rendered-prompt) message of a captured turn.
fn user_text(turn: &CapturedTurn) -> Option<String> {
    Some(turn.prompt.clone())
}

/// Build an instance from a raw definition + current state.
fn instance(definition: Value, state: &str) -> WorkflowInstance {
    WorkflowInstance {
        id: "wf_skill".into(),
        definition_id: "demo".into(),
        definition_version: "1.0.0".into(),
        definition,
        state: state.into(),
        version: 0,
        input: json!({}),
        context: json!({}),
        started_at: chrono::Utc::now(),
        trace_id: None,
        run_id: None,
        cancelled_at: None,
        cancelled_reason: None,
        depth: 0,
        parent: None,
    }
}

/// One snapshot-library entry for `subject` with the given body.
fn lib_entry(body: &str) -> Value {
    json!({
        "verb": "review",
        "lifecycle": "stable",
        "body": body,
        "hash": "sha256:abc",
        "source": "config"
    })
}

/// Run the executor over `instance` (firing `transition`) and return the
/// result plus the capturing factory, so tests can inspect the `Context`.
async fn run(
    instance: WorkflowInstance,
    transition: Option<&str>,
) -> (
    Result<ExecuteResult, ExecutorError>,
    Arc<MockProviderFactory>,
) {
    let factory = Arc::new(MockProviderFactory::single(
        MockProviderScenarios::happy_path(),
    ));
    let resolver: Arc<dyn TransitionResolver> = Arc::new(AdvanceResolver);
    let executor =
        LlmExecutor::with_provider_factory(Arc::new(NullAuditSink), resolver, factory.clone());
    let req = ExecuteRequest {
        workflow: instance,
        transition: transition.map(String::from),
        arguments: json!({}),
        executor_config: json!({
            "model": "anthropic:claude-sonnet-4-6",
            "prompt_template": "Draft the release notes."
        }),
        idempotency_key: None,
        correlation_id: None,
    };
    let res = executor.execute(req).await;
    (res, factory)
}

#[tokio::test]
async fn no_in_scope_skills_produces_no_system_message() {
    // A step with no `skills:` at any scope must send only the user message
    // (byte-identical to the pre-injection behavior).
    let inst = instance(
        json!({ "initialState": "drafting", "states": { "drafting": {} } }),
        "drafting",
    );
    let (res, factory) = run(inst, None).await;
    res.expect("happy path must succeed");
    let ctx = &factory.turns_seen()[0];
    assert!(
        system_content(ctx).is_none(),
        "no in-scope skills must mean no system message"
    );
    assert_eq!(ctx.message_count(), 1, "only the user message is sent");
}

#[tokio::test]
async fn scopes_concatenate_workflow_then_state_then_transition_in_order() {
    let def = json!({
        "initialState": "drafting",
        "skills": ["review.wf"],
        "states": {
            "drafting": {
                "skills": ["review.state"],
                "transitions": {
                    "advance": { "target": "drafting", "skills": ["review.txn"] }
                }
            }
        },
        "_skillsLibrary": {
            "review.wf": lib_entry("AAA-workflow"),
            "review.state": lib_entry("BBB-state"),
            "review.txn": lib_entry("CCC-transition")
        }
    });
    let (res, factory) = run(instance(def, "drafting"), Some("advance")).await;
    res.expect("happy path");
    let system = system_content(&factory.turns_seen()[0]).expect("system message");
    let (a, b, c) = (
        system.find("AAA-workflow").expect("workflow body"),
        system.find("BBB-state").expect("state body"),
        system.find("CCC-transition").expect("transition body"),
    );
    assert!(
        a < b && b < c,
        "order must be workflow→state→transition: {system}"
    );
}

#[tokio::test]
async fn declared_subject_absent_from_library_fails_loud() {
    // `skills:` names a subject the snapshot never stamped → fail loud,
    // never a silent no-op (the agent's instructions would vanish).
    let def = json!({
        "initialState": "drafting",
        "skills": ["review.ghost"],
        "states": { "drafting": {} },
        "_skillsLibrary": {}
    });
    let (res, _factory) = run(instance(def, "drafting"), None).await;
    match res {
        Err(ExecutorError::Llm(LlmErrorCode::SkillSubjectUnknown, msg)) => {
            assert!(msg.contains("review.ghost"), "names the subject: {msg}");
        }
        other => panic!("expected Llm(SkillSubjectUnknown), got {other:?}"),
    }
}

#[tokio::test]
async fn prompt_template_remains_the_user_message() {
    let inst = instance_with_workflow_skill();
    let (res, factory) = run(inst, None).await;
    res.expect("happy path");
    let ctx = &factory.turns_seen()[0];
    assert_eq!(
        user_text(ctx).as_deref(),
        Some("Draft the release notes."),
        "the rendered prompt_template must still be the user message"
    );
}

#[tokio::test]
async fn skill_body_is_injected_verbatim_not_templated() {
    // A `{{ ... }}` sequence in a skill body must reach the model literally:
    // skills are static instructions, never run through the prompt templater.
    let literal = "Always cite {{ $.context.source }} verbatim.";
    let def = json!({
        "initialState": "drafting",
        "skills": ["review.literal"],
        "states": { "drafting": {} },
        "_skillsLibrary": { "review.literal": lib_entry(literal) }
    });
    let (res, factory) = run(instance(def, "drafting"), None).await;
    res.expect("happy path");
    let system = system_content(&factory.turns_seen()[0]).expect("system message");
    assert!(
        system.contains(literal),
        "skill body must be injected verbatim (no templating): {system}"
    );
}

#[tokio::test]
async fn duplicate_subject_across_scopes_is_injected_once() {
    let def = json!({
        "initialState": "drafting",
        "skills": ["review.dup"],
        "states": {
            "drafting": {
                "skills": ["review.dup"],
                "transitions": { "advance": { "target": "drafting", "skills": ["review.dup"] } }
            }
        },
        "_skillsLibrary": { "review.dup": lib_entry("ONCE-ONLY") }
    });
    let (res, factory) = run(instance(def, "drafting"), Some("advance")).await;
    res.expect("happy path");
    let system = system_content(&factory.turns_seen()[0]).expect("system message");
    assert_eq!(
        system.matches("ONCE-ONLY").count(),
        1,
        "a subject declared at multiple scopes is injected once: {system}"
    );
}

#[tokio::test]
async fn in_scope_skill_becomes_the_system_message() {
    let factory = Arc::new(MockProviderFactory::single(
        MockProviderScenarios::happy_path(),
    ));
    let resolver: Arc<dyn TransitionResolver> = Arc::new(AdvanceResolver);
    let executor =
        LlmExecutor::with_provider_factory(Arc::new(NullAuditSink), resolver, factory.clone());

    executor
        .execute(request(instance_with_workflow_skill()))
        .await
        .expect("happy path must succeed");

    let contexts = factory.turns_seen();
    assert_eq!(contexts.len(), 1, "exactly one provider call expected");
    let system =
        system_content(&contexts[0]).expect("a workflow-scope skill must produce a system message");
    assert!(
        system.contains(TONE_BODY),
        "system message must carry the skill body verbatim; got: {system}"
    );
}

#[tokio::test]
async fn deprecated_lifecycle_skill_is_still_injected() {
    // A deprecated skill is injected (with a warn log) — not dropped.
    let def = json!({
        "initialState": "drafting",
        "skills": ["review.old"],
        "states": { "drafting": {} },
        "_skillsLibrary": {
            "review.old": {
                "verb": "review", "lifecycle": "deprecated",
                "body": "DEPRECATED-BODY", "hash": "sha256:x", "source": "config"
            }
        }
    });
    let (res, factory) = run(instance(def, "drafting"), None).await;
    res.expect("happy path");
    let system = system_content(&factory.turns_seen()[0]).expect("system message");
    assert!(
        system.contains("DEPRECATED-BODY"),
        "deprecated skill must still be injected: {system}"
    );
}

#[tokio::test]
async fn empty_skill_body_fails_loud() {
    // A scoped subject whose snapshot body is blank must fail loud, not
    // silently inject nothing.
    let def = json!({
        "initialState": "drafting",
        "skills": ["review.empty"],
        "states": { "drafting": {} },
        "_skillsLibrary": {
            "review.empty": {
                "verb": "review", "lifecycle": "stable",
                "body": "   ", "hash": "sha256:x", "source": "config"
            }
        }
    });
    let (res, _factory) = run(instance(def, "drafting"), None).await;
    match res {
        Err(ExecutorError::Llm(LlmErrorCode::SkillBodyMissing, msg)) => {
            assert!(msg.contains("review.empty"), "names the subject: {msg}")
        }
        other => panic!("expected Llm(SkillBodyMissing), got {other:?}"),
    }
}
