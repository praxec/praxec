use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, RwLock};
use std::time::{Duration, Instant, SystemTime};

use async_trait::async_trait;
use serde_json::Value;

use crate::discovery::{DiscoveryIndex, DiscoveryItem, DiscoveryKind, SearchHit, SearchRequest};
use crate::ports::{DefinitionStore, Executor, ExecutorRegistry};

// ---------------------------------------------------------------------------
// P6b — lazy TTL-based config-staleness recheck.
//
// On WSL, filesystem watchers don't fire reliably, so `serve` cannot depend on
// fs-events to notice a post-startup config edit. Instead the serve path calls
// [`StalenessTracker::stale_check_due`] lazily at the top of request handling:
// within the TTL window it is a single mutex lock + Instant compare (cheap when
// called often); at most once per TTL it stats the tracked files and reports
// whether any mtime advanced past the value captured at last (re)load. A `true`
// answer triggers the SAME gated reload as SIGHUP / `praxec.command {reload}`
// — this module only makes the DECISION; it never reloads anything itself.
// ---------------------------------------------------------------------------

/// How long a staleness verdict is trusted before the tracked files are
/// stat'ed again. Requests inside the window skip the filesystem entirely.
// TODO: surface in config (e.g. `gateway.staleness_ttl_secs`).
pub const STALENESS_TTL: Duration = Duration::from_secs(10);

/// Pure TTL throttle: is a staleness probe due? `true` once `ttl` has elapsed
/// since `last_checked`. Time is injected so tests never sleep.
pub fn should_check(last_checked: Instant, now: Instant, ttl: Duration) -> bool {
    now.saturating_duration_since(last_checked) >= ttl
}

/// Pure staleness decision: did any tracked file's mtime advance past the
/// value captured at last (re)load? `now_mtimes` is injected so tests never
/// touch the filesystem. A file whose current mtime is unreadable (deleted
/// mid-edit, permission change) is treated as NOT stale — the safe option:
/// triggering a reload against a missing file would only fail the load, and
/// an editor's save-via-rename lands moments later with a fresh mtime that
/// IS observed on the next due probe.
pub fn is_stale(
    tracked: &[(PathBuf, SystemTime)],
    now_mtimes: impl Fn(&Path) -> Option<SystemTime>,
) -> bool {
    tracked
        .iter()
        .any(|(path, captured)| match now_mtimes(path) {
            Some(current) => current > *captured,
            None => false,
        })
}

/// The production `now_mtimes` source: a file's mtime, `None` if unreadable.
pub fn fs_mtime(path: &Path) -> Option<SystemTime> {
    std::fs::metadata(path).and_then(|m| m.modified()).ok()
}

/// Capture the current mtimes of `paths`. Files whose mtime cannot be read
/// are dropped from the tracked set (they'll be picked up by the recapture
/// after the next reload, if they exist by then).
fn capture_mtimes(paths: &[PathBuf]) -> Vec<(PathBuf, SystemTime)> {
    paths
        .iter()
        .filter_map(|p| fs_mtime(p).map(|t| (p.clone(), t)))
        .collect()
}

struct TrackerState {
    last_checked: Instant,
    tracked: Vec<(PathBuf, SystemTime)>,
}

/// Lazy, TTL-throttled config-staleness tracker (P6b). Composes the pure
/// [`should_check`] / [`is_stale`] decisions over real time + [`fs_mtime`].
///
/// The serve path holds one of these, calls [`Self::stale_check_due`] at the
/// top of every request, and — when it answers `true` — runs the gated reload
/// and then [`Self::recapture`]s (regardless of the reload outcome, so a
/// rejected edit is audited once, not retried every TTL until the file
/// changes again).
pub struct StalenessTracker {
    ttl: Duration,
    inner: Mutex<TrackerState>,
}

impl StalenessTracker {
    /// Track `paths`, capturing their current mtimes as the freshness
    /// baseline. The TTL clock starts now.
    pub fn new(paths: Vec<PathBuf>, ttl: Duration) -> Self {
        Self {
            ttl,
            inner: Mutex::new(TrackerState {
                last_checked: Instant::now(),
                tracked: capture_mtimes(&paths),
            }),
        }
    }

