//! FIFO wait-queue for workflows suspended on repo file-locks.
//!
//! When the acquire-gate cannot take a transition's `owned_files`, the runtime
//! durably suspends the workflow and enqueues a [`Waiter`] here. On every lock
//! release, the runtime asks [`LockScheduler::take_ready`] for the FIFO-first
//! waiter whose *entire* file-set is now free and re-drives it. Event-driven —
//! no polling.

use std::collections::HashSet;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};

use tokio::sync::Mutex;

use crate::model::Principal;
use crate::repo_locks::RepoLocks;

/// What "ready" means for a waiter — the readiness predicate `take_ready`
/// applies to its `files`.
///
/// A typed enum rather than a bool so the match below is exhaustive: a third
/// acquisition shape cannot be added without the compiler naming every site
/// that must decide what readiness means for it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AcquireMode {
    /// The waiter needs its ENTIRE file-set free (a transition's `owned_files`).
    All,
    /// The waiter needs ANY ONE member free (a pool of interchangeable slots).
    Any,
}

/// A workflow waiting for its file-set to free.
#[derive(Debug, Clone)]
pub struct Waiter {
    /// Insertion order — the FIFO key.
    pub seq: u64,
    pub workflow_id: String,
    pub transition: String,
    pub files: Vec<PathBuf>,
    pub principal: Principal,
    /// How to decide this waiter is ready. Carried on the WAITER because the
    /// queue holds both shapes at once and the predicate differs per entry —
    /// applying all-of to a pool waiter starves it permanently.
    pub mode: AcquireMode,
}

/// FIFO queue of suspended workflows, ordered by enqueue time.
pub struct LockScheduler {
    queue: Mutex<Vec<Waiter>>,
    seq: AtomicU64,
}

impl LockScheduler {
    pub fn new() -> Self {
        Self {
            queue: Mutex::new(Vec::new()),
            seq: AtomicU64::new(0),
        }
    }

    /// Enqueue a suspended workflow waiting on its ENTIRE file-set (FIFO).
    pub async fn enqueue(
        &self,
        workflow_id: String,
        transition: String,
        files: Vec<PathBuf>,
        principal: Principal,
    ) {
        self.enqueue_with_mode(workflow_id, transition, files, principal, AcquireMode::All)
            .await
    }

    /// Enqueue a workflow waiting on ANY ONE member of a pool (FIFO).
    pub async fn enqueue_any(
        &self,
        workflow_id: String,
        transition: String,
        candidates: Vec<PathBuf>,
        principal: Principal,
    ) {
        self.enqueue_with_mode(
            workflow_id,
            transition,
            candidates,
            principal,
            AcquireMode::Any,
        )
        .await
    }

    async fn enqueue_with_mode(
        &self,
        workflow_id: String,
        transition: String,
        files: Vec<PathBuf>,
        principal: Principal,
        mode: AcquireMode,
    ) {
        let seq = self.seq.fetch_add(1, Ordering::SeqCst);
        let mut q = self.queue.lock().await;
        q.retain(|w| w.workflow_id != workflow_id); // one entry per workflow
        q.push(Waiter {
            seq,
            workflow_id,
            transition,
            files,
            principal,
            mode,
        });
    }

    /// Remove and return the FIFO-first waiter that is now ready, or `None`.
    ///
    /// Readiness is per-waiter (see [`AcquireMode`]): an `All` waiter needs its
    /// entire set free; an `Any` waiter needs one member free. Scanning past a
    /// blocked head to a ready follower is deliberate — FIFO orders *eligible*
    /// waiters, and letting a blocked file-set waiter pin the queue would starve
    /// every pool waiter behind it.
    pub async fn take_ready(&self, locks: &dyn RepoLocks) -> Option<Waiter> {
        let held: HashSet<PathBuf> = locks.held().await.into_iter().map(|h| h.file).collect();
        let mut q = self.queue.lock().await;
        let pos = q.iter().position(|w| match w.mode {
            AcquireMode::All => w.files.iter().all(|f| !held.contains(f)),
            // An empty candidate list is never ready: waking a waiter that can
            // hold nothing would send it straight back to a failed acquire.
            AcquireMode::Any => w.files.iter().any(|f| !held.contains(f)),
        })?;
        Some(q.remove(pos))
    }

    /// Number of queued waiters.
    pub async fn len(&self) -> usize {
        self.queue.lock().await.len()
    }

    /// True when no waiters are queued.
    pub async fn is_empty(&self) -> bool {
        self.queue.lock().await.is_empty()
    }
}

impl Default for LockScheduler {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::repo_locks::RepoLockSpace;
    use std::time::Duration;

    fn p(s: &str) -> PathBuf {
        PathBuf::from(s)
    }
    async fn enq(sch: &LockScheduler, id: &str, files: &[&str]) {
        sch.enqueue(
            id.into(),
            "edit".into(),
            files.iter().map(|f| p(f)).collect(),
            Principal::anonymous(),
        )
        .await;
    }

