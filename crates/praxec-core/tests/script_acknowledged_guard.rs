//! SPEC §22 — `script_acknowledged` guard. Mirror of the
//! guidance_acknowledged test contract: hash-flip invalidation is the
//! semantic teeth that makes "review-before-execute" meaningful for
//! destructive scripts (e.g. `deploy.production.rollout`).

use std::sync::Arc;

use chrono::Utc;
use praxec_core::guards::DefaultGuardEvaluator;
use praxec_core::model::{Principal, WorkflowInstance};
use praxec_core::ports::{GuardEvaluator, ScriptAcknowledgmentStore};
use praxec_core::store::InMemoryScriptAcknowledgmentStore;
use serde_json::{json, Value};

fn instance_with_script(subject: &str, hash: &str) -> WorkflowInstance {
    WorkflowInstance {
        id: "wf_test".into(),
        definition_id: "demo".into(),
        definition_version: "0".into(),
        definition: json!({
            "_scriptsLibrary": {
                subject: {
                    "verb": "deploy",
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
        trace_id: None,
        run_id: None,
        cancelled_at: None,
        cancelled_reason: None,
        depth: 0,
        parent: None,
    }
}

fn guard(subject: &str) -> Value {
    json!({ "kind": "script_acknowledged", "subject": subject })
}

// ── Negative: no ack store wired → guard fails ─────────────────────────────

#[tokio::test]
async fn guard_fails_when_no_script_ack_store_wired() {
    let evaluator = DefaultGuardEvaluator::new();
    let instance = instance_with_script("deploy.prod.rollout", "sha256:abc");
    let pass = evaluator
        .evaluate(
            &guard("deploy.prod.rollout"),
            &instance,
            &json!({}),
            &Principal::anonymous(),
        )
        .await
        .expect("evaluate succeeds");
    assert!(!pass, "without script ack store the guard cannot pass");
}

// ── Negative: never described → guard fails ────────────────────────────────

#[tokio::test]
async fn guard_fails_when_script_never_described() {
    let ack: Arc<dyn ScriptAcknowledgmentStore> =
        Arc::new(InMemoryScriptAcknowledgmentStore::new());
    let evaluator = DefaultGuardEvaluator::new().with_script_ack_store(ack);
    let instance = instance_with_script("deploy.prod.rollout", "sha256:abc");
    let pass = evaluator
        .evaluate(
            &guard("deploy.prod.rollout"),
            &instance,
            &json!({}),
            &Principal::anonymous(),
        )
        .await
        .expect("evaluate succeeds");
    assert!(!pass);
}

// ── Positive: described with current hash → guard passes ──────────────────

#[tokio::test]
async fn guard_passes_when_script_described_with_current_hash() {
    let ack: Arc<dyn ScriptAcknowledgmentStore> =
        Arc::new(InMemoryScriptAcknowledgmentStore::new());
    ack.record("wf_test", "deploy.prod.rollout", "sha256:abc")
        .await
        .unwrap();
    let evaluator = DefaultGuardEvaluator::new().with_script_ack_store(ack);
    let instance = instance_with_script("deploy.prod.rollout", "sha256:abc");
    let pass = evaluator
        .evaluate(
            &guard("deploy.prod.rollout"),
            &instance,
            &json!({}),
            &Principal::anonymous(),
        )
        .await
        .expect("evaluate succeeds");
    assert!(pass);
}

// ── Hash flip invalidates a prior ack ──────────────────────────────────────

#[tokio::test]
async fn hash_flip_invalidates_prior_script_acknowledgment() {
    let ack: Arc<dyn ScriptAcknowledgmentStore> =
        Arc::new(InMemoryScriptAcknowledgmentStore::new());
    ack.record("wf_test", "deploy.prod.rollout", "sha256:old_hash_value")
        .await
        .unwrap();
    let evaluator = DefaultGuardEvaluator::new().with_script_ack_store(ack);
    // Instance now carries a NEW hash — the prior ack is stale.
    let instance = instance_with_script("deploy.prod.rollout", "sha256:new_hash_value");
    let pass = evaluator
        .evaluate(
            &guard("deploy.prod.rollout"),
            &instance,
            &json!({}),
            &Principal::anonymous(),
        )
        .await
        .expect("evaluate succeeds");
    assert!(
        !pass,
        "hash flip MUST invalidate the prior acknowledgment (TRIZ-bounded semantic teeth)"
    );
}

// ── Unknown subject → SCRIPT_SUBJECT_UNKNOWN error ────────────────────────

#[tokio::test]
async fn unknown_script_subject_surfaces_clear_error() {
    let ack: Arc<dyn ScriptAcknowledgmentStore> =
        Arc::new(InMemoryScriptAcknowledgmentStore::new());
    let evaluator = DefaultGuardEvaluator::new().with_script_ack_store(ack);
    let instance = instance_with_script("deploy.prod.rollout", "sha256:abc");
    let err = evaluator
        .evaluate(
            &guard("deploy.prod.unrelated"),
            &instance,
            &json!({}),
            &Principal::anonymous(),
        )
        .await
        .expect_err("unknown subject must error");
    let s = format!("{err:?}");
    assert!(s.contains("SCRIPT_SUBJECT_UNKNOWN"), "got: {s}");
    assert!(s.contains("deploy.prod.unrelated"), "got: {s}");
}

// ── Script and guidance acks are namespace-isolated ───────────────────────

#[tokio::test]
async fn script_ack_does_not_satisfy_guidance_guard_and_vice_versa() {
    // This test exists to lock the contract that the two ack stores are
    // distinct keyspaces — i.e. recording a script ack must not satisfy
    // a guidance_acknowledged guard for the same subject.
    use praxec_core::ports::GuidanceAcknowledgmentStore;
    use praxec_core::store::InMemoryGuidanceAcknowledgmentStore;

    let script_ack: Arc<dyn ScriptAcknowledgmentStore> =
        Arc::new(InMemoryScriptAcknowledgmentStore::new());
    script_ack
        .record("wf_test", "build.shared", "sha256:abc")
        .await
        .unwrap();

    // Wire ONLY the script ack store (no guidance ack).
    let guidance_ack: Arc<dyn GuidanceAcknowledgmentStore> =
        Arc::new(InMemoryGuidanceAcknowledgmentStore::new());
    let evaluator = DefaultGuardEvaluator::new()
        .with_script_ack_store(script_ack)
        .with_ack_store(guidance_ack);

    // Build an instance that has the subject in BOTH skill and script
    // libraries (synthetic; real workflows wouldn't double up).
    let instance = WorkflowInstance {
        id: "wf_test".into(),
        definition_id: "demo".into(),
        definition_version: "0".into(),
        definition: json!({
            "_skillsLibrary": {
                "build.shared": {
                    "verb": "review",
                    "lifecycle": "stable",
                    "body": "x",
                    "hash": "sha256:abc",
                    "source": "config",
                }
            },
            "_scriptsLibrary": {
                "build.shared": {
                    "verb": "build",
                    "lifecycle": "stable",
                    "body": "x",
                    "hash": "sha256:abc",
                    "source": "config",
                }
            }
        }),
        state: "s".into(),
        version: 0,
        input: json!({}),
        context: json!({}),
        started_at: Utc::now(),
        trace_id: None,
        run_id: None,
        cancelled_at: None,
        cancelled_reason: None,
        depth: 0,
        parent: None,
    };

    // Script guard passes (the script ack was recorded).
    let script_pass = evaluator
        .evaluate(
            &json!({ "kind": "script_acknowledged", "subject": "build.shared" }),
            &instance,
            &json!({}),
            &Principal::anonymous(),
        )
        .await
        .unwrap();
    assert!(script_pass);

    // Guidance guard FAILS (no guidance ack recorded, despite the script
    // ack having the same subject key).
    let guidance_pass = evaluator
        .evaluate(
            &json!({ "kind": "guidance_acknowledged", "subject": "build.shared" }),
            &instance,
            &json!({}),
            &Principal::anonymous(),
        )
        .await
        .unwrap();
    assert!(
        !guidance_pass,
        "script ack must NOT satisfy guidance guard — distinct keyspaces"
    );
}
