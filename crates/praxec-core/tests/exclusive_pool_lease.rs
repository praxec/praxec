//! Run-scoped exclusive-pool leasing — the collision-prevention mechanism for
//! parallel browser E2E runs.
//!
//! A browser MCP server has ONE global `select_page` pointer per process, so two
//! runs sharing a connection corrupt each other. The pool gives each run its own
//! server process: a flow declares `exclusive_pools: [browser]` and leases one
//! member at the run boundary, bound to `$.run.leased.browser`. These tests pin
//! that (a) concurrent runs get DISTINCT members, (b) an exhausted pool fails
//! fast rather than sharing, and (c) a finished run's slot is freed for reuse.

use std::collections::BTreeMap;
use std::path::PathBuf;
use std::sync::Arc;

use praxec_core::WorkflowRuntime;
use praxec_core::audit::{AuditSink, MemoryAuditSink};
use praxec_core::guards::DefaultGuardEvaluator;
use praxec_core::model::{Principal, StartWorkflow};
use praxec_core::ports::WorkflowStore;
use praxec_core::repo_locks::{RepoLockSpace, RepoLocks};
use praxec_core::store::{ConfigDefinitionStore, InMemoryWorkflowStore};
use serde_json::{Value, json};

struct EmptyRegistry;
impl praxec_core::ports::ExecutorRegistry for EmptyRegistry {
    fn get(&self, _kind: &str) -> Option<Arc<dyn praxec_core::ports::Executor>> {
        None
    }
}

/// A one-shot flow that declares it needs the `browser` pool and reaches a
/// human gate (so it stays alive holding the lease until we cancel it).
fn browser_flow() -> Value {
    json!({
        "version": "1.0.0",
        "workflows": {
            "b": {
                "exclusive_pools": ["browser"],
                "initialState": "exploring",
                "states": {
                    "exploring": {
                        "transitions": {
                            "done": { "target": "done", "actor": "human", "executor": { "kind": "noop" } }
                        }
                    },
                    "done": { "terminal": true }
                }
            }
        }
    })
}

/// A flow that reaches terminal immediately in the start drive, so its lease is
/// released within `start()`.
fn browser_flow_autoterminal() -> Value {
    json!({
        "version": "1.0.0",
        "workflows": {
            "b": {
                "exclusive_pools": ["browser"],
                "initialState": "done",
                "states": { "done": { "terminal": true } }
            }
        }
    })
}

fn runtime_with_pool(
    config: Value,
    locks: Arc<dyn RepoLocks>,
    members: Vec<&str>,
) -> (WorkflowRuntime, Arc<InMemoryWorkflowStore>) {
    let store = Arc::new(InMemoryWorkflowStore::new());
    let mut pools: BTreeMap<String, Vec<PathBuf>> = BTreeMap::new();
    pools.insert(
        "browser".into(),
        members.into_iter().map(PathBuf::from).collect(),
    );
    let runtime = WorkflowRuntime::new(
        Arc::new(ConfigDefinitionStore::from_config(&config)),
        store.clone(),
        Arc::new(EmptyRegistry),
        Arc::new(DefaultGuardEvaluator::new()),
        Arc::new(MemoryAuditSink::new()) as Arc<dyn AuditSink>,
    )
    .with_writable_repo_roots(vec![praxec_core::RepoRoot::for_test()])
    .with_repo_locks(locks)
    .with_exclusive_pools(pools);
    (runtime, store)
}

async fn start(runtime: &WorkflowRuntime) -> anyhow::Result<Value> {
    runtime
        .start(StartWorkflow {
            definition_id: "b".into(),
            input: json!({}),
            principal: Principal::anonymous(),
            run_env: praxec_core::RunEnv::for_test(),
            depth: 0,
            parent: None,
        })
        .await
}

/// Two concurrent runs over a 2-member pool get DISTINCT leased members — the
/// core no-collision guarantee. Each run's `$.run.leased.browser` names a
/// different server process, so their `select_page` pointers never alias.
#[tokio::test]
async fn two_runs_lease_distinct_pool_members() {
    let locks: Arc<dyn RepoLocks> = Arc::new(RepoLockSpace::new());
    let (runtime, store) = runtime_with_pool(
        browser_flow(),
        locks,
        vec!["browser_chrome_1", "browser_chrome_2"],
    );

    let a = start(&runtime).await.unwrap()["workflow"]["id"]
        .as_str()
        .unwrap()
        .to_string();
    let b = start(&runtime).await.unwrap()["workflow"]["id"]
        .as_str()
        .unwrap()
        .to_string();

    let la = store.load(&a).await.unwrap().run_env.leased;
    let lb = store.load(&b).await.unwrap().run_env.leased;
    assert!(la.contains_key("browser") && lb.contains_key("browser"));
    assert_ne!(
        la.get("browser"),
        lb.get("browser"),
        "two runs must not share one browser server process"
    );
}

/// A third run over an exhausted 2-member pool fails FAST with POOL_EXHAUSTED —
/// never a silent share of a busy slot — and creates no instance.
#[tokio::test]
async fn an_exhausted_pool_fails_fast_and_creates_nothing() {
    let locks: Arc<dyn RepoLocks> = Arc::new(RepoLockSpace::new());
    let (runtime, store) = runtime_with_pool(
        browser_flow(),
        locks,
        vec!["browser_chrome_1", "browser_chrome_2"],
    );

    start(&runtime).await.unwrap();
    start(&runtime).await.unwrap();
    let err = start(&runtime)
        .await
        .expect_err("a third run must not get a browser from a 2-member pool");
    assert!(
        err.to_string().contains("POOL_EXHAUSTED"),
        "must be a typed capacity error, got: {err}"
    );
    // Exactly two instances exist — the failed start left nothing behind.
    assert_eq!(store.list_all().await.unwrap().len(), 2);
}

/// When a leased run finishes, its slot is released and a subsequent run may
/// take it — so a bounded pool serves an unbounded stream of runs over time.
#[tokio::test]
async fn a_finished_runs_slot_is_reusable() {
    let locks: Arc<dyn RepoLocks> = Arc::new(RepoLockSpace::new());
    // Single-member pool: the second run can only succeed if the first released.
    let (runtime, _store) =
        runtime_with_pool(browser_flow_autoterminal(), locks, vec!["browser_chrome_1"]);

    // First run auto-terminates inside start() → releases its lease.
    start(&runtime).await.unwrap();
    // Second run must therefore acquire the sole member, not exhaust.
    start(&runtime)
        .await
        .expect("the sole slot must be free again after the first run finished");
}

/// A flow that declares a pool with no configured members fails fast — an
/// authoring/config error surfaced at run start, not a silent unleased run.
#[tokio::test]
async fn declaring_an_unconfigured_pool_fails_fast() {
    let locks: Arc<dyn RepoLocks> = Arc::new(RepoLockSpace::new());
    let store = Arc::new(InMemoryWorkflowStore::new());
    // No pools configured at all.
    let runtime = WorkflowRuntime::new(
        Arc::new(ConfigDefinitionStore::from_config(&browser_flow())),
        store,
        Arc::new(EmptyRegistry),
        Arc::new(DefaultGuardEvaluator::new()),
        Arc::new(MemoryAuditSink::new()) as Arc<dyn AuditSink>,
    )
    .with_writable_repo_roots(vec![praxec_core::RepoRoot::for_test()])
    .with_repo_locks(locks);

    let err = start(&runtime)
        .await
        .expect_err("a declared-but-unconfigured pool must fail");
    assert!(err.to_string().contains("POOL_UNDECLARED"), "got: {err}");
}