    #[tokio::test]
    async fn take_ready_returns_waiter_when_files_are_free() {
        let sch = LockScheduler::new();
        let locks = RepoLockSpace::new();
        enq(&sch, "B", &["a"]).await;
        assert_eq!(sch.take_ready(&locks).await.unwrap().workflow_id, "B");
    }

    #[tokio::test]
    async fn take_ready_is_none_when_a_file_is_held() {
        let sch = LockScheduler::new();
        let locks = RepoLockSpace::new();
        locks
            .acquire(&[p("a")], "other", Duration::from_secs(300))
            .await
            .unwrap();
        enq(&sch, "B", &["a"]).await;
        assert!(sch.take_ready(&locks).await.is_none());
    }

    #[tokio::test]
    async fn take_ready_is_fifo_among_eligible_waiters() {
        let sch = LockScheduler::new();
        let locks = RepoLockSpace::new();
        enq(&sch, "B1", &["a"]).await;
        enq(&sch, "B2", &["a"]).await;
        assert_eq!(sch.take_ready(&locks).await.unwrap().workflow_id, "B1");
    }

    #[tokio::test]
    async fn take_ready_requires_the_full_set_free() {
        let sch = LockScheduler::new();
        let locks = RepoLockSpace::new();
        locks
            .acquire(&[p("b")], "other", Duration::from_secs(300))
            .await
            .unwrap();
        enq(&sch, "B", &["a", "b"]).await;
        assert!(sch.take_ready(&locks).await.is_none());
    }

    // --- any-of waiters (pool acquisition) --------------------------------
    //
    // `take_ready`'s readiness predicate is ALL-of: a waiter is ready when its
    // entire file-set is free. That is correct for a file-set waiter and WRONG
    // for a pool waiter, which needs any ONE member. An any-of waiter enqueued
    // with the whole pool is never ready while any slot is busy — it starves.
    //
    // The bug is invisible at pool size 1, because all-of over a singleton IS
    // any-of. Every test would pass, and N>1 would starve the day it shipped.
    // Hence the mode rides on the Waiter.

    async fn enq_any(sch: &LockScheduler, id: &str, files: &[&str]) {
        sch.enqueue_any(
            id.into(),
            "explore".into(),
            files.iter().map(|f| p(f)).collect(),
            Principal::anonymous(),
        )
        .await;
    }

    #[tokio::test]
    async fn an_any_of_waiter_resumes_when_one_pool_slot_frees() {
        let sch = LockScheduler::new();
        let locks = RepoLockSpace::new();
        // slot-1 busy, slot-2 free: an any-of waiter IS ready.
        locks
            .acquire(&[p("slot-1")], "other", Duration::from_secs(300))
            .await
            .unwrap();
        enq_any(&sch, "B", &["slot-1", "slot-2"]).await;
        assert_eq!(
            sch.take_ready(&locks).await.map(|w| w.workflow_id),
            Some("B".to_string()),
            "one free slot is enough for a pool waiter"
        );
    }

    #[tokio::test]
    async fn an_any_of_waiter_stays_queued_when_every_slot_is_held() {
        let sch = LockScheduler::new();
        let locks = RepoLockSpace::new();
        for (f, h) in [("slot-1", "o1"), ("slot-2", "o2")] {
            locks
                .acquire(&[p(f)], h, Duration::from_secs(300))
                .await
                .unwrap();
        }
        enq_any(&sch, "B", &["slot-1", "slot-2"]).await;
        assert!(sch.take_ready(&locks).await.is_none());
    }

    #[tokio::test]
    async fn two_any_of_waiters_are_served_fifo_not_starved() {
        let sch = LockScheduler::new();
        let locks = RepoLockSpace::new();
        enq_any(&sch, "B1", &["slot-1", "slot-2"]).await;
        enq_any(&sch, "B2", &["slot-1", "slot-2"]).await;
        assert_eq!(sch.take_ready(&locks).await.unwrap().workflow_id, "B1");
        assert_eq!(sch.take_ready(&locks).await.unwrap().workflow_id, "B2");
    }

    #[tokio::test]
    async fn an_all_of_waiter_still_requires_its_full_set_alongside_any_of_waiters() {
        // Regression fence: adding the mode must not relax file-set waiters.
        let sch = LockScheduler::new();
        let locks = RepoLockSpace::new();
        locks
            .acquire(&[p("b")], "other", Duration::from_secs(300))
            .await
            .unwrap();
        enq(&sch, "ALLOF", &["a", "b"]).await;
        enq_any(&sch, "ANYOF", &["a", "b"]).await;
        // The all-of waiter is blocked; the any-of waiter behind it is ready.
        // FIFO must not let a blocked head starve a ready follower.
        assert_eq!(
            sch.take_ready(&locks).await.map(|w| w.workflow_id),
            Some("ANYOF".to_string())
        );
    }

    #[tokio::test]
    async fn taking_a_waiter_removes_it_from_the_queue() {
        let sch = LockScheduler::new();
        let locks = RepoLockSpace::new();
        enq(&sch, "B", &["a"]).await;
        sch.take_ready(&locks).await.unwrap();
        assert!(sch.take_ready(&locks).await.is_none());
    }
}
