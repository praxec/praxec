//! An **inactivity** (idle) timeout primitive for bounding external I/O whose
//! latency is dominated by progress we can observe, not by a fixed wall-clock
//! budget. A cold `npx -y …` MCP server can take tens of seconds to download —
//! it is *slow but alive* (bytes keep flowing on stdio). A hung server is
//! *silent*. A total timeout cannot tell them apart; an idle timeout can: it
//! fires only after `idle` elapses with **no activity**, and every observed
//! byte resets the clock.
//!
//! [`ActivityClock`] is the shared "last activity" signal — the transport bumps
//! it on every byte (in either direction). [`with_idle_timeout`] races a future
//! against that clock, returning [`IdleElapsed`] only once the connection has
//! been silent for the full window.

use std::future::Future;
use std::pin::Pin;
use std::sync::{Arc, Mutex};
use std::task::{Context, Poll};
use std::time::{Duration, Instant};

use tokio::io::{AsyncRead, ReadBuf};

/// A cheap, clonable "time of last activity" signal shared between the I/O that
/// produces activity (a transport reading child stdio) and the [`with_idle_timeout`]
/// guard watching it. Bumped via [`mark`](Self::mark) on every observed byte.
#[derive(Clone)]
pub struct ActivityClock {
    last: Arc<Mutex<Instant>>,
}

impl Default for ActivityClock {
    fn default() -> Self {
        Self::new()
    }
}

impl ActivityClock {
    pub fn new() -> Self {
        Self {
            last: Arc::new(Mutex::new(Instant::now())),
        }
    }

    /// Record activity now — resets the idle window.
    pub fn mark(&self) {
        *self.last.lock().expect("activity clock mutex poisoned") = Instant::now();
    }

    /// How long since the last [`mark`](Self::mark) (or construction).
    pub fn idle_for(&self) -> Duration {
        self.last
            .lock()
            .expect("activity clock mutex poisoned")
            .elapsed()
    }
}

/// The operation was idle (no activity) for the full `idle` window.
#[derive(Debug, Clone, Copy)]
pub struct IdleElapsed {
    pub idle: Duration,
}

impl std::fmt::Display for IdleElapsed {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "idle for {}ms with no activity", self.idle.as_millis())
    }
}

/// Run `fut`, but abort with [`IdleElapsed`] if `clock` records no activity for
/// `idle`. Activity (a [`mark`](ActivityClock::mark)) resets the window, so a
/// slow-but-progressing operation runs to completion while a silent one is cut
/// off shortly after `idle`.
pub async fn with_idle_timeout<F, T>(
    idle: Duration,
    clock: &ActivityClock,
    fut: F,
) -> Result<T, IdleElapsed>
where
    F: Future<Output = T>,
{
    tokio::pin!(fut);
    loop {
        // Recompute the remaining window from the *shared clock* each pass, so
        // any activity since the last check pushes the deadline out — that is
        // what makes this idle-based, not a fixed budget.
        let idle_for = clock.idle_for();
        if idle_for >= idle {
            return Err(IdleElapsed { idle });
        }
        let remaining = idle - idle_for;
        tokio::select! {
            out = &mut fut => return Ok(out),
            _ = tokio::time::sleep(remaining) => { /* re-check the clock */ }
        }
    }
}

/// An [`AsyncRead`] adapter that [`mark`](ActivityClock::mark)s a shared
/// [`ActivityClock`] whenever the underlying reader yields bytes. Wrapping the
/// child's stdout in this is how the transport's own reads become the activity
/// signal that keeps a live-but-slow connection from tripping the idle window.
pub struct ActivityTracked<R> {
    inner: R,
    clock: ActivityClock,
}

impl<R> ActivityTracked<R> {
    pub fn new(inner: R, clock: ActivityClock) -> Self {
        Self { inner, clock }
    }
}

impl<R: AsyncRead + Unpin> AsyncRead for ActivityTracked<R> {
    fn poll_read(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut ReadBuf<'_>,
    ) -> Poll<std::io::Result<()>> {
        let before = buf.filled().len();
        let polled = Pin::new(&mut self.inner).poll_read(cx, buf);
        if let Poll::Ready(Ok(())) = &polled {
            if buf.filled().len() > before {
                self.clock.mark();
            }
        }
        polled
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::io::AsyncReadExt;

    #[tokio::test]
    async fn idle_timeout_fires_when_there_is_no_activity() {
        let clock = ActivityClock::new();
        let start = Instant::now();
        let result = with_idle_timeout(Duration::from_millis(100), &clock, async {
            tokio::time::sleep(Duration::from_millis(800)).await;
            42
        })
        .await;
        assert!(
            result.is_err(),
            "a silent operation must hit the idle timeout"
        );
        assert!(
            start.elapsed() < Duration::from_millis(500),
            "must fire near the idle window, not wait out the whole operation"
        );
    }

    #[tokio::test]
    async fn an_operation_that_completes_before_the_window_returns_ok() {
        let clock = ActivityClock::new();
        let result = with_idle_timeout(Duration::from_millis(500), &clock, async {
            tokio::time::sleep(Duration::from_millis(20)).await;
            "done"
        })
        .await;
        assert_eq!(result.expect("completed within the window"), "done");
    }

    #[tokio::test]
    async fn activity_resets_the_window_so_a_slow_but_alive_operation_survives() {
        // The operation runs ~600ms — longer than the idle window — but marks
        // activity every 25ms. A *total* timeout would kill it at the 500ms
        // window; an *idle* timeout must let it finish because it never goes
        // silent. The 25ms tick / 500ms window ratio (20x) keeps the test
        // robust under heavy parallel-test load (a stalled tick has ~500ms of
        // headroom before the window would falsely fire) while staying
        // discriminating (the 500ms window is still < the ~600ms total, so a
        // total/non-resetting timeout would fire before the op completes).
        let clock = ActivityClock::new();
        let ticker = clock.clone();
        let op = async move {
            for _ in 0..24 {
                tokio::time::sleep(Duration::from_millis(25)).await;
                ticker.mark();
            }
            "alive"
        };
        let result = with_idle_timeout(Duration::from_millis(500), &clock, op).await;
        assert_eq!(
            result.expect("a steadily-active operation must not be cut off by the idle window"),
            "alive"
        );
    }

    #[tokio::test]
    async fn reading_bytes_through_the_tracker_marks_activity() {
        let clock = ActivityClock::new();
        tokio::time::sleep(Duration::from_millis(60)).await;
        assert!(
            clock.idle_for() >= Duration::from_millis(50),
            "clock should have aged"
        );

        let data = b"hello";
        let mut reader = ActivityTracked::new(&data[..], clock.clone());
        let mut buf = [0u8; 5];
        reader.read_exact(&mut buf).await.expect("read");

        assert_eq!(&buf, b"hello");
        assert!(
            clock.idle_for() < Duration::from_millis(30),
            "a successful read must reset the activity clock"
        );
    }
}
