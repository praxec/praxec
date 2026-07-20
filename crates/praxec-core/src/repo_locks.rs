//! Global repository file-lock space — repo-wide write exclusion.
//!
//! Reuses the proven atomic-acquire + TTL-reap design from the CPM planner's
//! lock store (as in the `cpm-planner` crate) but **decoupled from plans**: one
//! global `file -> holder` table, so that no two agents — across any
//! workflows, sub-workflows, or missions — can hold a write lock on the same
//! file at once.
//!
//! Acquisition is **all-or-nothing**: a holder either locks its entire
//! file-set or nothing. That is what makes multi-file acquisition deadlock-
//! free (a waiter never holds a partial set while reaching for more).
//!
//! Contention is not an error here — it returns a [`LockConflict`] describing
//! what blocked, and the caller (the runtime) durably suspends and retries.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use chrono::{DateTime, Utc};
use tokio::sync::Mutex;

/// A currently-held file lock.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HeldLock {
    pub file: PathBuf,
    pub holder: String,
    pub expires_at: DateTime<Utc>,
}

/// Returned when an acquire cannot take its full set. Carries the files that
/// blocked it and who currently holds them. **Nothing was acquired.**
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LockConflict {
    pub conflicts: Vec<(PathBuf, String)>,
}

/// Global repository write-exclusion. Implementations MUST be atomic
/// all-or-nothing and MUST reap TTL-expired locks before granting.
#[async_trait]
pub trait RepoLocks: Send + Sync {
    /// Atomically lock all `files` for `holder`. On any conflict, acquire
    /// nothing and return the blocking files + their holders. Re-locking
    /// files the same `holder` already holds is idempotent (and refreshes TTL).
    async fn acquire(
        &self,
        files: &[PathBuf],
        holder: &str,
        ttl: Duration,
    ) -> Result<(), LockConflict>;

    /// Atomically lock **exactly one** free member of `candidates` for `holder`,
    /// returning which one. The pool analogue of [`acquire`](Self::acquire).
    ///
    /// `acquire` is all-or-nothing over an EXACT set — right for files, where a
    /// transition needs precisely the files it edits. A pool is the opposite
    /// shape: its members are interchangeable, the caller needs any one, and
    /// requesting the whole set would serialize the pool to a single slot.
    ///
    /// On exhaustion returns a [`LockConflict`] naming EVERY holder, so an
    /// operator sees what the pool is busy with rather than one arbitrary
    /// blocker. An empty `candidates` list is a config error and also fails —
    /// returning `Ok` there would report "no slot" as success and let the caller
    /// proceed holding nothing.
    ///
    /// Default impl is a linear first-fit over `acquire`, which inherits its
    /// atomicity: each probe either takes that single member or takes nothing.
    async fn acquire_any(
        &self,
        candidates: &[PathBuf],
        holder: &str,
        ttl: Duration,
    ) -> Result<PathBuf, LockConflict> {
        let mut conflicts: Vec<(PathBuf, String)> = Vec::new();
        for candidate in candidates {
            match self
                .acquire(std::slice::from_ref(candidate), holder, ttl)
                .await
            {
                Ok(()) => return Ok(candidate.clone()),
                Err(c) => conflicts.extend(c.conflicts),
            }
        }
        Err(LockConflict { conflicts })
    }

    /// Release `files` held by `holder`. Files held by a different holder are
    /// left untouched.
    async fn release(&self, files: &[PathBuf], holder: &str);

    /// Extend the TTL on `files` held by `holder` (a busy holder keeps its lock).
    async fn heartbeat(&self, files: &[PathBuf], holder: &str);

    /// Snapshot of every currently-held lock (for response surfacing).
    async fn held(&self) -> Vec<HeldLock>;
}

/// Injectable clock so tests can drive TTL deterministically.
pub type Clock = Arc<dyn Fn() -> DateTime<Utc> + Send + Sync>;

struct Entry {
    holder: String,
    expires_at: DateTime<Utc>,
    ttl: chrono::Duration,
}

/// In-process global lock table. `Arc`-share one instance across the runtime.
pub struct RepoLockSpace {
    files: Mutex<HashMap<PathBuf, Entry>>,
    clock: Clock,
}

impl RepoLockSpace {
    pub fn new() -> Self {
        Self {
            files: Mutex::new(HashMap::new()),
            clock: Arc::new(Utc::now),
        }
    }

    pub fn with_clock(clock: Clock) -> Self {
        Self {
            files: Mutex::new(HashMap::new()),
            clock,
        }
    }

