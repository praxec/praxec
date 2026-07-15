//! Concurrency regression test for the optimistic-concurrency primitive
//! (`WorkflowStore::save_if_version`). Production correctness depends on this
//! serializing concurrent writers: under contention exactly ONE writer at a
//! given expected_version may commit; every other must be rejected as stale,
//! never silently overwrite. This locks that invariant against regressions.

use std::sync::Arc;

use praxec_core::InMemoryWorkflowStore;
use praxec_core::model::WorkflowInstance;
use praxec_core::ports::WorkflowStore;

fn instance(id: &str, version: u64) -> WorkflowInstance {
    WorkflowInstance {
        id: id.into(),
        definition_id: "demo".into(),
        definition_version: "1.0.0".into(),
        definition: serde_json::json!({ "version": "1.0.0" }),
        state: "s".into(),
        version,
        input: serde_json::json!({}),
        context: serde_json::json!({}),
        started_at: chrono::Utc::now(),
        run_env: praxec_core::RunEnv::for_test(),
        cancelled_at: None,
        cancelled_reason: None,
        depth: 0,
        parent: None,
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 8)]
async fn save_if_version_serializes_concurrent_writers() {
    let store = Arc::new(InMemoryWorkflowStore::new());
    store.create(instance("wf", 0)).await.unwrap();

    // 16 writers all race to commit from the SAME observed version (0).
    let mut handles = Vec::new();
    for i in 0..16u64 {
        let store = store.clone();
        handles.push(tokio::spawn(async move {
            let mut next = instance("wf", 1);
            next.context = serde_json::json!({ "writer": i });
            store.save_if_version(next, 0).await
        }));
    }

    let mut committed = 0;
    let mut stale = 0;
    for h in handles {
        match h.await.unwrap() {
            Ok(_) => committed += 1,
            Err(e) => {
                assert!(
                    e.to_string().contains("stale workflow version"),
                    "a losing writer must fail with a stale-version error, not some \
                     other error or a silent overwrite: {e}"
                );
                stale += 1;
            }
        }
    }

    assert_eq!(
        committed, 1,
        "exactly one writer may win the expected_version=0 race"
    );
    assert_eq!(stale, 15, "every other writer must be rejected as stale");

    // The single survivor's write is intact at version 1.
    let final_state = store.load("wf").await.unwrap();
    assert_eq!(final_state.version, 1);
}
