//! Run identity is TOTAL via `run_ref` — the engine-minted internal identity of
//! the run TREE — WITHOUT polluting `run_id` (optional caller correlation).
//!
//! `RunEnv.run_id` is caller-supplied and optional (SPEC §20.2 correlation);
//! anything downstream needing a per-run identity for RESOURCE isolation (pool
//! leases) uses `run_ref` instead, which the engine stamps at the root boundary
//! and every sub-workflow inherits. This keeps the audit contract ("run_id null
//! when the caller supplies none") intact while giving the lease a total key.

use std::sync::Arc;

use praxec_core::WorkflowRuntime;
use praxec_core::audit::{AuditSink, MemoryAuditSink};
use praxec_core::guards::DefaultGuardEvaluator;
use praxec_core::model::{Principal, StartWorkflow};
use praxec_core::ports::WorkflowStore;
use praxec_core::store::{ConfigDefinitionStore, InMemoryWorkflowStore};
use serde_json::{Value, json};

struct EmptyRegistry;
impl praxec_core::ports::ExecutorRegistry for EmptyRegistry {
    fn get(&self, _kind: &str) -> Option<Arc<dyn praxec_core::ports::Executor>> {
        None
    }
}

fn one_state_config() -> Value {
    json!({
        "version": "1.0.0",
        "workflows": {
            "p": {
                "initialState": "a",
                "states": {
                    "a": {
                        "transitions": {
                            "go": { "target": "b", "actor": "human", "executor": { "kind": "noop" } }
                        }
                    },
                    "b": { "terminal": true }
                }
            }
        }
    })
}

/// Build a runtime, keeping the store so the persisted instance can be read.
fn runtime_with_store() -> (WorkflowRuntime, Arc<InMemoryWorkflowStore>) {
    let store = Arc::new(InMemoryWorkflowStore::new());
    let runtime = WorkflowRuntime::new(
        Arc::new(ConfigDefinitionStore::from_config(&one_state_config())),
        store.clone(),
        Arc::new(EmptyRegistry),
        Arc::new(DefaultGuardEvaluator::new()),
        Arc::new(MemoryAuditSink::new()) as Arc<dyn AuditSink>,
    )
    .with_writable_repo_roots(vec![praxec_core::RepoRoot::for_test()]);
    (runtime, store)
}

async fn start_with(runtime: &WorkflowRuntime, run_env: praxec_core::RunEnv) -> String {
    runtime
        .start(StartWorkflow {
            definition_id: "p".into(),
            input: json!({}),
            principal: Principal::anonymous(),
            run_env,
            depth: 0,
            parent: None,
        })
        .await
        .unwrap()["workflow"]["id"]
        .as_str()
        .unwrap()
        .to_string()
}

/// A run started without a caller-supplied `run_id` gets a total `run_ref` (the
/// internal identity the pool lease keys on) while `run_id` stays None — the
/// §20.2 audit contract is preserved, and per-run isolation still has an identity.
#[tokio::test]
async fn a_run_gets_a_run_ref_but_run_id_stays_caller_controlled() {
    let (runtime, store) = runtime_with_store();
    // RunEnv::for_test() carries run_id: None — the orchestrator's exact shape.
    let id = start_with(&runtime, praxec_core::RunEnv::for_test()).await;

    let saved = store.load(&id).await.unwrap();
    assert_eq!(
        saved.run_env.run_ref.as_deref(),
        Some(id.as_str()),
        "run_ref must be minted from the root instance id (total identity)"
    );
    assert!(
        saved.run_env.run_id.is_none(),
        "run_id is caller correlation and must stay None when unsupplied"
    );
}

/// A caller-supplied `run_id` is preserved verbatim — `run_ref` minting is
/// separate and must not touch the correlation id or its dedup semantics.
#[tokio::test]
async fn a_caller_supplied_run_id_is_preserved() {
    let (runtime, store) = runtime_with_store();
    let env = praxec_core::RunEnv::new(
        praxec_core::RepoRoot::for_test(),
        Some("caller-chosen-run".into()),
        None,
    );
    let id = start_with(&runtime, env).await;

    let saved = store.load(&id).await.unwrap();
    assert_eq!(
        saved.run_env.run_id.as_deref(),
        Some("caller-chosen-run"),
        "a supplied run_id is the caller's dedup key and must survive"
    );
    assert!(
        saved.run_env.run_ref.is_some(),
        "run_ref is minted regardless"
    );
}

/// Two runs get DISTINCT `run_ref`s. A shared constant would collapse per-run
/// resource isolation into one bucket.
#[tokio::test]
async fn two_runs_get_distinct_run_refs() {
    let (runtime, store) = runtime_with_store();
    let a = start_with(&runtime, praxec_core::RunEnv::for_test()).await;
    let b = start_with(&runtime, praxec_core::RunEnv::for_test()).await;

    let ra = store.load(&a).await.unwrap().run_env.run_ref;
    let rb = store.load(&b).await.unwrap().run_env.run_ref;
    assert!(ra.is_some() && rb.is_some());
    assert_ne!(
        ra, rb,
        "minted run refs must be per-run, not a shared constant"
    );
}
