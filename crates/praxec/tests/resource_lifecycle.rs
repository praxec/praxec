//! Resource-lifecycle harness foundation (docs/development/resource-leak-test-plan.md, Tier A).
//!
//! The leak classes that bite a process like praxec are *orphaned OS children*
//! and *detached tasks*, not C-style heap leaks. This module provides the two
//! reusable primitives the plan's process-reaping matrix is built on — a
//! `/proc`-based liveness check and a `CountedGuard` resource counter — and a
//! self-validating test that pins the **exact mechanism the sandbox fix relies
//! on**: a child spawned with `kill_on_drop(true)` is reaped when its `output()`
//! future is dropped on timeout (rather than detaching as an orphan).
//!
//! Linux-only (`/proc`); the CI target is Linux. The real-resource matrix
//! (MCP children, bwrap/OCI sandbox children across complete/cancel/timeout) is
//! tracked as follow-up in the leak plan and builds on these primitives.
#![cfg(target_os = "linux")]

use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

/// Is `pid` a live process? (`/proc/<pid>` exists while the process is alive;
/// note a not-yet-reaped zombie still has the entry — the `kill_on_drop` path we
/// test reaps fully, so the entry disappears.)
fn pid_alive(pid: u32) -> bool {
    std::path::Path::new(&format!("/proc/{pid}")).exists()
}

/// Wait up to ~2s for a pid to disappear (kill + reap is asynchronous).
async fn wait_gone(pid: u32) -> bool {
    for _ in 0..200 {
        if !pid_alive(pid) {
            return true;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    !pid_alive(pid)
}

/// A RAII resource counter (the plan's `CountedGuard`): `+1` on construction,
/// `-1` on drop. After a lifecycle the shared count must settle to its baseline —
/// the in-process analogue of the `/proc` check, for tasks/connections/handles.
struct CountedGuard {
    count: Arc<AtomicUsize>,
}
impl CountedGuard {
    fn new(count: Arc<AtomicUsize>) -> Self {
        count.fetch_add(1, Ordering::SeqCst);
        Self { count }
    }
}
impl Drop for CountedGuard {
    fn drop(&mut self) {
        self.count.fetch_sub(1, Ordering::SeqCst);
    }
}

#[tokio::test]
async fn a_kill_on_drop_child_is_reaped_when_its_future_is_dropped_on_timeout() {
    // This is the mechanism the sandbox `kill_on_drop(true)` fix depends on: a
    // long-running child whose `output()` future is dropped by a wall-clock
    // timeout must be terminated, not orphaned.
    let mut cmd = tokio::process::Command::new("sleep");
    cmd.arg("30");
    cmd.kill_on_drop(true);
    let mut child = cmd.spawn().expect("spawn sleep");
    let pid = child.id().expect("child pid");
    assert!(pid_alive(pid), "child should be alive right after spawn");

    // Drop the wait future via a timeout — exactly what the sandbox does.
    let waited = tokio::time::timeout(Duration::from_millis(50), child.wait()).await;
    assert!(
        waited.is_err(),
        "the child outlives the timeout (so the future is dropped)"
    );
    drop(child); // kill_on_drop fires here

    assert!(
        wait_gone(pid).await,
        "a kill_on_drop child must be reaped, not orphaned (pid {pid})"
    );
}

#[tokio::test]
async fn a_child_without_kill_on_drop_would_orphan_demonstrates_the_bug_shape() {
    // The negative control — *why* the fix matters. Without kill_on_drop, dropping
    // the handle detaches the child (it keeps running). We assert it is still
    // alive shortly after drop, then clean it up so the test leaks nothing.
    let mut cmd = tokio::process::Command::new("sleep");
    cmd.arg("30"); // no kill_on_drop
    let child = cmd.spawn().expect("spawn sleep");
    let pid = child.id().expect("child pid");
    drop(child); // detaches — the orphan bug shape

    tokio::time::sleep(Duration::from_millis(100)).await;
    let still_running = pid_alive(pid);
    // Clean up the deliberately-orphaned child regardless of the assertion.
    let _ = std::process::Command::new("kill")
        .arg("-9")
        .arg(pid.to_string())
        .output();
    assert!(
        still_running,
        "without kill_on_drop the child detaches (this is the leak the fix prevents)"
    );
}

#[test]
fn counted_guard_settles_to_baseline_after_scope() {
    let count = Arc::new(AtomicUsize::new(0));
    {
        let _a = CountedGuard::new(count.clone());
        let _b = CountedGuard::new(count.clone());
        assert_eq!(count.load(Ordering::SeqCst), 2, "two live resources");
    }
    assert_eq!(
        count.load(Ordering::SeqCst),
        0,
        "all resources released at scope end"
    );
}
