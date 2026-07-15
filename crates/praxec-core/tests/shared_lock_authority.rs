//! The runtime exposes its RepoLocks so overlays (untrusted-agent promotion)
//! coordinate on the SAME authority as transition owned_files locks.

use std::sync::Arc;

use praxec_core::WorkflowRuntime;
use praxec_core::audit::{AuditSink, MemoryAuditSink};
use praxec_core::guards::DefaultGuardEvaluator;
use praxec_core::ports::{Executor, ExecutorRegistry};
use praxec_core::repo_locks::{RepoLockSpace, RepoLocks};
use praxec_core::store::{ConfigDefinitionStore, InMemoryWorkflowStore};
use serde_json::json;

/// Empty executor registry — this test never executes a transition, it only
/// reads the shared lock authority back through the accessor.
struct EmptyReg;
impl ExecutorRegistry for EmptyReg {
    fn get(&self, _kind: &str) -> Option<Arc<dyn Executor>> {
        None
    }
}

/// Minimal one-state workflow config so `ConfigDefinitionStore::from_config`
/// has something to load.
fn minimal_config() -> serde_json::Value {
    json!({
        "version": "1.0.0",
        "workflows": { "wf": {
            "initialState": "s",
            "states": { "s": { "transitions": {} } }
        }}
    })
}

fn build_runtime_with_locks(locks: Arc<dyn RepoLocks>) -> WorkflowRuntime {
    let definitions = Arc::new(ConfigDefinitionStore::from_config(&minimal_config()));
    let store = Arc::new(InMemoryWorkflowStore::new());
    let executors = Arc::new(EmptyReg);
    let guards = Arc::new(DefaultGuardEvaluator::new());
    let audit = Arc::new(MemoryAuditSink::new());
    WorkflowRuntime::new(
        definitions,
        store,
        executors,
        guards,
        audit as Arc<dyn AuditSink>,
    )
    .with_writable_repo_roots(vec![praxec_core::RepoRoot::for_test()])
    .with_repo_locks(locks)
}

#[tokio::test]
async fn runtime_exposes_its_shared_repo_locks() {
    let locks: Arc<dyn RepoLocks> = Arc::new(RepoLockSpace::new());
    // Hold a path on the original handle.
    let files = vec![std::path::PathBuf::from("src/lib.rs")];
    locks
        .acquire(&files, "holder-a", std::time::Duration::from_secs(60))
        .await
        .expect("first acquire");

    // Build a runtime with these locks and read them back through the accessor.
    let runtime = build_runtime_with_locks(locks.clone());

    let shared = runtime.repo_locks().expect("runtime exposes repo_locks");
    let conflict = shared
        .acquire(&files, "holder-b", std::time::Duration::from_secs(60))
        .await;
    assert!(
        conflict.is_err(),
        "shared authority must see holder-a's lock"
    );
}