    /// Drop every lock whose `expires_at` is strictly before `now`.
    fn reap(map: &mut HashMap<PathBuf, Entry>, now: DateTime<Utc>) {
        map.retain(|_, e| e.expires_at >= now);
    }
}

impl Default for RepoLockSpace {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait]
impl RepoLocks for RepoLockSpace {
    async fn acquire(
        &self,
        files: &[PathBuf],
        holder: &str,
        ttl: Duration,
    ) -> Result<(), LockConflict> {
        let now = (self.clock)();
        let mut map = self.files.lock().await;
        Self::reap(&mut map, now);

        let conflicts: Vec<(PathBuf, String)> = files
            .iter()
            .filter_map(|f| match map.get(f) {
                Some(e) if e.holder != holder => Some((f.clone(), e.holder.clone())),
                _ => None,
            })
            .collect();
        if !conflicts.is_empty() {
            return Err(LockConflict { conflicts });
        }

        let ttl_c =
            chrono::Duration::from_std(ttl).unwrap_or_else(|_| chrono::Duration::seconds(300));
        let expires_at = now + ttl_c;
        for f in files {
            map.insert(
                f.clone(),
                Entry {
                    holder: holder.to_string(),
                    expires_at,
                    ttl: ttl_c,
                },
            );
        }
        Ok(())
    }

    async fn release(&self, files: &[PathBuf], holder: &str) {
        let mut map = self.files.lock().await;
        for f in files {
            if matches!(map.get(f), Some(e) if e.holder == holder) {
                map.remove(f);
            }
        }
    }

    async fn heartbeat(&self, files: &[PathBuf], holder: &str) {
        let now = (self.clock)();
        let mut map = self.files.lock().await;
        // Reap first: an already-expired lock is logically released. A late
        // heartbeat must NOT resurrect it (that risks a double-held file when a
        // waiter has since acquired). After reaping, an expired entry is gone
        // and the heartbeat is a no-op — the holder must re-acquire.
        Self::reap(&mut map, now);
        for f in files {
            if let Some(e) = map.get_mut(f) {
                if e.holder == holder {
                    e.expires_at = now + e.ttl;
                }
            }
        }
    }

    async fn held(&self) -> Vec<HeldLock> {
        let now = (self.clock)();
        let mut map = self.files.lock().await;
        // Reap on read so a dead holder's expired lock never blocks a waiter
        // (the scheduler decides readiness from `held()`).
        Self::reap(&mut map, now);
        map.iter()
            .map(|(file, e)| HeldLock {
                file: file.clone(),
                holder: e.holder.clone(),
                expires_at: e.expires_at,
            })
            .collect()
    }
}

