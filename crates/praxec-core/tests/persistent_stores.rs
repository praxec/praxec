//! File-backed and SQLite-backed `WorkflowStore` round-trip tests.
//!
//! Both implementations share the optimistic-locking semantics of the trait:
//! create + load + save_if_version with the right version, and stale versions
//! are rejected.

use praxec_core::model::WorkflowInstance;
use praxec_core::ports::WorkflowStore;
use praxec_core::store_file::FileWorkflowStore;
use praxec_core::store_sqlite::SqliteWorkflowStore;
use serde_json::json;

fn instance(id: &str, state: &str, version: u64) -> WorkflowInstance {
    instance_with_run_id(id, state, version, None)
}

fn instance_with_run_id(
    id: &str,
    state: &str,
    version: u64,
    run_id: Option<&str>,
) -> WorkflowInstance {
    WorkflowInstance {
        id: id.to_string(),
        definition_id: "demo".into(),
        definition_version: "1.0.0".into(),
        definition: json!({"initialState": "open", "states": {}}),
        state: state.to_string(),
        version,
        input: json!({}),
        context: json!({}),
        started_at: chrono::Utc::now(),
        trace_id: None,
        run_id: run_id.map(str::to_string),
        cancelled_at: None,
        cancelled_reason: None,
        depth: 0,
        parent: None,
    }
}

async fn round_trip(store: &dyn WorkflowStore) {
    let original = instance("wf_a", "open", 0);
    let created = store.create(original.clone()).await.unwrap();
    assert_eq!(created.id, "wf_a");

    let loaded = store.load("wf_a").await.unwrap();
    assert_eq!(loaded.state, "open");
    assert_eq!(loaded.version, 0);

    // Successful version-checked write.
    let mut next = loaded.clone();
    next.state = "running".into();
    next.version = 1;
    let saved = store.save_if_version(next, 0).await.unwrap();
    assert_eq!(saved.state, "running");
    assert_eq!(saved.version, 1);

    // Stale version is rejected.
    let mut stale = saved.clone();
    stale.state = "done".into();
    stale.version = 2;
    let err = store.save_if_version(stale, 99).await.unwrap_err();
    assert!(err.to_string().contains("stale"), "got: {err}");

    // Latest is still version 1.
    let latest = store.load("wf_a").await.unwrap();
    assert_eq!(latest.version, 1);
    assert_eq!(latest.state, "running");
}

#[tokio::test]
async fn file_store_round_trip() {
    let dir = tempfile::tempdir().unwrap();
    let store = FileWorkflowStore::new(dir.path()).unwrap();
    round_trip(&store).await;
}

#[tokio::test]
async fn sqlite_store_round_trip_in_memory() {
    let store = SqliteWorkflowStore::open_in_memory().unwrap();
    round_trip(&store).await;
}

#[tokio::test]
async fn sqlite_store_persists_across_reopen() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("state.sqlite");

    let s1 = SqliteWorkflowStore::open(&path).unwrap();
    s1.create(instance("wf_persist", "open", 0)).await.unwrap();
    drop(s1);

    let s2 = SqliteWorkflowStore::open(&path).unwrap();
    let loaded = s2.load("wf_persist").await.unwrap();
    assert_eq!(loaded.state, "open");
}

#[tokio::test]
async fn file_store_creates_id_collision_error() {
    let dir = tempfile::tempdir().unwrap();
    let store = FileWorkflowStore::new(dir.path()).unwrap();
    store.create(instance("wf_dup", "s", 0)).await.unwrap();
    let err = store.create(instance("wf_dup", "s", 0)).await.unwrap_err();
    assert!(err.to_string().contains("collision"), "got: {err}");
}

#[tokio::test]
async fn sqlite_store_creates_id_collision_error() {
    let store = SqliteWorkflowStore::open_in_memory().unwrap();
    store.create(instance("wf_dup", "s", 0)).await.unwrap();
    let err = store.create(instance("wf_dup", "s", 0)).await.unwrap_err();
    assert!(err.to_string().contains("collision"), "got: {err}");
}

// CMP-006: find_by_run_id must be implemented (not the Ok(None) default) on
// the persistent backends, otherwise the RUN_ID_ALREADY_RUNNING duplicate-run
// guard is dead on file/sqlite deployments.

#[tokio::test]
async fn sqlite_find_by_run_id() {
    let store = SqliteWorkflowStore::open_in_memory().unwrap();
    store
        .create(instance_with_run_id("wf_run", "open", 0, Some("r-abc")))
        .await
        .unwrap();
    // Instance with no run_id must not be matched by any lookup.
    store
        .create(instance_with_run_id("wf_norun", "open", 0, None))
        .await
        .unwrap();

    assert_eq!(
        store.find_by_run_id("r-abc").await.unwrap().as_deref(),
        Some("wf_run")
    );
    assert_eq!(store.find_by_run_id("r-missing").await.unwrap(), None);
}

#[tokio::test]
async fn file_find_by_run_id() {
    let dir = tempfile::tempdir().unwrap();
    let store = FileWorkflowStore::new(dir.path()).unwrap();
    store
        .create(instance_with_run_id("wf_run", "open", 0, Some("r-abc")))
        .await
        .unwrap();
    store
        .create(instance_with_run_id("wf_norun", "open", 0, None))
        .await
        .unwrap();

    assert_eq!(
        store.find_by_run_id("r-abc").await.unwrap().as_deref(),
        Some("wf_run")
    );
    assert_eq!(store.find_by_run_id("r-missing").await.unwrap(), None);
}

#[tokio::test]
async fn file_find_by_run_id_empty_store_is_none() {
    let dir = tempfile::tempdir().unwrap();
    let store = FileWorkflowStore::new(dir.path()).unwrap();
    assert_eq!(store.find_by_run_id("anything").await.unwrap(), None);
}
