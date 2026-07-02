//! Phase 2 — the repo-lock acquire-gate + durable suspend in `dispatch_once`.
//!
//! A file-owning transition acquires its `owned_files` before executing and
//! releases after; on contention it durably suspends (`waiting_on_lock` +
//! a persisted `_lock_wait` record) without running its executor.

use std::path::PathBuf;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use async_trait::async_trait;
use praxec_core::audit::{AuditSink, MemoryAuditSink};
use praxec_core::error::ExecutorError;
use praxec_core::guards::DefaultGuardEvaluator;
use praxec_core::model::{
    ExecuteRequest, ExecuteResult, Principal, StartWorkflow, SubmitTransition,
};
use praxec_core::ports::{Executor, ExecutorRegistry};
use praxec_core::repo_locks::{RepoLockSpace, RepoLocks};
use praxec_core::store::{ConfigDefinitionStore, InMemoryWorkflowStore};
use praxec_core::WorkflowRuntime;
use serde_json::{json, Value};

// ── harness ──────────────────────────────────────────────────────────────────

/// Executor that records the lock set held *while it runs* and its call count,
/// and can be told to fail.
struct Probe {
    locks: Arc<dyn RepoLocks>,
    held_during: Mutex<Vec<String>>,
    calls: AtomicUsize,
    fail: bool,
}

impl Probe {
    fn new(locks: Arc<dyn RepoLocks>, fail: bool) -> Arc<Self> {
        Arc::new(Self {
            locks,
            held_during: Mutex::new(Vec::new()),
            calls: AtomicUsize::new(0),
            fail,
        })
    }
}

#[async_trait]
impl Executor for Probe {
    async fn execute(&self, _req: ExecuteRequest) -> Result<ExecuteResult, ExecutorError> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        let mut held: Vec<String> = self
            .locks
            .held()
            .await
            .into_iter()
            .map(|h| h.file.to_string_lossy().into_owned())
            .collect();
        held.sort();
        *self.held_during.lock().unwrap() = held;
        if self.fail {
            return Err(ExecutorError::Permanent("boom".into()));
        }
        Ok(ExecuteResult {
            output: json!({}),
            evidence: vec![],
            child_workflow_id: None,
            next_transition: None,
            suspend: None,
            telemetry: None,
        })
    }
}

struct Reg(Arc<dyn Executor>);
impl ExecutorRegistry for Reg {
    fn get(&self, _kind: &str) -> Option<Arc<dyn Executor>> {
        Some(self.0.clone())
    }
}

/// A one-state workflow whose only transition's executor optionally declares
/// `owned_files`.
fn config(owned: Option<&[&str]>) -> Value {
    let mut exec = json!({ "kind": "agent" });
    if let Some(files) = owned {
        exec["owned_files"] = json!(files);
    }
    json!({
        "version": "1.0.0",
        "workflows": { "wf": {
            "initialState": "editing",
            "states": {
                "editing": { "transitions": { "edit": {
                    "target": "done", "actor": "agent", "executor": exec
                }}},
                "done": { "terminal": true }
            }
        }}
    })
}

fn build(cfg: Value, exec: Arc<dyn Executor>, locks: Arc<dyn RepoLocks>) -> WorkflowRuntime {
    WorkflowRuntime::new(
        Arc::new(ConfigDefinitionStore::from_config(&cfg)),
        Arc::new(InMemoryWorkflowStore::new()),
        Arc::new(Reg(exec)),
        Arc::new(DefaultGuardEvaluator::new()),
        Arc::new(MemoryAuditSink::new()) as Arc<dyn AuditSink>,
    )
    .with_repo_locks(locks)
}

async fn start(rt: &WorkflowRuntime) -> (String, u64) {
    let s = rt
        .start(StartWorkflow {
            definition_id: "wf".into(),
            input: json!({}),
            principal: Principal::anonymous(),
            trace_id: None,
            run_id: None,
            depth: 0,
            parent: None,
        })
        .await
        .unwrap();
    (
        s["workflow"]["id"].as_str().unwrap().to_string(),
        s["workflow"]["version"].as_u64().unwrap(),
    )
}