/// The file-set a transition's executor declares it will write. A single
/// executor contributes its own `owned_files`; a `kind: parallel` executor
/// contributes the **union** of its branches' `owned_files`. Empty when none
/// are declared. Stable order, de-duplicated. This is what the runtime
/// acquire-gate locks before executing a file-owning transition.
pub fn owned_files_in(executor_config: &serde_json::Value) -> Vec<PathBuf> {
    use serde_json::Value;
    fn files_of(v: &Value) -> Vec<PathBuf> {
        v.get("owned_files")
            .and_then(Value::as_array)
            .map(|a| {
                a.iter()
                    .filter_map(Value::as_str)
                    .map(PathBuf::from)
                    .collect()
            })
            .unwrap_or_default()
    }
    let mut out: Vec<PathBuf> = files_of(executor_config);
    if let Some(branches) = executor_config.get("branches").and_then(Value::as_array) {
        for b in branches {
            out.extend(files_of(b));
        }
    }
    let mut seen = std::collections::HashSet::new();
    out.retain(|p| seen.insert(p.clone()));
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn p(s: &str) -> PathBuf {
        PathBuf::from(s)
    }
    fn ttl() -> Duration {
        Duration::from_secs(300)
    }
    async fn held_files(space: &RepoLockSpace) -> Vec<PathBuf> {
        let mut v: Vec<PathBuf> = space.held().await.into_iter().map(|h| h.file).collect();
        v.sort();
        v
    }
    fn clock_from(t: &Arc<std::sync::Mutex<DateTime<Utc>>>) -> Clock {
        let t = t.clone();
        Arc::new(move || *t.lock().unwrap())
    }
    fn base() -> Arc<std::sync::Mutex<DateTime<Utc>>> {
        Arc::new(std::sync::Mutex::new(
            DateTime::<Utc>::from_timestamp(1_000_000, 0).unwrap(),
        ))
    }
    fn advance(t: &Arc<std::sync::Mutex<DateTime<Utc>>>, secs: i64) {
        let mut g = t.lock().unwrap();
        *g += chrono::Duration::seconds(secs);
    }

    #[tokio::test]
    async fn acquire_free_files_succeeds() {
        let space = RepoLockSpace::new();
        assert!(space.acquire(&[p("a")], "h1", ttl()).await.is_ok());
    }

    // --- acquire_any: pool acquisition ------------------------------------
    //
    // `acquire` is all-or-nothing over an EXACT set, which is right for files
    // (a transition needs precisely the files it edits). A pool of
    // interchangeable slots is the opposite shape: the caller needs ANY ONE
    // free member, and asking for the whole pool would serialize it to nothing.

    #[tokio::test]
    async fn acquire_any_takes_one_free_slot_from_the_pool() {
        let space = RepoLockSpace::new();
        let got = space
            .acquire_any(&[p("slot-1"), p("slot-2")], "h1", ttl())
            .await
            .expect("a free pool must grant one slot");
        assert!(got == p("slot-1") || got == p("slot-2"));
        // EXACTLY one slot is taken — not the whole pool.
        assert_eq!(held_files(&space).await.len(), 1);
    }

    #[tokio::test]
    async fn acquire_any_skips_a_held_slot_and_takes_the_free_one() {
        let space = RepoLockSpace::new();
        space.acquire(&[p("slot-1")], "other", ttl()).await.unwrap();
        let got = space
            .acquire_any(&[p("slot-1"), p("slot-2")], "h1", ttl())
            .await
            .expect("one slot is free");
        assert_eq!(got, p("slot-2"));
    }

    #[tokio::test]
    async fn acquire_any_fails_when_every_slot_is_held() {
        let space = RepoLockSpace::new();
        space.acquire(&[p("slot-1")], "o1", ttl()).await.unwrap();
        space.acquire(&[p("slot-2")], "o2", ttl()).await.unwrap();
        let err = space
            .acquire_any(&[p("slot-1"), p("slot-2")], "h1", ttl())
            .await
            .expect_err("an exhausted pool must not grant");
        // The conflict names EVERY holder, so the operator can see what the
        // pool is busy with rather than one arbitrary blocker.
        assert_eq!(err.conflicts.len(), 2);
    }

    #[tokio::test]
    async fn acquire_any_on_an_empty_candidate_list_fails_rather_than_granting() {
        // An empty pool is an authoring/config error. Returning Ok here would
        // hand back "no slot" as success and let the caller proceed unleased.
        let space = RepoLockSpace::new();
        assert!(space.acquire_any(&[], "h1", ttl()).await.is_err());
    }

    #[tokio::test]
    async fn held_lists_the_acquired_file_and_holder() {
        let space = RepoLockSpace::new();
        space.acquire(&[p("a")], "h1", ttl()).await.unwrap();
        let held: Vec<(PathBuf, String)> = space
            .held()
            .await
            .into_iter()
            .map(|h| (h.file, h.holder))
            .collect();
        assert_eq!(held, vec![(p("a"), "h1".to_string())]);
    }

    #[tokio::test]
    async fn acquire_is_atomic_all_or_nothing() {
        let space = RepoLockSpace::new();
        space.acquire(&[p("b")], "h1", ttl()).await.unwrap();
        let _ = space.acquire(&[p("a"), p("b")], "h2", ttl()).await; // conflicts on b
        assert!(!held_files(&space).await.contains(&p("a"))); // a was NOT locked
    }

    #[tokio::test]
    async fn conflict_reports_the_blocking_file_and_holder() {
        let space = RepoLockSpace::new();
        space.acquire(&[p("b")], "h1", ttl()).await.unwrap();
        let err = space
            .acquire(&[p("a"), p("b")], "h2", ttl())
            .await
            .unwrap_err();
        assert_eq!(err.conflicts, vec![(p("b"), "h1".to_string())]);
    }

    #[tokio::test]
    async fn release_frees_the_files() {
        let space = RepoLockSpace::new();
        space.acquire(&[p("a")], "h1", ttl()).await.unwrap();
        space.release(&[p("a")], "h1").await;
        assert!(space.held().await.is_empty());
    }

    #[tokio::test]
    async fn release_by_non_holder_leaves_the_lock_intact() {
        let space = RepoLockSpace::new();
        space.acquire(&[p("a")], "h1", ttl()).await.unwrap();
        space.release(&[p("a")], "h2").await; // wrong holder
        assert!(held_files(&space).await.contains(&p("a")));
    }

    #[tokio::test]
    async fn holder_of_ab_blocks_acquirer_of_bc() {
        let space = RepoLockSpace::new();
        space.acquire(&[p("a"), p("b")], "h1", ttl()).await.unwrap();
        assert!(space.acquire(&[p("b"), p("c")], "h2", ttl()).await.is_err());
    }

    #[tokio::test]
    async fn same_holder_reacquiring_its_own_files_is_idempotent() {
        let space = RepoLockSpace::new();
        space.acquire(&[p("a")], "h1", ttl()).await.unwrap();
        assert!(space.acquire(&[p("a")], "h1", ttl()).await.is_ok());
    }

    #[tokio::test]
    async fn lock_past_ttl_is_reaped_on_next_acquire() {
        let t = base();
        let space = RepoLockSpace::with_clock(clock_from(&t));
        space
            .acquire(&[p("a")], "h1", Duration::from_secs(10))
            .await
            .unwrap();
        advance(&t, 20); // past the 10s TTL
        assert!(
            space
                .acquire(&[p("a")], "h2", Duration::from_secs(10))
                .await
                .is_ok()
        );
    }

    #[tokio::test]
    async fn heartbeat_extends_ttl_so_a_busy_holder_keeps_its_lock() {
        let t = base();
        let space = RepoLockSpace::with_clock(clock_from(&t));
        space
            .acquire(&[p("a")], "h1", Duration::from_secs(10))
            .await
            .unwrap();
        advance(&t, 8);
        space.heartbeat(&[p("a")], "h1").await; // expires now extends to t+18
        advance(&t, 4); // t=12: past original t10, but within the heartbeat-extended t18
        assert!(
            space
                .acquire(&[p("a")], "h2", Duration::from_secs(10))
                .await
                .is_err()
        );
    }

    #[tokio::test]
    async fn heartbeat_does_not_resurrect_an_already_expired_lock() {
        // H3 regression: a late heartbeat after the TTL lapsed must not bring a
        // logically-released lock back to life. Reap-before-extend makes it a
        // no-op; the holder must re-acquire.
        let t = base();
        let space = RepoLockSpace::with_clock(clock_from(&t));
        space
            .acquire(&[p("a")], "h1", Duration::from_secs(10))
            .await
            .unwrap();
        advance(&t, 20); // past the 10s TTL — lock is expired/released
        space.heartbeat(&[p("a")], "h1").await; // late heartbeat
        assert!(
            held_files(&space).await.is_empty(),
            "expired lock must stay reaped — heartbeat must not resurrect it"
        );
    }

    #[tokio::test]
    async fn reaping_frees_only_the_expired_lock_not_others() {
        let t = base();
        let space = RepoLockSpace::with_clock(clock_from(&t));
        space
            .acquire(&[p("a")], "h1", Duration::from_secs(10))
            .await
            .unwrap();
        space
            .acquire(&[p("b")], "h2", Duration::from_secs(100))
            .await
            .unwrap();
        advance(&t, 20); // a expired; b still valid
        assert_eq!(held_files(&space).await, vec![p("b")]);
    }

    // ── owned_files_in extraction ──────────────────────────────────────────

    #[test]
    fn owned_files_single_executor() {
        let cfg = serde_json::json!({ "kind": "agent", "owned_files": ["a", "b"] });
        assert_eq!(owned_files_in(&cfg), vec![p("a"), p("b")]);
    }

    #[test]
    fn owned_files_none_when_undeclared() {
        let cfg = serde_json::json!({ "kind": "noop" });
        assert!(owned_files_in(&cfg).is_empty());
    }

    #[test]
    fn owned_files_parallel_is_union_of_branches() {
        let cfg = serde_json::json!({
            "kind": "parallel",
            "branches": [
                { "kind": "agent", "owned_files": ["a"] },
                { "kind": "agent", "owned_files": ["b"] },
            ]
        });
        assert_eq!(owned_files_in(&cfg), vec![p("a"), p("b")]);
    }

    #[test]
    fn owned_files_dedups_overlapping_branches() {
        let cfg = serde_json::json!({
            "kind": "parallel",
            "branches": [
                { "owned_files": ["a"] },
                { "owned_files": ["a"] },
            ]
        });
        assert_eq!(owned_files_in(&cfg), vec![p("a")]);
    }
}
