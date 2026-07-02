//! Mockable filesystem abstraction used by [`FileAuditSink`].
//!
//! The trait surface is deliberately minimal — only the operations
//! `FileAuditSink` actually performs. Do not add methods for callers that
//! don't exist yet (YAGNI).
//!
//! [`FileAuditSink`]: crate::audit::FileAuditSink

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use async_trait::async_trait;

// ---------------------------------------------------------------------------
// Trait
// ---------------------------------------------------------------------------

/// Async filesystem operations used by [`FileAuditSink`].
///
/// The interface is intentionally narrow — only the operations the audit sink
/// needs are represented. Implementations must be `Send + Sync` so they can be
/// held behind an `Arc` and shared across async tasks.
#[async_trait]
pub trait Filesystem: Send + Sync {
    /// Create `path` (and all parents) as a directory. Succeeds silently when
    /// the directory already exists.
    async fn create_dir_all(&self, path: &Path) -> anyhow::Result<()>;

    /// Append `bytes` to the file at `path`, creating it if it does not exist.
    ///
    /// **Durability contract**: an `Ok` return means the bytes have been
    /// flushed AND synced to the storage device — the implementation MUST
    /// `fsync` (e.g. `sync_data`) before returning. Callers rely on this to
    /// avoid losing events on a crash, where a mere flush (OS page cache only)
    /// would not survive.
    async fn append(&self, path: &Path, bytes: &[u8]) -> anyhow::Result<()>;

    /// Return the paths of all immediate children of `path`.
    /// Returns an empty `Vec` when the directory does not exist.
    async fn read_dir(&self, path: &Path) -> anyhow::Result<Vec<PathBuf>>;

    /// Read and return the full contents of the file at `path`.
    async fn read_to_string(&self, path: &Path) -> anyhow::Result<String>;
}

// ---------------------------------------------------------------------------
// RealFilesystem — production impl over tokio::fs
// ---------------------------------------------------------------------------

/// Production [`Filesystem`] that delegates to `tokio::fs`.
///
/// The `append` operation opens the file with `create | append`, writes,
/// **flushes**, and **`sync_data`s** before returning `Ok`. This honors the
/// durability contract (`FileAuditSink::record` must not return `Ok` until the
/// bytes are durable on the storage device, not merely in the OS page cache).
pub struct RealFilesystem;

#[async_trait]
impl Filesystem for RealFilesystem {
    async fn create_dir_all(&self, path: &Path) -> anyhow::Result<()> {
        tokio::fs::create_dir_all(path).await?;
        Ok(())
    }

    async fn append(&self, path: &Path, bytes: &[u8]) -> anyhow::Result<()> {
        use tokio::io::AsyncWriteExt;
        let mut file = tokio::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(path)
            .await?;
        file.write_all(bytes).await?;
        // Flush pushes buffered bytes to the OS, but flush != durability:
        // on crash the OS page cache can still be lost. CMP-021 — the
        // durability contract promises bytes survive a crash, so we
        // `sync_data` to force them to the storage device before returning Ok.
        file.flush().await?;
        file.sync_data().await?;
        Ok(())
    }

    async fn read_dir(&self, path: &Path) -> anyhow::Result<Vec<PathBuf>> {
        let mut entries = match tokio::fs::read_dir(path).await {
            Ok(rd) => rd,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(vec![]),
            Err(e) => return Err(e.into()),
        };
        let mut paths = Vec::new();
        loop {
            match entries.next_entry().await {
                Ok(Some(entry)) => paths.push(entry.path()),
                Ok(None) => break,
                // Skip an unreadable entry — but log it, because dropping it
                // silently can make an audit-read listing incomplete without any
                // signal. (Fail-fast on detect: surface, don't swallow.)
                Err(e) => {
                    tracing::warn!(
                        error = %e,
                        "read_dir: skipping unreadable entry — listing may be incomplete"
                    );
                    continue;
                }
            }
        }
        Ok(paths)
    }

    async fn read_to_string(&self, path: &Path) -> anyhow::Result<String> {
        let s = tokio::fs::read_to_string(path).await?;
        Ok(s)
    }
}

// ---------------------------------------------------------------------------
// InMemoryFilesystem — deterministic test impl
// ---------------------------------------------------------------------------