    /// TTL-throttled staleness probe. Within the TTL window this is a mutex
    /// lock + `Instant` compare (no filesystem access) and returns `false`.
    /// At most once per TTL it stats the tracked files and returns whether
    /// any mtime advanced past the captured baseline. The TTL clock resets
    /// on every probe that reaches the filesystem, stale or not.
    pub fn stale_check_due(&self) -> bool {
        let mut state = self
            .inner
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let now = Instant::now();
        if !should_check(state.last_checked, now, self.ttl) {
            return false;
        }
        state.last_checked = now;
        is_stale(&state.tracked, fs_mtime)
    }

    /// Re-baseline after a reload attempt: capture the current mtimes of
    /// `paths` (re-enumerated by the caller — the file set itself may have
    /// changed, e.g. an added `include:`) and reset the TTL clock.
    pub fn recapture(&self, paths: Vec<PathBuf>) {
        let mut state = self
            .inner
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        state.tracked = capture_mtimes(&paths);
        state.last_checked = Instant::now();
    }
}

pub struct SwappableDefinitionStore {
    inner: RwLock<Arc<dyn DefinitionStore>>,
}

impl SwappableDefinitionStore {
    pub fn new(initial: Arc<dyn DefinitionStore>) -> Self {
        Self {
            inner: RwLock::new(initial),
        }
    }

    pub fn swap(&self, new: Arc<dyn DefinitionStore>) {
        *self
            .inner
            .write()
            .expect("LOCK_POISONED: swappable definition store") = new;
    }
}

#[async_trait]
impl DefinitionStore for SwappableDefinitionStore {
    async fn load(&self, definition_id: &str) -> anyhow::Result<Value> {
        let store = self
            .inner
            .read()
            .expect("LOCK_POISONED: swappable definition store")
            .clone();
        store.load(definition_id).await
    }
}

pub struct SwappableExecutorRegistry {
    inner: RwLock<Arc<dyn ExecutorRegistry>>,
}

impl SwappableExecutorRegistry {
    pub fn new(initial: Arc<dyn ExecutorRegistry>) -> Self {
        Self {
            inner: RwLock::new(initial),
        }
    }

    pub fn swap(&self, new: Arc<dyn ExecutorRegistry>) {
        *self
            .inner
            .write()
            .expect("LOCK_POISONED: swappable executor registry") = new;
    }

    /// SPEC §33 D4 — snapshot the currently-held registry. The binary
    /// uses this to overlay the `kind: llm` executor onto the
    /// runtime-built base registry: capture the original, wrap it,
    /// swap the wrapper in. Holding the returned Arc separately means
    /// the overlay's delegation target is the original (not the
    /// swappable), so there's no get→swap→get cycle.
    pub fn current(&self) -> Arc<dyn ExecutorRegistry> {
        self.inner
            .read()
            .expect("LOCK_POISONED: swappable executor registry")
            .clone()
    }
}

impl ExecutorRegistry for SwappableExecutorRegistry {
    fn get(&self, kind: &str) -> Option<Arc<dyn Executor>> {
        let registry = self
            .inner
            .read()
            .expect("LOCK_POISONED: swappable executor registry")
            .clone();
        registry.get(kind)
    }
}

pub struct SwappableDiscoveryIndex {
    inner: RwLock<Arc<dyn DiscoveryIndex>>,
}

impl SwappableDiscoveryIndex {
    pub fn new(initial: Arc<dyn DiscoveryIndex>) -> Self {
        Self {
            inner: RwLock::new(initial),
        }
    }

    pub fn swap(&self, new: Arc<dyn DiscoveryIndex>) {
        *self
            .inner
            .write()
            .expect("LOCK_POISONED: swappable discovery index") = new;
    }
}

#[async_trait]
impl DiscoveryIndex for SwappableDiscoveryIndex {
    async fn search(&self, request: SearchRequest) -> anyhow::Result<Vec<SearchHit>> {
        let index = self
            .inner
            .read()
            .expect("LOCK_POISONED: swappable discovery index")
            .clone();
        index.search(request).await
    }

    async fn describe(&self, id: &str) -> anyhow::Result<Option<DiscoveryItem>> {
        let index = self
            .inner
            .read()
            .expect("LOCK_POISONED: swappable discovery index")
            .clone();
        index.describe(id).await
    }

