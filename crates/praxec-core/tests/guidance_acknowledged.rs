//! SPEC §5.9 + §17.4 — `guidance_acknowledged` guard. Hash-flip invariant
//! is the TRIZ-bounded semantic teeth (FMECA FM-4).

use std::sync::Arc;

use chrono::Utc;
use praxec_core::guards::DefaultGuardEvaluator;
use praxec_core::model::{Principal, WorkflowInstance};
use praxec_core::ports::{GuardEvaluator, GuidanceAcknowledgmentStore};
use praxec_core::store::InMemoryGuidanceAcknowledgmentStore;
use serde_json::{Value, json};

fn instance_with_skill(subject: &str, hash: &str) -> WorkflowInstance {
    WorkflowInstance {
        id: "wf_test".into(),
        definition_id: "demo".into(),
        definition_version: "0".into(),
        definition: json!({
            "_skillsLibrary": {
                subject: {
                    "verb": "review",
                    "lifecycle": "stable",
                    "body": "ignored",
                    "hash": hash,
                    "source": "config",
                }
            }
        }),
        state: "s".into(),
        version: 0,
        input: json!({}),
        context: json!({}),
        started_at: Utc::now(),
        run_env: praxec_core::RunEnv::for_test(),
        cancelled_at: None,
        cancelled_reason: None,
        depth: 0,
        parent: None,
    }
}

fn guard(subject: &str) -> Value {
    json!({ "kind": "guidance_acknowledged", "subject": subject })
}

// ── Negative: no ack store wired → guard fails ──────────────────────────────

#[tokio::test]
async fn guard_fails_when_no_ack_store_wired() {
    let evaluator = DefaultGuardEvaluator::new();
    let instance = instance_with_skill("review.style.x", "sha256:abc");
    let pass = evaluator
        .evaluate(
            &guard("review.style.x"),
            &instance,
            &json!({}),
            &Principal::anonymous(),
        )
        .await
        .expect("evaluate succeeds");
    assert!(!pass, "without ack store the guard cannot be satisfied");
}

// ── Negative: never described → guard fails ─────────────────────────────────

#[tokio::test]
async fn guard_fails_when_subject_never_described() {
    let ack: Arc<dyn GuidanceAcknowledgmentStore> =
        Arc::new(InMemoryGuidanceAcknowledgmentStore::new());
    let evaluator = DefaultGuardEvaluator::new().with_ack_store(ack);
    let instance = instance_with_skill("review.style.x", "sha256:abc");
    let pass = evaluator
        .evaluate(
            &guard("review.style.x"),
            &instance,
            &json!({}),
            &Principal::anonymous(),
        )
        .await
        .expect("evaluate succeeds");
    assert!(!pass);
}

// ── Positive: described with current hash → guard passes ────────────────────

#[tokio::test]
async fn guard_passes_when_subject_described_with_current_hash() {
    let ack: Arc<dyn GuidanceAcknowledgmentStore> =
        Arc::new(InMemoryGuidanceAcknowledgmentStore::new());
    ack.record("wf_test", "review.style.x", "sha256:abc")
        .await
        .unwrap();
    let evaluator = DefaultGuardEvaluator::new().with_ack_store(ack);
    let instance = instance_with_skill("review.style.x", "sha256:abc");
    let pass = evaluator
        .evaluate(
            &guard("review.style.x"),
            &instance,
            &json!({}),
            &Principal::anonymous(),
        )
        .await
        .expect("evaluate succeeds");
    assert!(pass);
}

// ── Hash-flip invariant (TRIZ teeth, FM-4): ack with stale hash → fails ────

#[tokio::test]
async fn hash_flip_invalidates_prior_ack() {
    let ack: Arc<dyn GuidanceAcknowledgmentStore> =
        Arc::new(InMemoryGuidanceAcknowledgmentStore::new());
    // LLM described the body when its hash was abc...
    ack.record("wf_test", "review.style.x", "sha256:abc")
        .await
        .unwrap();
    let evaluator = DefaultGuardEvaluator::new().with_ack_store(ack);
    // ...but the current snapshot's hash is now def (body was edited).
    let instance = instance_with_skill("review.style.x", "sha256:def");
    let pass = evaluator
        .evaluate(
            &guard("review.style.x"),
            &instance,
            &json!({}),
            &Principal::anonymous(),
        )
        .await
        .expect("evaluate succeeds");
    assert!(!pass, "hash flip must invalidate the prior ack");
}

// ── Cross-workflow leak prevention: ack scoped to workflow_id ──────────────

#[tokio::test]
async fn ack_from_different_workflow_does_not_satisfy_guard() {
    let ack: Arc<dyn GuidanceAcknowledgmentStore> =
        Arc::new(InMemoryGuidanceAcknowledgmentStore::new());
    // Different workflow described it.
    ack.record("OTHER_workflow_id", "review.style.x", "sha256:abc")
        .await
        .unwrap();
    let evaluator = DefaultGuardEvaluator::new().with_ack_store(ack);
    let instance = instance_with_skill("review.style.x", "sha256:abc");
    let pass = evaluator
        .evaluate(
            &guard("review.style.x"),
            &instance,
            &json!({}),
            &Principal::anonymous(),
        )
        .await
        .expect("evaluate succeeds");
    assert!(
        !pass,
        "ack from another workflow must not leak across instances"
    );
}

// ── Edge: subject not in snapshot → GUIDANCE_SUBJECT_UNKNOWN error ─────────

#[tokio::test]
async fn unknown_subject_in_snapshot_surfaces_as_error_not_silent_fail() {
    let ack: Arc<dyn GuidanceAcknowledgmentStore> =
        Arc::new(InMemoryGuidanceAcknowledgmentStore::new());
    let evaluator = DefaultGuardEvaluator::new().with_ack_store(ack);
    let instance = instance_with_skill("review.style.x", "sha256:abc");
    let err = evaluator
        .evaluate(
            &guard("review.style.NOT_IN_SNAPSHOT"),
            &instance,
            &json!({}),
            &Principal::anonymous(),
        )
        .await
        .expect_err("missing subject must error, not silently return false");
    assert!(format!("{err}").contains("GUIDANCE_SUBJECT_UNKNOWN"));
}

// ── Edge: guard with missing `subject` field errors ────────────────────────

#[tokio::test]
async fn guard_without_subject_errors() {
    let ack: Arc<dyn GuidanceAcknowledgmentStore> =
        Arc::new(InMemoryGuidanceAcknowledgmentStore::new());
    let evaluator = DefaultGuardEvaluator::new().with_ack_store(ack);
    let instance = instance_with_skill("review.style.x", "sha256:abc");
    let err = evaluator
        .evaluate(
            &json!({ "kind": "guidance_acknowledged" }),
            &instance,
            &json!({}),
            &Principal::anonymous(),
        )
        .await
        .expect_err("missing subject must error");
    assert!(format!("{err}").contains("subject"));
}
