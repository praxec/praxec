//! #3 (v0.0.22 hardening) — differential / metamorphic store parity.
//!
//! The same sequence of operations must yield the same OBSERVABLE state across
//! every `WorkflowStore` backend (in-memory, file, sqlite). Backends diverge in
//! this codebase because sqlite queries serde json paths (`json_extract(...,
//! '$.context._lock_wait')`) while in-memory/file use struct/`.context.get(...)`
//! access — a path typo or a field rename breaks sqlite silently while the
//! in-memory test stays green.
//!
//! This is the exact class that escaped once: the sqlite `list_waiting_on_lock`
//! filter queried the top-level `$._lock_wait` instead of `$.context._lock_wait`,
//! so lock-suspended workflows never resumed on sqlite (prod) deployments while
//! the in-memory test passed. A differential test catches it automatically.
//!
//! NOTE: `list_*` ordering is unspecified and differs by backend (dir scan vs
//! row order vs map iteration), so every comparison is set-based.

use std::collections::BTreeSet;

use praxec_core::model::WorkflowInstance;
use praxec_core::ports::WorkflowStore;
use praxec_core::store::{FileWorkflowStore, InMemoryWorkflowStore, SqliteWorkflowStore};
use serde_json::{Value, json};

fn inst(id: &str, state: &str, context: Value, run_id: Option<&str>) -> WorkflowInstance {
    WorkflowInstance {
        id: id.into(),
        definition_id: "d".into(),
        definition_version: "1".into(),
        definition: json!({}),
        state: state.into(),
        version: 1,
        input: json!({}),
        context,
        started_at: chrono::Utc::now(),
        run_env: praxec_core::RunEnv::new(
            praxec_core::RepoRoot::for_test(),
            run_id.map(str::to_string),
            None,
        ),
        cancelled_at: None,
        cancelled_reason: None,
        depth: 0,
        parent: None,
    }
}

/// The full observable state a caller can see, captured as order-independent
/// sets so backend list-ordering differences don't cause false failures.
#[derive(Debug, PartialEq, Eq)]
struct Snapshot {
    all: BTreeSet<(String, String, u64)>, // (id, state, version)
    waiting_lock: BTreeSet<String>,
    waiting_subworkflow: BTreeSet<String>,
    run_a_lookup: Option<String>,
    missing_run_lookup: Option<String>,
}

async fn snapshot(store: &dyn WorkflowStore) -> Snapshot {
    let all = store
        .list_all()
        .await
        .unwrap()
        .into_iter()
        .map(|i| (i.id, i.state, i.version))
        .collect();
    let waiting_lock = store
        .list_waiting_on_lock()
        .await
        .unwrap()
        .into_iter()
        .map(|i| i.id)
        .collect();
    let waiting_subworkflow = store
        .list_waiting_on_subworkflow()
        .await
        .unwrap()
        .into_iter()
        .map(|i| i.id)
        .collect();
    Snapshot {
        all,
        waiting_lock,
        waiting_subworkflow,
        run_a_lookup: store.find_by_run_id("run-a").await.unwrap(),
        missing_run_lookup: store.find_by_run_id("run-none").await.unwrap(),
    }
}

/// One op sequence exercising every divergence-prone path: run_id lookup,
/// lock-wait filter, subworkflow-wait filter, and an optimistic-version save.
async fn apply_ops(store: &dyn WorkflowStore) {
    store
        .create(inst("a", "s1", json!({}), Some("run-a")))
        .await
        .unwrap();
    store
        .create(inst(
            "b",
            "s1",
            json!({ "_lock_wait": { "lock": "x" } }),
            Some("run-b"),
        ))
        .await
        .unwrap();
    store
        .create(inst(
            "c",
            "s1",
            json!({ "_subworkflow_wait": { "transition": "t", "child_workflow_id": "k" } }),
            None,
        ))
        .await
        .unwrap();
    // Optimistic-version advance on `a`: s1@v1 -> s2@v2.
    let a = store.load("a").await.unwrap();
    let advanced = WorkflowInstance {
        state: "s2".into(),
        version: 2,
        ..a
    };
    store.save_if_version(advanced, 1).await.unwrap();
}

async fn snapshot_of<S: WorkflowStore>(store: S) -> Snapshot {
    apply_ops(&store).await;
    snapshot(&store).await
}

#[tokio::test]
async fn all_backends_agree_on_observable_state() {
    let dir = tempfile::tempdir().unwrap();
    let mem = snapshot_of(InMemoryWorkflowStore::new()).await;
    let file = snapshot_of(FileWorkflowStore::new(dir.path()).unwrap()).await;
    let sql = snapshot_of(SqliteWorkflowStore::open_in_memory().unwrap()).await;

    assert_eq!(mem, file, "in-memory vs file store diverge");
    assert_eq!(mem, sql, "in-memory vs sqlite store diverge");
}

/// Atomic: the archetypal escaped bug — the lock-wait filter must find `b` on
/// EVERY backend (sqlite once queried `$._lock_wait` and silently matched none).
#[tokio::test]
async fn all_backends_agree_on_lock_wait_filter() {
    let dir = tempfile::tempdir().unwrap();
    let want: BTreeSet<String> = ["b".to_string()].into_iter().collect();
    for snap in [
        snapshot_of(InMemoryWorkflowStore::new()).await,
        snapshot_of(FileWorkflowStore::new(dir.path()).unwrap()).await,
        snapshot_of(SqliteWorkflowStore::open_in_memory().unwrap()).await,
    ] {
        assert_eq!(snap.waiting_lock, want, "lock-wait filter divergence");
    }
}

/// Atomic: `find_by_run_id` reads `run_env.run_id` — a serde-path drift would
/// break only sqlite. It must resolve `run-a -> a` on every backend.
#[tokio::test]
async fn all_backends_agree_on_find_by_run_id() {
    let dir = tempfile::tempdir().unwrap();
    for snap in [
        snapshot_of(InMemoryWorkflowStore::new()).await,
        snapshot_of(FileWorkflowStore::new(dir.path()).unwrap()).await,
        snapshot_of(SqliteWorkflowStore::open_in_memory().unwrap()).await,
    ] {
        assert_eq!(snap.run_a_lookup.as_deref(), Some("a"));
        assert_eq!(snap.missing_run_lookup, None);
    }
}
