//! Run-ambient execution environment (SPEC — v0.0.21).
//!
//! A run always operates on exactly one repository, so `repo_root` is *ambient*
//! to the whole run: established once at the boundary and threaded structurally
//! through every sub-workflow spawn (exactly like
//! [`crate::model::WorkflowInstance::depth`]), never hand-wired through
//! `use.inputs` bindings. Hand-wiring was the defect — a `kind: workflow` leaf
//! that forgot to thread `repo_path` handed the coding agent an empty
//! filesystem root, which it silently burned its whole step budget against.
//! This is the Reader-monad / ambient-context pattern realized as a value on
//! the request carriers.
//!
//! [`RunEnv`] also carries the run correlation identity (`run_id` / `trace_id`).
//! They live here — not as standalone `WorkflowInstance` fields — so they
//! survive a sub-workflow spawn; the former standalone fields reset to `None`
//! at each spawn boundary, breaking correlation across the run tree.

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use thiserror::Error;

/// A validated repository root: an **absolute, canonical, existing directory**.
///
/// Illegal states are unrepresentable — the only filesystem-touching
/// constructor ([`RepoRoot::new`]) canonicalizes and asserts
/// `is_absolute() && is_dir()`, so a `RepoRoot` value can never hold an empty,
/// relative, or missing path. This is the poka-yoke that collapses the whole
/// "coding agent got an empty filesystem root" defect class into a type
/// invariant.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(try_from = "String", into = "String")]
pub struct RepoRoot(PathBuf);

/// Why a candidate path is not a valid [`RepoRoot`].
#[derive(Debug, Error)]
pub enum RepoRootError {
    #[error("repo_root is not absolute: {0}")]
    NotAbsolute(String),
    #[error("repo_root is not a directory: {0}")]
    NotADir(String),
    #[error("repo_root canonicalize failed for {path}: {source}")]
    Canonicalize {
        path: String,
        #[source]
        source: std::io::Error,
    },
}

impl RepoRoot {
    /// The boundary constructor: canonicalize `p`, then assert it is an
    /// absolute existing directory. This is the ONLY path that touches the
    /// filesystem — it runs once, where the run's root is established from a
    /// config-declared writable repo.
    pub fn new(p: impl AsRef<Path>) -> Result<Self, RepoRootError> {
        let raw = p.as_ref();
        let canonical =
            std::fs::canonicalize(raw).map_err(|source| RepoRootError::Canonicalize {
                path: raw.display().to_string(),
                source,
            })?;
        if !canonical.is_absolute() {
            return Err(RepoRootError::NotAbsolute(canonical.display().to_string()));
        }
        if !canonical.is_dir() {
            return Err(RepoRootError::NotADir(canonical.display().to_string()));
        }
        Ok(Self(canonical))
    }

    /// The reload constructor: a value round-tripping from a store was already
    /// filesystem-validated by [`RepoRoot::new`] at creation, so re-check only
    /// the cheap structural invariant (`is_absolute`) — do NOT re-`stat`, so a
    /// temporarily-unmounted repo does not make in-flight instances unloadable.
    pub fn from_persisted(s: impl Into<String>) -> Result<Self, RepoRootError> {
        let s = s.into();
        let p = PathBuf::from(&s);
        if !p.is_absolute() {
            return Err(RepoRootError::NotAbsolute(s));
        }
        Ok(Self(p))
    }

    /// The canonical absolute directory.
    pub fn as_path(&self) -> &Path {
        &self.0
    }

    /// The canonical root as a string — what `$.run.repo_root` renders to and
    /// what a `file:<root>` connection is built from.
    pub fn as_str(&self) -> &str {
        // A `RepoRoot` only ever holds a canonical path; on the platforms
        // praxec targets that is valid UTF-8. Fall back lossily rather than
        // panic if that ever fails.
        self.0.to_str().unwrap_or_default()
    }

    /// Test-only constructor: a valid root at the OS temp dir. Deliberately
    /// **always compiled** (not `#[cfg(test)]`) because a `#[cfg(test)]` method
    /// is invisible to *other* crates' test targets, and those build
    /// `WorkflowInstance` / `StartWorkflow` literals that now require a
    /// `RunEnv`. Never call from a production path — the boundary uses
    /// [`RepoRoot::new`].
    #[doc(hidden)]
    pub fn for_test() -> Self {
        let dir =
            std::fs::canonicalize(std::env::temp_dir()).unwrap_or_else(|_| std::env::temp_dir());
        Self(dir)
    }
}

impl std::fmt::Display for RepoRoot {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.0.display())
    }
}

impl From<RepoRoot> for String {
    fn from(r: RepoRoot) -> String {
        r.0.to_string_lossy().into_owned()
    }
}

