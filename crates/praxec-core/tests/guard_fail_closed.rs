//! #2 (v0.0.22 hardening) — governance guards must FAIL CLOSED when their
//! backing store is unwired. A gate that reads evidence / an acknowledgment it
//! cannot look up must deny (return `false`), never pass by default. One atomic
//! assertion per store-dependent guard kind — a newly added store-dependent
//! kind that fails open would need its own test here and would be caught in
//! review against this pattern.

use std::sync::Arc;

use praxec_core::audit::MemoryAuditSink;
use praxec_core::guards::DefaultGuardEvaluator;
use praxec_core::model::{Principal, WorkflowInstance};
use praxec_core::ports::GuardEvaluator;
use serde_json::{Value, json};

fn instance(definition: Value) -> WorkflowInstance {
    WorkflowInstance {
        id: "wf".into(),
        definition_id: "d".into(),
        definition_version: "1".into(),
        definition,
        state: "s".into(),
        version: 1,
        input: json!({}),
        context: json!({}),
        started_at: chrono::Utc::now(),
        run_env: praxec_core::RunEnv::for_test(),
        cancelled_at: None,
        cancelled_reason: None,
        depth: 0,
        parent: None,
    }
}

async fn evaluate_unwired(guard: Value, definition: Value) -> bool {
    // A DefaultGuardEvaluator with NO evidence store and NO ack stores.
    let _audit: Arc<MemoryAuditSink> = Arc::new(MemoryAuditSink::new());
    let ev = DefaultGuardEvaluator::new();
    ev.evaluate(
        &guard,
        &instance(definition),
        &json!({}),
        &Principal::anonymous(),
    )
    .await
    .expect("guard evaluation must not error in these fixtures")
}

#[tokio::test]
async fn evidence_guard_fails_closed_when_evidence_store_unwired() {
    // Non-empty `requires` forces the quorum path (empty requires is vacuously
    // true and would not exercise the store).
    let got = evaluate_unwired(
        json!({ "kind": "evidence", "requires": ["tests_passed"] }),
        json!({}),
    )
    .await;
    assert!(
        !got,
        "evidence guard must deny with no evidence store wired"
    );
}

#[tokio::test]
async fn guidance_ack_guard_fails_closed_when_ack_store_unwired() {
    // The subject must exist in the skills library, else it errors SUBJECT_UNKNOWN
    // instead of reaching the no-store deny arm.
    let got = evaluate_unwired(
        json!({ "kind": "guidance_acknowledged", "subject": "x" }),
        json!({ "_skillsLibrary": { "x": { "hash": "abc" } } }),
    )
    .await;
    assert!(
        !got,
        "guidance_acknowledged must deny with no acknowledgment store wired"
    );
}

#[tokio::test]
async fn script_ack_guard_fails_closed_when_script_store_unwired() {
    let got = evaluate_unwired(
        json!({ "kind": "script_acknowledged", "subject": "x" }),
        json!({ "_scriptsLibrary": { "x": { "hash": "abc" } } }),
    )
    .await;
    assert!(
        !got,
        "script_acknowledged must deny with no script-ack store wired"
    );
}
