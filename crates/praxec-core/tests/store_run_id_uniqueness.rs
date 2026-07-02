//! Atomic run_id uniqueness at the store layer (async-safety prerequisite):
//! two creates with the same run_id → exactly one succeeds.

use praxec_core::model::WorkflowInstance;
use praxec_core::ports::WorkflowStore;
use praxec_core::store::InMemoryWorkflowStore;
use praxec_core::store::SqliteWorkflowStore;
use serde_json::json;

fn instance(id: &str, run_id: &str) -> WorkflowInstance {
    WorkflowInstance {
        id: id.to_string(),
        definition_id: "d".into(),
        definition_version: "1.0.0".into(),
        definition: json!({}),
        state: "s".into(),
        version: 0,
        input: json!({}),
        context: json!({}),
        started_at: chrono::Utc::now(),
        trace_id: None,
        run_id: Some(run_id.to_string()),
        cancelled_at: None,
        cancelled_reason: None,
        depth: 0,
        parent: None,
    }
}

async fn assert_one_wins(store: &dyn WorkflowStore) {
    let a = store.create(instance("wf_a", "run-dup")).await;
    let b = store.create(instance("wf_b", "run-dup")).await;
    assert!(
        a.is_ok() ^ b.is_ok(),
        "exactly one create with a duplicate run_id must succeed; a={:?} b={:?}",
        a.is_ok(),
        b.is_ok()
    );
}

#[tokio::test]
async fn sqlite_create_enforces_run_id_uniqueness() {
    let store = SqliteWorkflowStore::open_in_memory().expect("sqlite");
    assert_one_wins(&store).await;
}

#[tokio::test]
async fn in_memory_create_enforces_run_id_uniqueness() {
    let store = InMemoryWorkflowStore::new();
    assert_one_wins(&store).await;
}
