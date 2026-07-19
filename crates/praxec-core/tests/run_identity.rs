//! Run identity is TOTAL: every started run has a `run_id`.
//!
//! `RunEnv.run_id` is caller-supplied and optional — the MCP handler passes it
//! through from an optional argument, and the headless orchestrator hardcodes
//! `None`. Anything downstream that needs a per-run identity (evidence paths,
//! per-run resource isolation) therefore had no identity to key on for a large
//! class of legitimate runs.
//!
//! The fix is to ELIMINATE the absent case rather than fail on it: mint the
//! instance id as the run id when the caller supplies none. A caller-supplied id
//! keeps its dedup semantics (the store enforces run_id uniqueness); absence
//! means "no dedup requested", not "no identity".

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

/// A run started without a caller-supplied `run_id` still has one afterwards.
/// This is what makes `$.run.run_id` a total scope rather than one that resolves
/// on the MCP path and vanishes under `praxec orchestrate`.
#[tokio::test]
async fn a_run_started_without_a_run_id_still_gets_one() {
    let (runtime, store) = runtime_with_store();
    // RunEnv::for_test() carries run_id: None — the orchestrator's exact shape.
    let id = start_with(&runtime, praxec_core::RunEnv::for_test()).await;

    let saved = store.load(&id).await.unwrap();
    assert_eq!(
        saved.run_env.run_id.as_deref(),
        Some(id.as_str()),
        "an absent run_id must be minted from the instance id, not left None"
    );
}

/// A caller-supplied `run_id` is preserved verbatim — minting must not clobber
/// it, or the store's run_id-uniqueness dedup silently stops working.
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
}

/// Two runs started with no supplied id get DISTINCT identities. If minting
/// produced a shared constant, per-run isolation keyed on it would silently
/// collapse into one shared bucket.
#[tokio::test]
async fn two_unidentified_runs_get_distinct_run_ids() {
    let (runtime, store) = runtime_with_store();
    let a = start_with(&runtime, praxec_core::RunEnv::for_test()).await;
    let b = start_with(&runtime, praxec_core::RunEnv::for_test()).await;

    let ra = store.load(&a).await.unwrap().run_env.run_id;
    let rb = store.load(&b).await.unwrap().run_env.run_id;
    assert!(ra.is_some() && rb.is_some());
    assert_ne!(
        ra, rb,
        "minted run ids must be per-run, not a shared constant"
    );
}
