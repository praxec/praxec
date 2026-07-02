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

/// A workflow waiting for its file-set to free.
#[derive(Debug, Clone)]
pub struct Waiter {
    /// Insertion order — the FIFO key.
    pub seq: u64,
    pub workflow_id: String,
    pub transition: String,
    pub files: Vec<PathBuf>,
    pub principal: Principal,
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

    /// Enqueue a suspended workflow (FIFO by insertion).
    pub async fn enqueue(
        &self,
        workflow_id: String,
        transition: String,
        files: Vec<PathBuf>,
        principal: Principal,
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
        });
    }

    /// Remove and return the FIFO-first waiter whose **entire** file-set is
    /// currently free, or `None` if no waiter is fully unblocked. (The queue is
    /// insertion-ordered, so the first eligible entry is the earliest.)
    pub async fn take_ready(&self, locks: &dyn RepoLocks) -> Option<Waiter> {
        let held: HashSet<PathBuf> = locks.held().await.into_iter().map(|h| h.file).collect();
        let mut q = self.queue.lock().await;
        let pos = q
            .iter()
            .position(|w| w.files.iter().all(|f| !held.contains(f)))?;
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

    #[tokio::test]
    async fn taking_a_waiter_removes_it_from_the_queue() {
        let sch = LockScheduler::new();
        let locks = RepoLockSpace::new();
        enq(&sch, "B", &["a"]).await;
        sch.take_ready(&locks).await.unwrap();
        assert!(sch.take_ready(&locks).await.is_none());
    }
}
