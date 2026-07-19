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
    #[error(
        "'{0}' resolves outside the repo root through a symlink; a link inside the root \
         may not point outside it (writing through it would escape the declared scope)"
    )]
    SymlinkEscape(String),
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

/// Resolve `rel` under `root` AND prove the result is still inside `root` after
/// the filesystem resolves symlinks. Use this on every **write** path.
///
/// [`resolve_under`] is lexical only, so a link inside the root that points
/// outside it — no `..`, not absolute — passes cleanly and then writes wherever
/// it points. That defeats scope confinement entirely: an agent rooted at
/// `<repo>/src` reaches `<repo>/tests` through one `ln -s`.
///
/// The invariant enforced here is **containment, not link-freedom**: a symlink
/// that stays inside the root is fine, because it cannot widen the agent's reach.
///
/// A path that does not exist yet is legal (the write host creates it) — only
/// the deepest existing ancestor is resolved. The final component is checked
/// with `symlink_metadata` (no-follow), so a link planted at the target itself
/// is caught even when its parent is legitimately inside the root.
pub fn resolve_under_no_symlink_escape(root: &Path, rel: &str) -> Result<PathBuf, PathSafetyError> {
    // Lexical first: cheap, and it preserves the existing typed errors.
    let joined = resolve_under(root, rel)?;

    let escape = || PathSafetyError::SymlinkEscape(rel.to_string());

    // Resolve the root itself — it may legitimately be reached through a link
    // (a worktree, /tmp on macOS), in which case every legal path would look
    // like an escape once canonicalised.
    let real_root = root.canonicalize().map_err(|_| escape())?;

    // The final component must not itself be a symlink: its parent can be
    // perfectly legal while the leaf points anywhere. `symlink_metadata` does
    // not follow, which is the check `O_NOFOLLOW` would give us at open time.
    if let Ok(md) = std::fs::symlink_metadata(&joined) {
        if md.file_type().is_symlink() {
            return Err(escape());
        }
    }

    // Walk up to the deepest ancestor that exists and canonicalise that. For a
    // not-yet-created file this is its nearest existing parent directory.
    let mut probe = joined.as_path();
    let real_existing = loop {
        match probe.canonicalize() {
            Ok(p) => break p,
            Err(_) => match probe.parent() {
                // Ran out of ancestors without reaching the root: the join was
                // never under it to begin with.
                Some(parent) => probe = parent,
                None => return Err(escape()),
            },
        }
    };
    if !real_existing.starts_with(&real_root) {
        return Err(escape());
    }
    Ok(joined)
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

    // --- filesystem-aware containment (write path) --------------------------
    //
    // The lexical guard above cannot see symlinks. A link INSIDE the root that
    // points OUTSIDE it contains no `..` and is not absolute, so it passes every
    // check above and then writes wherever it points. For the role-separation
    // guarantee (a fixer rooted at `<repo>/src` must not reach `<repo>/tests`)
    // that is the whole ballgame, so the write path resolves for real.

    #[test]
    fn write_path_allows_an_ordinary_relative_path() {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(tmp.path().join("src")).unwrap();
        let got = resolve_under_no_symlink_escape(tmp.path(), "src/lib.rs").unwrap();
        assert_eq!(got, tmp.path().join("src/lib.rs"));
    }

    #[test]
    fn write_path_allows_creating_a_new_nested_file() {
        // A file that does not exist yet is legal — the write host creates it.
        // Only the deepest EXISTING ancestor is resolved.
        let tmp = tempfile::tempdir().unwrap();
        let got = resolve_under_no_symlink_escape(tmp.path(), "a/b/c.rs").unwrap();
        assert_eq!(got, tmp.path().join("a/b/c.rs"));
    }

    #[cfg(unix)]
    #[test]
    fn write_through_symlinked_directory_outside_root_is_refused() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().join("root");
        let outside = tmp.path().join("outside");
        std::fs::create_dir_all(&root).unwrap();
        std::fs::create_dir_all(&outside).unwrap();
        // `root/escape` -> `outside`. No `..`, not absolute: lexically clean.
        std::os::unix::fs::symlink(&outside, root.join("escape")).unwrap();

        // The lexical guard waves it through — this is the defect.
        assert!(resolve_under(&root, "escape/pwned.rs").is_ok());

        // The filesystem-aware guard refuses it.
        assert!(matches!(
            resolve_under_no_symlink_escape(&root, "escape/pwned.rs"),
            Err(PathSafetyError::SymlinkEscape(_))
        ));
    }

    #[cfg(unix)]
    #[test]
    fn write_to_a_symlinked_file_outside_root_is_refused() {
        // The final component itself is the link (O_NOFOLLOW case): the parent is
        // legitimately inside the root, so only checking ancestors would miss it.
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().join("root");
        let outside = tmp.path().join("outside");
        std::fs::create_dir_all(&root).unwrap();
        std::fs::create_dir_all(&outside).unwrap();
        let target = outside.join("oracle.rs");
        std::fs::write(&target, "approved test").unwrap();
        std::os::unix::fs::symlink(&target, root.join("innocent.rs")).unwrap();

        assert!(matches!(
            resolve_under_no_symlink_escape(&root, "innocent.rs"),
            Err(PathSafetyError::SymlinkEscape(_))
        ));
    }

    #[cfg(unix)]
    #[test]
    fn a_symlink_that_stays_inside_the_root_is_allowed() {
        // Not every symlink is an escape. Refusing all of them would break
        // legitimate in-repo links; the invariant is CONTAINMENT, not link-freedom.
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().join("root");
        std::fs::create_dir_all(root.join("real")).unwrap();
        std::os::unix::fs::symlink(root.join("real"), root.join("alias")).unwrap();

        assert!(resolve_under_no_symlink_escape(&root, "alias/x.rs").is_ok());
    }

    #[cfg(unix)]
    #[test]
    fn a_symlinked_root_is_itself_resolved() {
        // If the ROOT is reached through a link, every legal path would otherwise
        // look like an escape once canonicalised. Resolve the root too.
        let tmp = tempfile::tempdir().unwrap();
        let real = tmp.path().join("real_root");
        std::fs::create_dir_all(real.join("src")).unwrap();
        let linked = tmp.path().join("linked_root");
        std::os::unix::fs::symlink(&real, &linked).unwrap();

        assert!(resolve_under_no_symlink_escape(&linked, "src/lib.rs").is_ok());
    }
}
