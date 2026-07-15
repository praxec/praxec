//! Shared repo-root path-safety (v0.0.21).
//!
//! One traversal guard used by BOTH the trusted file-edit tool host (which
//! writes under a repo root) and the `path_grounding` gate (which checks that
//! agent-referenced paths exist under the run's root). A relative path that
//! stays inside the root resolves; an absolute path or any `..` escape is
//! refused — never a silent operation outside scope.

use std::path::{Component, Path, PathBuf};

use thiserror::Error;

/// Why a candidate path is unsafe relative to a repo root.
#[derive(Debug, Error, PartialEq, Eq)]
pub enum PathSafetyError {
    #[error("'{0}' is absolute; paths must be relative to the repo root")]
    Absolute(String),
    #[error("'{0}' contains '..'; paths may not escape the repo root")]
    ParentEscape(String),
}

/// Resolve `rel` under `root`, refusing absolute paths and any `..` escape.
///
/// This is purely lexical — it does NOT touch the filesystem, so callers decide
/// whether the resolved path must exist (the grounding gate) or may be created
/// (the write host).
pub fn resolve_under(root: &Path, rel: &str) -> Result<PathBuf, PathSafetyError> {
    let p = Path::new(rel);
    if p.is_absolute() {
        return Err(PathSafetyError::Absolute(rel.to_string()));
    }
    if p.components().any(|c| matches!(c, Component::ParentDir)) {
        return Err(PathSafetyError::ParentEscape(rel.to_string()));
    }
    Ok(root.join(p))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn joins_a_relative_path_under_root() {
        let got = resolve_under(Path::new("/repo"), "src/lib.rs").unwrap();
        assert_eq!(got, PathBuf::from("/repo/src/lib.rs"));
    }

    #[test]
    fn rejects_absolute() {
        assert_eq!(
            resolve_under(Path::new("/repo"), "/etc/passwd"),
            Err(PathSafetyError::Absolute("/etc/passwd".into()))
        );
    }

    #[test]
    fn rejects_parent_escape() {
        assert!(matches!(
            resolve_under(Path::new("/repo"), "../../etc/passwd"),
            Err(PathSafetyError::ParentEscape(_))
        ));
    }
}