/// In-memory [`Filesystem`] for unit tests.
///
/// Backed by an `Arc<Mutex<…>>` map of path → bytes so test instances are
/// cheaply clone-able and fully independent (no shared global state). Every
/// `record` call is synchronous under the lock — there is no real I/O.
///
/// # Inspection
///
/// Use the standard [`Filesystem`] methods (`read_to_string`, `read_dir`) to
/// inspect what was written, or call [`InMemoryFilesystem::files`] to get a
/// snapshot of all path → content pairs.
#[derive(Clone, Default)]
pub struct InMemoryFilesystem {
    inner: Arc<Mutex<InMemoryState>>,
}

#[derive(Default)]
struct InMemoryState {
    /// Files stored as raw bytes, keyed by the canonical path string.
    files: HashMap<String, Vec<u8>>,
    /// Set of known directories (path string). Populated by `create_dir_all`.
    dirs: std::collections::HashSet<String>,
}

impl InMemoryFilesystem {
    /// Create a new, empty in-memory filesystem.
    pub fn new() -> Self {
        Self::default()
    }

    /// Return a snapshot of all file paths and their UTF-8 contents.
    ///
    /// Files whose contents are not valid UTF-8 are silently omitted from the
    /// result. Entries are returned in **unspecified order** — callers that need
    /// a stable order must sort the returned `Vec` themselves. Intended for use
    /// in tests that want to assert on the written data without going through
    /// the async trait methods.
    pub fn files(&self) -> Vec<(PathBuf, String)> {
        let state = self
            .inner
            .lock()
            .expect("LOCK_POISONED: in-memory filesystem state");
        state
            .files
            .iter()
            .filter_map(|(k, v)| {
                String::from_utf8(v.clone())
                    .ok()
                    .map(|s| (PathBuf::from(k), s))
            })
            .collect()
    }
}

#[async_trait]
impl Filesystem for InMemoryFilesystem {
    async fn create_dir_all(&self, path: &Path) -> anyhow::Result<()> {
        let mut state = self
            .inner
            .lock()
            .expect("LOCK_POISONED: in-memory filesystem state");
        // Record the directory and all parents.
        let mut p = path.to_path_buf();
        loop {
            state.dirs.insert(p.to_string_lossy().into_owned());
            match p.parent() {
                Some(parent) if parent != p => p = parent.to_path_buf(),
                _ => break,
            }
        }
        Ok(())
    }

    async fn append(&self, path: &Path, bytes: &[u8]) -> anyhow::Result<()> {
        let mut state = self
            .inner
            .lock()
            .expect("LOCK_POISONED: in-memory filesystem state");
        // Mirror RealFilesystem: the parent directory must exist.
        if let Some(parent) = path.parent() {
            let parent_key = parent.to_string_lossy().into_owned();
            if !state.dirs.contains(&parent_key) {
                return Err(anyhow::anyhow!(
                    "parent directory does not exist: {}",
                    parent.display()
                ));
            }
        }
        let key = path.to_string_lossy().into_owned();
        state.files.entry(key).or_default().extend_from_slice(bytes);
        Ok(())
    }

    async fn read_dir(&self, path: &Path) -> anyhow::Result<Vec<PathBuf>> {
        let state = self
            .inner
            .lock()
            .expect("LOCK_POISONED: in-memory filesystem state");
        let prefix = path.to_string_lossy().into_owned();
        let paths: Vec<PathBuf> = state
            .files
            .keys()
            .filter_map(|k| {
                let p = Path::new(k);
                if p.parent().map(|par| par.to_string_lossy().into_owned()) == Some(prefix.clone())
                {
                    Some(PathBuf::from(k))
                } else {
                    None
                }
            })
            .collect();
        Ok(paths)
    }

    async fn read_to_string(&self, path: &Path) -> anyhow::Result<String> {
        let state = self
            .inner
            .lock()
            .expect("LOCK_POISONED: in-memory filesystem state");
        let key = path.to_string_lossy().into_owned();
        match state.files.get(&key) {
            Some(bytes) => {
                let s = String::from_utf8(bytes.clone())
                    .map_err(|e| anyhow::anyhow!("file is not valid UTF-8: {e}"))?;
                Ok(s)
            }
            None => Err(anyhow::anyhow!("file not found: {}", path.display())),
        }
    }
}
