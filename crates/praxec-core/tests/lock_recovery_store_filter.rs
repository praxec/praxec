//! Regression: `list_waiting_on_lock` on the sqlite store must match the
//! `_lock_wait` record where it actually lives — under `context` — not at the
//! top level. The runtime writes the lock-wait marker into `instance.context`
//! (`runtime_submit.rs`), and `recover_suspended_locks` reads
//! `inst.context.get("_lock_wait")`. A sqlite filter querying the top-level
//! `$._lock_wait` matches nothing, so lock-suspended workflows would never
//! resume after a restart on sqlite-backed (production) deployments.
//!
//! The pre-existing in-memory test (`repo_lock_resume.rs`) exercised
//! `InMemoryWorkflowStore`, whose filter was already correct — which is why the
//! sqlite path bug escaped.

use praxec_core::model::WorkflowInstance;
use praxec_core::ports::WorkflowStore;
use praxec_core::store::SqliteWorkflowStore;

fn suspended_instance(id: &str) -> WorkflowInstance {
    WorkflowInstance {
        id: id.to_string(),
        definition_id: "demo".into(),
        definition_version: "1.0.0".into(),
        definition: serde_json::json!({"initialState": "s", "states": {}}),
        state: "s".into(),
        version: 0,
        input: serde_json::json!({}),
        // Mirror the real shape written by runtime_submit when a transition is
        // blocked on a contended repo lock.
        context: serde_json::json!({
            "_lock_wait": {
                "files": ["src/auth.rs"],
                "blockedBy": ["other"],
                "transition": "edit"
            }
        }),
        started_at: chrono::Utc::now(),
        run_env: praxec_core::RunEnv::for_test(),
        cancelled_at: None,
        cancelled_reason: None,
        depth: 0,
        parent: None,
    }
}

#[tokio::test]
async fn sqlite_list_waiting_on_lock_matches_context_lock_wait() {
    let store = SqliteWorkflowStore::open_in_memory().unwrap();
    store.create(suspended_instance("wf_locked")).await.unwrap();

    let waiting = store.list_waiting_on_lock().await.unwrap();
    assert_eq!(
        waiting.len(),
        1,
        "sqlite list_waiting_on_lock must find the instance whose context._lock_wait is set"
    );
    assert_eq!(waiting[0].id, "wf_locked");
}
