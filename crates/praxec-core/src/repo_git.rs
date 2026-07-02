//! SPEC §9 + §8.4 — git operations for **remote** resource repos: importing
//! (clone/update a repo declared by URI into a local cache) and publishing
//! (push a writable repo's authored commits back to its remote).
//!
//! Everything shells out to `git`, inheriting the operator's existing git auth
//! (SSH key / credential helper / cached token / `gh`). Praxec never stores
//! or manages git credentials: if `git clone`/`git push` works in the operator's
//! shell, it works here. Headless/CI configures git the usual way.

use std::path::{Path, PathBuf};
use std::process::Command;

/// The git-cloneable URL for a repo `uri`. `git+https://…` → `https://…`;
/// `git+ssh://…` → `ssh://…`; a bare `file://…` or local path passes through
/// (so local mirrors + tests work without a network).
pub fn clone_url(uri: &str) -> String {
    uri.strip_prefix("git+")
        .map(str::to_string)
        .unwrap_or_else(|| uri.to_string())
}

/// A stable, filesystem-safe directory name derived from a repo URI — the
/// cache slot a remote repo clones into. Non-alphanumerics collapse to `-`.
pub fn cache_dir_name(uri: &str) -> String {
    let mut out = String::with_capacity(uri.len());
    let mut last_dash = false;
    for c in uri.chars() {
        if c.is_ascii_alphanumeric() {
            out.push(c);
            last_dash = false;
        } else if !last_dash {
            out.push('-');
            last_dash = true;
        }
    }
    out.trim_matches('-').to_string()
}

fn run_git(args: &[&str], cwd: Option<&Path>) -> anyhow::Result<()> {
    let mut cmd = Command::new("git");
    if let Some(dir) = cwd {
        cmd.arg("-C").arg(dir);
    }
    cmd.args(args);
    let out = cmd
        .output()
        .map_err(|e| anyhow::anyhow!("running `git {}`: {e} (is git on PATH?)", args.join(" ")))?;
    if !out.status.success() {
        anyhow::bail!(
            "`git {}` failed: {}",
            args.join(" "),
            String::from_utf8_lossy(&out.stderr).trim()
        );
    }
    Ok(())
}

/// Import a remote repo at `uri`@`gitref` into `dest` (idempotent): clone on
/// first use, otherwise fetch + hard-reset to the ref. Returns `dest`. Uses the
/// operator's git auth (no credentials handled here).
pub fn clone_or_update(uri: &str, gitref: &str, dest: &Path) -> anyhow::Result<PathBuf> {
    let url = clone_url(uri);
    if dest.join(".git").is_dir() {
        // The `.git` presence heuristic is not enough: a partial/interrupted
        // clone leaves a `.git` but isn't a healthy repo, and `fetch` would then
        // fail obscurely. Verify it, and fail-fast with an actionable remedy
        // rather than papering over a broken cache.
        run_git(&["rev-parse", "--git-dir"], Some(dest)).map_err(|e| {
            anyhow::anyhow!(
                "REPO_CACHE_CORRUPT: '{}' has a .git but is not a healthy clone ({e}). \
                 Remove the cache and retry: rm -rf {}",
                dest.display(),
                dest.display()
            )
        })?;
        // Already cloned — update to the pinned ref without re-downloading history.
        run_git(&["fetch", "origin", gitref], Some(dest)).map_err(|e| {
            anyhow::anyhow!(
                "REPO_FETCH_FAILED: updating '{uri}' in {}: {e}",
                dest.display()
            )
        })?;
        run_git(&["reset", "--hard", "FETCH_HEAD"], Some(dest))?;
    } else {
        if let Some(parent) = dest.parent() {
            std::fs::create_dir_all(parent)?;
        }
        run_git(
            &[
                "clone",
                "--branch",
                gitref,
                "--single-branch",
                &url,
                &dest.display().to_string(),
            ],
            None,
        )
        .map_err(|e| anyhow::anyhow!("REPO_CLONE_FAILED: cloning '{uri}' ({gitref}): {e}"))?;
    }
    Ok(dest.to_path_buf())
}