impl TryFrom<String> for RepoRoot {
    type Error = RepoRootError;
    fn try_from(s: String) -> Result<Self, Self::Error> {
        RepoRoot::from_persisted(s)
    }
}

/// The run-ambient environment: established once at a run's boundary and
/// propagated parent→child at every sub-workflow spawn. One rail for all
/// run-scoped identity (`repo_root` + correlation ids).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RunEnv {
    /// The repository this run operates on. Mandatory — every run is about a
    /// repo (v0.0.21 precondition: a deployment declares ≥1 writable repo).
    pub repo_root: RepoRoot,
    /// SPEC §20.2 run correlation id. Kept here (not a standalone instance
    /// field) so it survives a sub-workflow spawn.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub run_id: Option<String>,
    /// SPEC §20.2 trace id, same lifecycle as `run_id`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub trace_id: Option<String>,
    /// Run-scoped exclusive-pool leases: `pool name -> leased connection name`.
    ///
    /// A flow declaring `exclusive_pools: [browser]` leases one member of the
    /// `browser` pool at the run boundary; the winner lands here and resolves as
    /// `$.run.leased.browser`, so a browser-touching state uses
    /// `tools: ["{{ $.run.leased.browser }}"]`. Run-ambient (not context) so it
    /// survives a sub-workflow spawn — the browser-touching caps are CHILDREN of
    /// the exploring flow and must reach the same leased server process, or they
    /// would collide on that process's global `select_page` pointer.
    #[serde(default, skip_serializing_if = "std::collections::BTreeMap::is_empty")]
    pub leased: std::collections::BTreeMap<String, String>,
}

impl RunEnv {
    /// Construct from a validated root plus optional correlation ids. Leases are
    /// empty at construction — they are acquired at the run boundary (or
    /// inherited from a parent on spawn), never passed in by a caller.
    pub fn new(repo_root: RepoRoot, run_id: Option<String>, trace_id: Option<String>) -> Self {
        Self {
            repo_root,
            run_id,
            trace_id,
            leased: std::collections::BTreeMap::new(),
        }
    }

    /// The connection leased for `pool` this run, if any.
    pub fn leased_member(&self, pool: &str) -> Option<&str> {
        self.leased.get(pool).map(String::as_str)
    }

    /// Test-only environment (see [`RepoRoot::for_test`] for why this is not
    /// `#[cfg(test)]`).
    #[doc(hidden)]
    pub fn for_test() -> Self {
        Self {
            repo_root: RepoRoot::for_test(),
            run_id: None,
            trace_id: None,
            leased: std::collections::BTreeMap::new(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn new_rejects_relative() {
        assert!(RepoRoot::new("rel/path").is_err());
    }

    #[test]
    fn new_rejects_nonexistent() {
        assert!(RepoRoot::new("/definitely/not/here/praxec-xyz-4a1b").is_err());
    }

    #[test]
    fn new_rejects_a_file() {
        let f = std::env::temp_dir().join(format!("praxec-runenv-{}.tmp", std::process::id()));
        std::fs::write(&f, b"x").unwrap();
        let r = RepoRoot::new(&f);
        let _ = std::fs::remove_file(&f);
        assert!(matches!(r, Err(RepoRootError::NotADir(_))), "{r:?}");
    }

    #[test]
    fn new_accepts_and_canonicalizes_a_dir() {
        let r = RepoRoot::new(std::env::temp_dir()).expect("temp dir is a valid root");
        assert!(r.as_path().is_absolute());
    }

    #[test]
    fn serde_round_trips_as_a_bare_string() {
        let r = RepoRoot::for_test();
        let j = serde_json::to_value(&r).unwrap();
        assert!(j.is_string(), "RepoRoot serializes as a bare string: {j}");
        let back: RepoRoot = serde_json::from_value(j).unwrap();
        assert_eq!(back, r);
    }

    #[test]
    fn from_persisted_is_structural_only() {
        // An absolute path that no longer exists still loads — reload must not
        // re-stat, so a temporarily-unmounted repo stays loadable.
        let r = RepoRoot::from_persisted("/gone/but/absolute").expect("structural ok");
        assert_eq!(r.as_str(), "/gone/but/absolute");
        // A relative persisted value is structurally invalid.
        assert!(RepoRoot::from_persisted("rel").is_err());
    }

    #[test]
    fn run_env_serde_round_trip() {
        let e = RunEnv::new(
            RepoRoot::for_test(),
            Some("run_1".into()),
            Some("trace_1".into()),
        );
        let back: RunEnv = serde_json::from_value(serde_json::to_value(&e).unwrap()).unwrap();
        assert_eq!(back.run_id.as_deref(), Some("run_1"));
        assert_eq!(back.trace_id.as_deref(), Some("trace_1"));
        assert_eq!(back.repo_root, e.repo_root);
    }
}
