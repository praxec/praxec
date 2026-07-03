//! Phases 3–5 — auto-resume (FIFO), restart recovery, cross-workflow
//! serialization, lock surfacing, and audit for the global repo-lock wiring.

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use async_trait::async_trait;
use chrono::{DateTime, Utc};
use praxec_core::WorkflowRuntime;
use praxec_core::audit::{AuditSink, MemoryAuditSink};
use praxec_core::guards::DefaultGuardEvaluator;
use praxec_core::lock_scheduler::LockScheduler;
use praxec_core::model::{
    ExecuteRequest, ExecuteResult, Principal, StartWorkflow, SubmitTransition,
};
use praxec_core::ports::{Executor, ExecutorRegistry, WorkflowStore};
use praxec_core::repo_locks::{RepoLockSpace, RepoLocks};
use praxec_core::store::{ConfigDefinitionStore, InMemoryWorkflowStore};
use serde_json::{Value, json};
use std::path::PathBuf;

// ── harness ──────────────────────────────────────────────────────────────────

/// Executor recording the order of workflow ids that actually ran.
struct RecExec {
    order: Mutex<Vec<String>>,
    calls: AtomicUsize,
}
impl RecExec {
    fn new() -> Arc<Self> {
        Arc::new(Self {
            order: Mutex::new(Vec::new()),
            calls: AtomicUsize::new(0),
        })
    }
}
#[async_trait]
impl Executor for RecExec {
    async fn execute(
        &self,
        req: ExecuteRequest,
    ) -> Result<ExecuteResult, praxec_core::error::ExecutorError> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        self.order.lock().unwrap().push(req.workflow.id.clone());
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

fn config(files: &[&str]) -> Value {
    json!({
        "version": "1.0.0",
        "workflows": { "wf": {
            "initialState": "editing",
            "states": {
                "editing": { "transitions": { "edit": {
                    "target": "done", "actor": "agent",
                    "executor": { "kind": "agent", "owned_files": files }
                }}},
                "done": { "terminal": true }
            }
        }}
    })
}

#[allow(clippy::too_many_arguments)]
fn rt(
    cfg: Value,
    exec: Arc<dyn Executor>,
    locks: Arc<dyn RepoLocks>,
    sched: Arc<LockScheduler>,
    audit: Arc<MemoryAuditSink>,
    store: Arc<InMemoryWorkflowStore>,
) -> WorkflowRuntime {
    WorkflowRuntime::new(
        Arc::new(ConfigDefinitionStore::from_config(&cfg)),
        store as Arc<dyn WorkflowStore>,
        Arc::new(Reg(exec)),
        Arc::new(DefaultGuardEvaluator::new()),
        audit as Arc<dyn AuditSink>,
    )
    .with_repo_locks(locks)
    .with_lock_scheduler(sched)
}

async fn start(r: &WorkflowRuntime) -> (String, u64) {
    let s = r
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

async fn submit(r: &WorkflowRuntime, id: &str, ver: u64) -> Value {
    r.submit(SubmitTransition {
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

fn p(s: &str) -> PathBuf {
    PathBuf::from(s)
}
fn ttl() -> Duration {
    Duration::from_secs(300)
}

// ── Phase 3: auto-resume ─────────────────────────────────────────────────────

#[tokio::test]
async fn suspended_workflow_does_not_run_while_file_is_held() {
    let locks: Arc<dyn RepoLocks> = Arc::new(RepoLockSpace::new());
    locks.acquire(&[p("a.rs")], "other", ttl()).await.unwrap();
    let exec = RecExec::new();
    let r = rt(
        config(&["a.rs"]),
        exec.clone(),
        locks.clone(),
        Arc::new(LockScheduler::new()),
        Arc::new(MemoryAuditSink::new()),
        Arc::new(InMemoryWorkflowStore::new()),
    );
    let (id, ver) = start(&r).await;
    submit(&r, &id, ver).await;
    r.resume_ready_locks().await; // file still held → no resume
    assert_eq!(exec.calls.load(Ordering::SeqCst), 0);
}

#[tokio::test]
async fn released_file_auto_resumes_the_waiter() {
    let locks: Arc<dyn RepoLocks> = Arc::new(RepoLockSpace::new());
    locks.acquire(&[p("a.rs")], "other", ttl()).await.unwrap();
    let exec = RecExec::new();
    let r = rt(
        config(&["a.rs"]),
        exec.clone(),
        locks.clone(),
        Arc::new(LockScheduler::new()),
        Arc::new(MemoryAuditSink::new()),
        Arc::new(InMemoryWorkflowStore::new()),
    );
    let (id, ver) = start(&r).await;
    submit(&r, &id, ver).await; // suspends
    locks.release(&[p("a.rs")], "other").await;
    r.resume_ready_locks().await; // file free → re-drive
    assert_eq!(exec.calls.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn two_waiters_resume_in_fifo_order() {
    let locks: Arc<dyn RepoLocks> = Arc::new(RepoLockSpace::new());
    locks.acquire(&[p("a.rs")], "other", ttl()).await.unwrap();
    let exec = RecExec::new();
    let sched = Arc::new(LockScheduler::new());
    let r = rt(
        config(&["a.rs"]),
        exec.clone(),
        locks.clone(),
        sched,
        Arc::new(MemoryAuditSink::new()),
        Arc::new(InMemoryWorkflowStore::new()),
    );
    let (b1, v1) = start(&r).await;
    submit(&r, &b1, v1).await; // suspend B1 first
    let (b2, v2) = start(&r).await;
    submit(&r, &b2, v2).await; // suspend B2 second
    locks.release(&[p("a.rs")], "other").await;
    r.resume_ready_locks().await; // cascade: B1 then B2
    assert_eq!(*exec.order.lock().unwrap(), vec![b1, b2]);
}

#[tokio::test]
async fn waiter_needs_full_set_partial_free_stays_suspended() {
    let locks: Arc<dyn RepoLocks> = Arc::new(RepoLockSpace::new());
    locks
        .acquire(&[p("a.rs"), p("b.rs")], "other", ttl())
        .await
        .unwrap();
    let exec = RecExec::new();
    let r = rt(
        config(&["a.rs", "b.rs"]),
        exec.clone(),
        locks.clone(),
        Arc::new(LockScheduler::new()),
        Arc::new(MemoryAuditSink::new()),
        Arc::new(InMemoryWorkflowStore::new()),
    );
    let (id, ver) = start(&r).await;
    submit(&r, &id, ver).await;
    locks.release(&[p("a.rs")], "other").await; // only a freed; b still held
    r.resume_ready_locks().await;
    assert_eq!(exec.calls.load(Ordering::SeqCst), 0);
}

// ── Phase 4: restart recovery ────────────────────────────────────────────────

#[tokio::test]
async fn list_waiting_on_lock_returns_the_suspended_instance() {
    let locks: Arc<dyn RepoLocks> = Arc::new(RepoLockSpace::new());
    locks.acquire(&[p("a.rs")], "other", ttl()).await.unwrap();
    let store = Arc::new(InMemoryWorkflowStore::new());
    let r = rt(
        config(&["a.rs"]),
        RecExec::new(),
        locks,
        Arc::new(LockScheduler::new()),
        Arc::new(MemoryAuditSink::new()),
        store.clone(),
    );
    let (id, ver) = start(&r).await;
    submit(&r, &id, ver).await; // suspends, persisted
    assert_eq!(store.list_waiting_on_lock().await.unwrap().len(), 1);
}

#[tokio::test]
async fn workflow_suspended_before_restart_resumes_after_recovery() {
    // Pre-restart: suspend B against a held file, persisted in the store.
    let store = Arc::new(InMemoryWorkflowStore::new());
    let locks1: Arc<dyn RepoLocks> = Arc::new(RepoLockSpace::new());
    locks1.acquire(&[p("a.rs")], "other", ttl()).await.unwrap();
    let r1 = rt(
        config(&["a.rs"]),
        RecExec::new(),
        locks1,
        Arc::new(LockScheduler::new()),
        Arc::new(MemoryAuditSink::new()),
        store.clone(),
    );
    let (id, ver) = start(&r1).await;
    submit(&r1, &id, ver).await;

    // "Restart": fresh in-memory lock space (held locks gone), fresh scheduler;
    // the suspended workflow survives in the persisted store.
    let exec2 = RecExec::new();
    let r2 = rt(
        config(&["a.rs"]),
        exec2.clone(),
        Arc::new(RepoLockSpace::new()),
        Arc::new(LockScheduler::new()),
        Arc::new(MemoryAuditSink::new()),
        store.clone(),
    );
    r2.recover_suspended_locks().await; // re-register
    r2.resume_ready_locks().await; // files free post-restart → re-drive
    assert_eq!(exec2.calls.load(Ordering::SeqCst), 1);
}

// ── Phase 5: cross-workflow, surfacing, audit ────────────────────────────────

#[tokio::test]
async fn a_file_is_never_held_by_two_workflows_at_once() {
    let locks: Arc<dyn RepoLocks> = Arc::new(RepoLockSpace::new());
    locks.acquire(&[p("a.rs")], "wf:A", ttl()).await.unwrap(); // workflow A holds it
    let r = rt(
        config(&["a.rs"]),
        RecExec::new(),
        locks.clone(),
        Arc::new(LockScheduler::new()),
        Arc::new(MemoryAuditSink::new()),
        Arc::new(InMemoryWorkflowStore::new()),
    );
    let (id, ver) = start(&r).await;
    submit(&r, &id, ver).await; // B contends → suspends, does NOT also acquire
    let holders = locks
        .held()
        .await
        .into_iter()
        .filter(|h| h.file == p("a.rs"))
        .count();
    assert_eq!(holders, 1);
}

#[tokio::test]
async fn suspend_response_surfaces_the_held_lock() {
    let locks: Arc<dyn RepoLocks> = Arc::new(RepoLockSpace::new());
    locks.acquire(&[p("a.rs")], "other", ttl()).await.unwrap();
    let r = rt(
        config(&["a.rs"]),
        RecExec::new(),
        locks,
        Arc::new(LockScheduler::new()),
        Arc::new(MemoryAuditSink::new()),
        Arc::new(InMemoryWorkflowStore::new()),
    );
    let (id, ver) = start(&r).await;
    let resp = submit(&r, &id, ver).await;
    assert_eq!(resp["locks"][0]["file"], "a.rs");
}

#[tokio::test]
async fn granted_acquire_emits_lock_acquired_audit() {
    let audit = Arc::new(MemoryAuditSink::new());
    let r = rt(
        config(&["a.rs"]),
        RecExec::new(),
        Arc::new(RepoLockSpace::new()),
        Arc::new(LockScheduler::new()),
        audit.clone(),
        Arc::new(InMemoryWorkflowStore::new()),
    );
    let (id, ver) = start(&r).await;
    submit(&r, &id, ver).await;
    assert!(audit.event_types().contains(&"lock.acquired".to_string()));
}

#[tokio::test]
async fn release_emits_lock_released_audit() {
    let audit = Arc::new(MemoryAuditSink::new());
    let r = rt(
        config(&["a.rs"]),
        RecExec::new(),
        Arc::new(RepoLockSpace::new()),
        Arc::new(LockScheduler::new()),
        audit.clone(),
        Arc::new(InMemoryWorkflowStore::new()),
    );
    let (id, ver) = start(&r).await;
    submit(&r, &id, ver).await;
    assert!(audit.event_types().contains(&"lock.released".to_string()));
}

#[tokio::test]
async fn suspend_emits_lock_wait_audit() {
    let audit = Arc::new(MemoryAuditSink::new());
    let locks: Arc<dyn RepoLocks> = Arc::new(RepoLockSpace::new());
    locks.acquire(&[p("a.rs")], "other", ttl()).await.unwrap();
    let r = rt(
        config(&["a.rs"]),
        RecExec::new(),
        locks,
        Arc::new(LockScheduler::new()),
        audit.clone(),
        Arc::new(InMemoryWorkflowStore::new()),
    );
    let (id, ver) = start(&r).await;
    submit(&r, &id, ver).await;
    assert!(
        audit
            .event_types()
            .contains(&"lock.wait.suspended".to_string())
    );
}

#[tokio::test]
async fn resume_emits_lock_resumed_audit() {
    let audit = Arc::new(MemoryAuditSink::new());
    let locks: Arc<dyn RepoLocks> = Arc::new(RepoLockSpace::new());
    locks.acquire(&[p("a.rs")], "other", ttl()).await.unwrap();
    let r = rt(
        config(&["a.rs"]),
        RecExec::new(),
        locks.clone(),
        Arc::new(LockScheduler::new()),
        audit.clone(),
        Arc::new(InMemoryWorkflowStore::new()),
    );
    let (id, ver) = start(&r).await;
    submit(&r, &id, ver).await;
    locks.release(&[p("a.rs")], "other").await;
    r.resume_ready_locks().await;
    assert!(audit.event_types().contains(&"lock.resumed".to_string()));
}

#[tokio::test]
async fn dead_holder_ttl_reap_re_drives_the_waiter() {
    let t = Arc::new(Mutex::new(
        DateTime::<Utc>::from_timestamp(1_000_000, 0).unwrap(),
    ));
    let clock: praxec_core::repo_locks::Clock = {
        let t = t.clone();
        Arc::new(move || *t.lock().unwrap())
    };
    let locks: Arc<dyn RepoLocks> = Arc::new(RepoLockSpace::with_clock(clock));
    // A holder dies holding the file (short TTL, never releases).
    locks
        .acquire(&[p("a.rs")], "dead", Duration::from_secs(10))
        .await
        .unwrap();
    let exec = RecExec::new();
    let r = rt(
        config(&["a.rs"]),
        exec.clone(),
        locks.clone(),
        Arc::new(LockScheduler::new()),
        Arc::new(MemoryAuditSink::new()),
        Arc::new(InMemoryWorkflowStore::new()),
    );
    let (id, ver) = start(&r).await;
    submit(&r, &id, ver).await; // suspends: "dead" holds a.rs
    {
        // Single guard — `*t.lock() = *t.lock() + x` would self-deadlock the
        // non-reentrant std Mutex (RHS guard still held when LHS re-locks).
        let mut g = t.lock().unwrap();
        *g += chrono::Duration::seconds(20); // past the 10s TTL
    }
    r.resume_ready_locks().await; // held() reaps the dead lock → re-drive
    assert_eq!(exec.calls.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn recover_re_registers_suspended_into_the_scheduler() {
    let store = Arc::new(InMemoryWorkflowStore::new());
    let locks1: Arc<dyn RepoLocks> = Arc::new(RepoLockSpace::new());
    locks1.acquire(&[p("a.rs")], "other", ttl()).await.unwrap();
    let r1 = rt(
        config(&["a.rs"]),
        RecExec::new(),
        locks1,
        Arc::new(LockScheduler::new()),
        Arc::new(MemoryAuditSink::new()),
        store.clone(),
    );
    let (id, ver) = start(&r1).await;
    submit(&r1, &id, ver).await;

    // "Restart" with a fresh scheduler; recovery re-registers from the store.
    let sched2 = Arc::new(LockScheduler::new());
    let r2 = rt(
        config(&["a.rs"]),
        RecExec::new(),
        Arc::new(RepoLockSpace::new()),
        sched2.clone(),
        Arc::new(MemoryAuditSink::new()),
        store.clone(),
    );
    r2.recover_suspended_locks().await;
    assert_eq!(sched2.len().await, 1);
}