/// Publish a writable repo's commits: `git push` from `root` to its tracked
/// remote/branch. Inherits the operator's git auth. Surfaces a
/// `REPO_PUSH_FAILED` error (e.g. no remote, rejected, auth) rather than
/// swallowing it — publishing to a shared remote is not best-effort.
pub fn push(root: &Path) -> anyhow::Result<()> {
    run_git(&["push"], Some(root))
        .map_err(|e| anyhow::anyhow!("REPO_PUSH_FAILED: pushing {}: {e}", root.display()))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn git(args: &[&str], cwd: &Path) {
        assert!(
            Command::new("git")
                .arg("-C")
                .arg(cwd)
                .args(args)
                .output()
                .unwrap()
                .status
                .success(),
            "git {args:?} in {} failed",
            cwd.display()
        );
    }

    /// A local "remote": a non-bare repo with one commit we can clone over file://.
    fn seed_origin(dir: &Path) {
        Command::new("git")
            .arg("init")
            .arg("-b")
            .arg("main")
            .arg(dir)
            .output()
            .unwrap();
        std::fs::write(dir.join("praxec.repo.yaml"), "schema: praxec.repo/v1\n").unwrap();
        git(&["add", "."], dir);
        git(
            &[
                "-c",
                "user.email=t@t",
                "-c",
                "user.name=t",
                "commit",
                "-m",
                "seed",
            ],
            dir,
        );
    }

    #[test]
    fn clone_url_strips_the_git_prefix() {
        assert_eq!(clone_url("git+https://h/r"), "https://h/r");
        assert_eq!(clone_url("https://h/r"), "https://h/r");
        assert_eq!(clone_url("file:///tmp/r"), "file:///tmp/r");
    }

    #[test]
    fn cache_dir_name_is_filesystem_safe_and_stable() {
        let a = cache_dir_name("git+https://github.com/acme/repo@main");
        assert!(!a.contains('/') && !a.contains(':') && !a.contains('@'));
        assert_eq!(a, cache_dir_name("git+https://github.com/acme/repo@main"));
    }

    #[test]
    fn clone_then_update_is_idempotent() {
        let tmp = tempfile::tempdir().unwrap();
        let origin = tmp.path().join("origin");
        std::fs::create_dir_all(&origin).unwrap();
        seed_origin(&origin);
        let origin_uri = format!("file://{}", origin.display());

        let dest = tmp.path().join("cache").join(cache_dir_name(&origin_uri));
        // First call clones.
        clone_or_update(&origin_uri, "main", &dest).unwrap();
        assert!(dest.join("praxec.repo.yaml").exists());

        // A new commit on origin, then a second call updates (no re-clone error).
        std::fs::write(origin.join("extra.txt"), "x").unwrap();
        git(&["add", "."], &origin);
        git(
            &[
                "-c",
                "user.email=t@t",
                "-c",
                "user.name=t",
                "commit",
                "-m",
                "more",
            ],
            &origin,
        );
        clone_or_update(&origin_uri, "main", &dest).unwrap();
        assert!(
            dest.join("extra.txt").exists(),
            "update pulled the new commit"
        );
    }

    #[test]
    fn push_propagates_commits_to_origin() {
        let tmp = tempfile::tempdir().unwrap();
        // Bare origin so it can be pushed to.
        let origin = tmp.path().join("origin.git");
        Command::new("git")
            .arg("init")
            .arg("--bare")
            .arg("-b")
            .arg("main")
            .arg(&origin)
            .output()
            .unwrap();
        let work = tmp.path().join("work");
        Command::new("git")
            .args([
                "clone",
                &format!("file://{}", origin.display()),
                &work.display().to_string(),
            ])
            .output()
            .unwrap();
        std::fs::write(work.join("f.txt"), "hi").unwrap();
        git(&["add", "."], &work);
        git(
            &[
                "-c",
                "user.email=t@t",
                "-c",
                "user.name=t",
                "commit",
                "-m",
                "c",
            ],
            &work,
        );

        push(&work).unwrap();

        // The bare origin now has the commit on main.
        let log = Command::new("git")
            .arg("-C")
            .arg(&origin)
            .args(["log", "--oneline", "main"])
            .output()
            .unwrap();
        assert!(String::from_utf8_lossy(&log.stdout).contains("c"));
    }
}