    async fn list(&self, kind: Option<DiscoveryKind>) -> anyhow::Result<Vec<DiscoveryItem>> {
        let index = self
            .inner
            .read()
            .expect("LOCK_POISONED: swappable discovery index")
            .clone();
        index.list(kind).await
    }

    async fn home(&self) -> anyhow::Result<Value> {
        let index = self
            .inner
            .read()
            .expect("LOCK_POISONED: swappable discovery index")
            .clone();
        index.home().await
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::store::ConfigDefinitionStore;
    use serde_json::json;
    use std::collections::HashMap;

    #[tokio::test]
    async fn swap_definition_store() {
        let store_a = Arc::new(ConfigDefinitionStore::new(HashMap::from([(
            "wf_a".into(),
            json!({"initialState": "s", "states": {"s": {}}}),
        )])));
        let swappable = Arc::new(SwappableDefinitionStore::new(store_a));

        assert!(swappable.load("wf_a").await.is_ok());
        assert!(swappable.load("wf_b").await.is_err());

        let store_b = Arc::new(ConfigDefinitionStore::new(HashMap::from([(
            "wf_b".into(),
            json!({"initialState": "s", "states": {"s": {}}}),
        )])));
        swappable.swap(store_b);

        assert!(swappable.load("wf_a").await.is_err());
        assert!(swappable.load("wf_b").await.is_ok());
    }

    /// D3-T5 — a reload swaps the discovery index under live searches. Every
    /// concurrent reader must see EITHER the whole old index or the whole new
    /// one, never a half-swapped mix (which would read as "the workflow I just
    /// added is missing" / "the one I removed is still here"). This is why the
    /// reload path swaps a single `Arc<dyn DiscoveryIndex>` rather than mutating
    /// an index in place.
    #[tokio::test]
    async fn swap_discovery_index_is_atomic_under_concurrent_search() {
        use crate::discovery::{
            DiscoveryItem, DiscoveryKind, InMemoryDiscoveryIndex, SearchRequest,
        };

        fn index(ids: &[&str]) -> Arc<dyn DiscoveryIndex> {
            Arc::new(InMemoryDiscoveryIndex::new(
                ids.iter()
                    .map(|id| DiscoveryItem {
                        id: (*id).into(),
                        kind: DiscoveryKind::Workflow,
                        title: (*id).into(),
                        description: "flow".into(),
                        tags: vec![],
                        examples: vec![],
                        aliases: vec![],
                        text: String::new(),
                        links: vec![],
                        verb: None,
                        body: None,
                        source: None,
                    })
                    .collect(),
            ))
        }

        let swappable = Arc::new(SwappableDiscoveryIndex::new(index(&["old_a", "old_b"])));

        let searcher = {
            let swappable = swappable.clone();
            tokio::spawn(async move {
                for _ in 0..500 {
                    let hits = swappable
                        .search(SearchRequest {
                            query: "flow".into(),
                            kind: None,
                            limit: 10,
                        })
                        .await
                        .expect("search never fails mid-swap");
                    let ids: Vec<&str> = hits.iter().map(|h| h.item.id.as_str()).collect();
                    // Coherent generations only: never one old id beside one new.
                    assert!(
                        ids == ["old_a", "old_b"] || ids == ["new_a", "new_b", "new_c"],
                        "torn index observed: {ids:?}"
                    );
                    tokio::task::yield_now().await;
                }
            })
        };

        for _ in 0..50 {
            swappable.swap(index(&["new_a", "new_b", "new_c"]));
            tokio::task::yield_now().await;
            swappable.swap(index(&["old_a", "old_b"]));
            tokio::task::yield_now().await;
        }

        searcher.await.expect("no torn read");
    }

    // -- P6b staleness decisions (pure; time + mtimes injected) --------------

    #[test]
    fn should_check_is_false_within_ttl_and_true_after() {
        let ttl = Duration::from_secs(10);
        let t0 = Instant::now();
        assert!(!should_check(t0, t0, ttl));
        assert!(!should_check(t0, t0 + Duration::from_secs(9), ttl));
        assert!(should_check(t0, t0 + Duration::from_secs(10), ttl));
        assert!(should_check(t0, t0 + Duration::from_secs(11), ttl));
        // A clock that reads "before last_checked" must not underflow-panic.
        assert!(!should_check(t0 + Duration::from_secs(5), t0, ttl));
    }

    #[test]
    fn is_stale_true_when_a_tracked_mtime_advanced() {
        let base = SystemTime::UNIX_EPOCH + Duration::from_secs(1_000);
        let tracked = vec![
            (PathBuf::from("/cfg/praxec.yaml"), base),
            (PathBuf::from("/cfg/included.yaml"), base),
        ];
        // One file edited: its mtime advanced.
        assert!(is_stale(&tracked, |p| {
            if p == Path::new("/cfg/included.yaml") {
                Some(base + Duration::from_secs(5))
            } else {
                Some(base)
            }
        }));
    }

    #[test]
    fn is_stale_false_when_nothing_changed() {
        let base = SystemTime::UNIX_EPOCH + Duration::from_secs(1_000);
        let tracked = vec![(PathBuf::from("/cfg/praxec.yaml"), base)];
        assert!(!is_stale(&tracked, |_| Some(base)));
        // An mtime that went BACKWARD (restored backup) is not "newer".
        assert!(!is_stale(&tracked, |_| Some(base - Duration::from_secs(5))));
        // No tracked files at all: never stale.
        assert!(!is_stale(&[], |_| Some(base)));
    }

    #[test]
    fn is_stale_treats_missing_or_unreadable_file_as_not_stale() {
        let base = SystemTime::UNIX_EPOCH + Duration::from_secs(1_000);
        let tracked = vec![(PathBuf::from("/cfg/praxec.yaml"), base)];
        // Current mtime unreadable (file deleted mid-edit) — safe option:
        // not stale, no panic. The save-via-rename lands with a fresh mtime
        // that the next due probe observes.
        assert!(!is_stale(&tracked, |_| None));
    }

    #[test]
    fn tracker_detects_an_edit_and_recapture_rebaselines() {
        let dir = tempfile::tempdir().expect("tempdir");
        let cfg = dir.path().join("praxec.yaml");
        std::fs::write(&cfg, "a: 1\n").expect("write config");

        // TTL zero so every probe reaches the filesystem (no sleeping).
        let tracker = StalenessTracker::new(vec![cfg.clone()], Duration::ZERO);
        assert!(!tracker.stale_check_due(), "untouched file is not stale");

        // Simulate an operator edit by pushing the mtime firmly forward
        // (deterministic — no dependence on filesystem timestamp granularity).
        let future = SystemTime::now() + Duration::from_secs(30);
        let file = std::fs::File::options()
            .append(true)
            .open(&cfg)
            .expect("open config");
        file.set_times(std::fs::FileTimes::new().set_modified(future))
            .expect("set mtime");
        assert!(tracker.stale_check_due(), "advanced mtime is stale");

        // Recapture (what serve does after the gated reload) re-baselines:
        // the same mtime no longer reads as stale.
        tracker.recapture(vec![cfg.clone()]);
        assert!(!tracker.stale_check_due(), "recaptured baseline is fresh");
    }

    #[test]
    fn tracker_throttles_probes_within_the_ttl_window() {
        let dir = tempfile::tempdir().expect("tempdir");
        let cfg = dir.path().join("praxec.yaml");
        std::fs::write(&cfg, "a: 1\n").expect("write config");

        // Generous TTL: the constructor started the clock, so a probe fired
        // immediately afterward is inside the window and must not report
        // staleness even though the file HAS changed — at most one
        // filesystem-touching probe (and hence one reload) per TTL.
        let tracker = StalenessTracker::new(vec![cfg.clone()], Duration::from_secs(3600));
        let future = SystemTime::now() + Duration::from_secs(30);
        let file = std::fs::File::options()
            .append(true)
            .open(&cfg)
            .expect("open config");
        file.set_times(std::fs::FileTimes::new().set_modified(future))
            .expect("set mtime");
        assert!(!tracker.stale_check_due(), "within TTL: probe suppressed");
        assert!(!tracker.stale_check_due(), "still within TTL");
    }
}
