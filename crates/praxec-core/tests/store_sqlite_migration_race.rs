//! Concurrency-safety of the SQLite schema migration.
//!
//! Multiple `praxec serve` processes share one on-disk sqlite store and each
//! runs the `DROP INDEX … / CREATE UNIQUE INDEX …` migration in `open()`. If
//! that migration is not serialized, process A can create the index between
//! process B's DROP and CREATE, so B's CREATE fails "index … already exists"
//! and the gateway boots DEGRADED.
//!
//! This exercises the in-process analogue: N threads open the SAME db file
//! concurrently. Every open MUST succeed.

use praxec_core::ports::WorkflowStore;
use praxec_core::store::SqliteWorkflowStore;
use std::sync::Arc;
use std::sync::Barrier;

/// N concurrent opens of the same on-disk db file must ALL succeed — no
/// "index idx_workflows_run_id already exists" race in the DROP+CREATE
/// migration.
#[test]
fn concurrent_open_never_races_on_index_migration() {
    let tmp = tempfile::NamedTempFile::new().expect("temp db file");
    let path = tmp.path().to_path_buf();

    const N: usize = 16;
    let barrier = Arc::new(Barrier::new(N));
    let mut handles = Vec::with_capacity(N);
    for _ in 0..N {
        let path = path.clone();
        let barrier = barrier.clone();
        handles.push(std::thread::spawn(move || -> anyhow::Result<()> {
            // Line all threads up so their migrations overlap maximally.
            barrier.wait();
            SqliteWorkflowStore::open(&path)?;
            Ok(())
        }));
    }

    let mut failures = Vec::new();
    for h in handles {
        match h.join().expect("thread panicked") {
            Ok(()) => {}
            Err(e) => failures.push(e.to_string()),
        }
    }
    assert!(
        failures.is_empty(),
        "every concurrent open must succeed; failures: {failures:?}"
    );
}

/// Re-opening the same db file twice in a row both succeed AND the unique index
/// still enforces run_id uniqueness after the second migration ran against the
/// already-migrated DB.
#[tokio::test]
async fn sequential_reopen_preserves_run_id_uniqueness() {
    let tmp = tempfile::NamedTempFile::new().expect("temp db file");
    let path = tmp.path().to_path_buf();

    let first = SqliteWorkflowStore::open(&path).expect("first open");
    drop(first);
    let store = SqliteWorkflowStore::open(&path).expect("second open");

    let a = store.create(inst("wf_a", "run-dup")).await;
    let b = store.create(inst("wf_b", "run-dup")).await;
    assert!(
        a.is_ok() ^ b.is_ok(),
        "the recreated unique index must still enforce run_id uniqueness; a={} b={}",
        a.is_ok(),
        b.is_ok()
    );
}

fn inst(id: &str, run_id: &str) -> praxec_core::model::WorkflowInstance {
    praxec_core::model::WorkflowInstance {
        id: id.to_string(),
        definition_id: "d".into(),
        definition_version: "1.0.0".into(),
        definition: serde_json::json!({}),
        state: "s".into(),
        version: 0,
        input: serde_json::json!({}),
        context: serde_json::json!({}),
        started_at: chrono::Utc::now(),
        run_env: praxec_core::RunEnv::new(
            praxec_core::RepoRoot::for_test(),
            Some(run_id.to_string()),
            None,
        ),
        cancelled_at: None,
        cancelled_reason: None,
        depth: 0,
        parent: None,
    }
}