async fn submit(rt: &WorkflowRuntime, id: &str, ver: u64) -> Value {
    rt.submit(SubmitTransition {
        workflow_id: id.into(),
        expected_version: ver,
        transition: "edit".into(),
        arguments: json!({}),
        principal: Principal::anonymous(),
        summary: None,
        trace_id: None,
        run_id: None,
    })
    .await
    .unwrap()
}

fn locks() -> Arc<dyn RepoLocks> {
    Arc::new(RepoLockSpace::new())
}

// ── granted path ─────────────────────────────────────────────────────────────

#[tokio::test]
async fn file_owning_transition_acquires_before_executing() {
    let locks = locks();
    let probe = Probe::new(locks.clone(), false);
    let rt = build(config(Some(&["src/auth.rs"])), probe.clone(), locks);
    let (id, ver) = start(&rt).await;
    submit(&rt, &id, ver).await;
    assert_eq!(
        *probe.held_during.lock().unwrap(),
        vec!["src/auth.rs".to_string()]
    );
}

#[tokio::test]
async fn transition_with_no_owned_files_acquires_nothing() {
    let locks = locks();
    let probe = Probe::new(locks.clone(), false);
    let rt = build(config(None), probe.clone(), locks);
    let (id, ver) = start(&rt).await;
    submit(&rt, &id, ver).await;
    assert!(probe.held_during.lock().unwrap().is_empty());
}

#[tokio::test]
async fn successful_transition_releases_its_files_after() {
    let locks = locks();
    let probe = Probe::new(locks.clone(), false);
    let rt = build(config(Some(&["src/auth.rs"])), probe, locks.clone());
    let (id, ver) = start(&rt).await;
    submit(&rt, &id, ver).await;
    assert!(locks.held().await.is_empty());
}

#[tokio::test]
async fn failing_transition_still_releases_its_files() {
    let locks = locks();
    let probe = Probe::new(locks.clone(), true); // executor errors
    let rt = build(config(Some(&["src/auth.rs"])), probe, locks.clone());
    let (id, ver) = start(&rt).await;
    submit(&rt, &id, ver).await;
    assert!(locks.held().await.is_empty());
}

// ── contended path (durable suspend) ─────────────────────────────────────────

async fn build_contended() -> (WorkflowRuntime, Arc<Probe>, Arc<dyn RepoLocks>, String, u64) {
    let locks = locks();
    // Another agent already holds the file.
    locks
        .acquire(
            &[PathBuf::from("src/auth.rs")],
            "other-agent",
            Duration::from_secs(300),
        )
        .await
        .unwrap();
    let probe = Probe::new(locks.clone(), false);
    let rt = build(config(Some(&["src/auth.rs"])), probe.clone(), locks.clone());
    let (id, ver) = start(&rt).await;
    (rt, probe, locks, id, ver)
}

#[tokio::test]
async fn contended_transition_suspends_with_waiting_on_lock_status() {
    let (rt, _p, _l, id, ver) = build_contended().await;
    let resp = submit(&rt, &id, ver).await;
    assert_eq!(resp["result"]["status"], "waiting");
}

#[tokio::test]
async fn suspended_instance_records_contended_files_in_lock_wait() {
    let (rt, _p, _l, id, ver) = build_contended().await;
    let resp = submit(&rt, &id, ver).await;
    assert_eq!(resp["context"]["_lock_wait"]["files"][0], "src/auth.rs");
}

#[tokio::test]
async fn suspended_workflow_holds_no_locks_itself() {
    let (rt, _p, locks, id, ver) = build_contended().await;
    submit(&rt, &id, ver).await;
    let holders: Vec<String> = locks.held().await.into_iter().map(|h| h.holder).collect();
    assert!(!holders.iter().any(|h| h.starts_with("wf:")));
}

#[tokio::test]
async fn suspended_transition_does_not_run_its_executor() {
    let (rt, probe, _l, id, ver) = build_contended().await;
    submit(&rt, &id, ver).await;
    assert_eq!(probe.calls.load(Ordering::SeqCst), 0);
}

#[tokio::test]
async fn resubmitting_while_still_contended_stays_suspended() {
    let (rt, _p, _l, id, ver) = build_contended().await;
    let r1 = submit(&rt, &id, ver).await;
    let v1 = r1["workflow"]["version"].as_u64().unwrap();
    let r2 = submit(&rt, &id, v1).await;
    assert_eq!(r2["result"]["status"], "waiting");
}
