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

use serde::{Deserialize, Serialize};

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
        // Cold clone. `git clone --branch <ref>` (the previous implementation)
        // only resolves a branch or tag name — it cannot check out a bare
        // commit SHA, which is exactly what a pinned `praxec.lock` entry
        // needs to seed a never-before-seen cache at. `git fetch <remote>
        // <gitref>` has no such restriction: a branch, tag, OR a full SHA are
        // all legal fetch refspecs. So build the clone by hand from the same
        // primitives the warm-cache update path below already uses (`fetch` +
        // `reset --hard FETCH_HEAD`), rather than `git clone`, so both cases
        // go through one code path with one set of semantics.
        if let Some(parent) = dest.parent() {
            std::fs::create_dir_all(parent)?;
        }
        // `git init` creates `dest` BEFORE `fetch`/`reset` run, so a failure at
        // any later step (e.g. an unreachable remote) would otherwise leave a
        // partial cache behind. Do the cold clone all-or-nothing: on ANY error,
        // remove the partial `dest` before propagating, so a failed clone leaves
        // nothing (a retry does a clean cold clone; callers can rely on "no
        // partial cache on failure"). The warm-update path above is untouched.
        let cold_clone = (|| -> anyhow::Result<()> {
            run_git(&["init", "--quiet", &dest.display().to_string()], None)
                .map_err(|e| anyhow::anyhow!("REPO_CLONE_FAILED: initializing '{uri}': {e}"))?;
            run_git(&["remote", "add", "origin", &url], Some(dest)).map_err(|e| {
                anyhow::anyhow!("REPO_CLONE_FAILED: configuring remote for '{uri}': {e}")
            })?;
            run_git(&["fetch", "origin", gitref], Some(dest)).map_err(|e| {
                anyhow::anyhow!("REPO_CLONE_FAILED: cloning '{uri}' ({gitref}): {e}")
            })?;
            run_git(&["reset", "--hard", "FETCH_HEAD"], Some(dest)).map_err(|e| {
                anyhow::anyhow!("REPO_CLONE_FAILED: checking out '{uri}' ({gitref}): {e}")
            })?;
            Ok(())
        })();
        if let Err(e) = cold_clone {
            let _ = std::fs::remove_dir_all(dest);
            return Err(e);
        }
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

/// Non-blocking pack-staleness warning (`check`/`doctor`) — best-effort git
/// currency snapshot of a local working tree. Every field is derived from
/// LOCAL git state only (`HEAD`, the current branch, `git status`, and any
/// remote-tracking ref git already knows about) — this function NEVER shells
/// out to `git fetch` and never touches the network, so it is always safe to
/// call at `check`/`doctor` time with no connectivity.
///
/// `None` means `path` is not itself the root of a git working tree — either
/// it isn't tracked by git at all, or it's a plain subdirectory of some
/// larger enclosing repo (not a standalone checkout). Both are legitimate,
/// non-git `path:` packs from praxec's point of view: no warning, no error.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GitCurrency {
    /// Current branch name; `None` on a detached `HEAD`.
    pub branch: Option<String>,
    /// The default branch this checkout is compared against: this
    /// checkout's own `refs/remotes/origin/HEAD` when known (so a
    /// `master`/`trunk`/anything-else mainline is judged correctly), else a
    /// `"dev"`-if-known-else-`"main"` convention guess.
    pub default_branch: String,
    /// `true` iff `branch` is exactly `Some(default_branch)`. Always `false`
    /// on a detached `HEAD`.
    pub on_default_branch: bool,
    /// `true` if the working tree has uncommitted changes (tracked
    /// modifications, staged changes, or untracked files), per
    /// `git status --porcelain`.
    pub dirty: bool,
    /// Commits `HEAD` is behind `origin/<default_branch>`, using ONLY the
    /// locally-known remote-tracking ref (no fetch is ever performed).
    /// `None` means that ref isn't known locally — "upstream unknown", not
    /// "zero commits behind".
    pub behind_upstream: Option<u32>,
}

/// Best-effort git currency of `path`. See [`GitCurrency`] for field
/// semantics and the offline/no-panic guarantees. Never errors — a path this
/// can't make sense of (not git, not a repo root, unreadable) is simply
/// `None`.
pub fn git_currency(path: &Path) -> Option<GitCurrency> {
    let canonical = path.canonicalize().ok()?;

    // `path` must itself be the ROOT of a git working tree, not merely a
    // subdirectory living inside some larger enclosing repo (e.g. a test
    // fixture checked into this very workspace) — otherwise every plain-dir
    // `path:` pack that happens to be nested inside an unrelated git repo
    // would spuriously inherit THAT repo's branch/dirty state.
    let toplevel = git_stdout(&["rev-parse", "--show-toplevel"], &canonical)?;
    let toplevel_canonical = Path::new(toplevel.trim()).canonicalize().ok()?;
    if toplevel_canonical != canonical {
        return None;
    }

    // Detached HEAD: `symbolic-ref` fails (HEAD isn't a symbolic ref to a
    // branch) — `git_stdout` treats that as `None`, which is exactly what we
    // want for `branch`.
    let branch = git_stdout(&["symbolic-ref", "--short", "-q", "HEAD"], &canonical)
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty());

    // Default-branch resolution: prefer whatever this checkout's OWN remote
    // says its default branch is (`refs/remotes/origin/HEAD`, set by `git
    // clone` / `git remote set-head origin -a`) — this is what makes a
    // `master`/`trunk`/anything-else mainline resolve correctly instead of
    // being misjudged against praxec's own `dev` convention. Only when that
    // isn't known locally (no `origin`, or `origin/HEAD` was never set) do
    // we fall back to the dev-then-main convention-guess heuristic.
    let default_branch = origin_head_branch(&canonical).unwrap_or_else(|| {
        let has_dev = git_ok(
            &["show-ref", "--verify", "--quiet", "refs/heads/dev"],
            &canonical,
        ) || git_ok(
            &["show-ref", "--verify", "--quiet", "refs/remotes/origin/dev"],
            &canonical,
        );
        if has_dev { "dev" } else { "main" }.to_string()
    });
    let on_default_branch = branch.as_deref() == Some(default_branch.as_str());

    let dirty = git_stdout(&["status", "--porcelain"], &canonical)
        .map(|s| !s.trim().is_empty())
        .unwrap_or(false);

    // Behind-upstream: only computed if the remote-tracking ref is already
    // known locally (never fetched here). `rev-list --count HEAD..origin/X`
    // counts commits reachable from `origin/X` but not `HEAD` — i.e. exactly
    // how far behind that tip `HEAD` is.
    let upstream_ref = format!("refs/remotes/origin/{default_branch}");
    let behind_upstream = if git_ok(
        &["show-ref", "--verify", "--quiet", &upstream_ref],
        &canonical,
    ) {
        git_stdout(
            &[
                "rev-list",
                "--count",
                &format!("HEAD..origin/{default_branch}"),
            ],
            &canonical,
        )
        .and_then(|s| s.trim().parse::<u32>().ok())
    } else {
        None
    };

    Some(GitCurrency {
        branch,
        default_branch,
        on_default_branch,
        dirty,
        behind_upstream,
    })
}

/// pack-provenance-recording — the durable "what exactly ran" record for one
/// loaded pack: `{ namespace, source, sha, ref, dirty }`. RECORDS, never
/// CONSTRAINS — this is emitted as a `pack.provenance` audit event (the
/// governance trail) and surfaced live via `discovery::home()`'s
/// `loaded_packs`. Both surfaces are built from [`pack_provenance`], which
/// itself reuses [`git_currency`] — one git introspection, two outputs (the
/// staleness WARN and this record).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PackProvenance {
    /// The pack's declared `praxec.repo.yaml` namespace.
    pub namespace: String,
    /// How the operator declared the entry — the literal `path:` or `uri:`
    /// string (same convention the staleness warnings name a repo by).
    pub source: String,
    /// The exact `HEAD` commit driving this pack's currently-loaded content.
    /// `None` when `repo_path` isn't a git working tree (a plain `path:`
    /// pack) — never a load failure.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub sha: Option<String>,
    /// The operator's declared `ref:` for a remote pack, or the branch name
    /// for a local checkout. `None` when neither is known (non-git pack, or
    /// a local detached HEAD).
    #[serde(rename = "ref", skip_serializing_if = "Option::is_none")]
    pub git_ref: Option<String>,
    /// Uncommitted local changes, per `git status --porcelain`. `None` for a
    /// non-git pack (there is no working tree to be dirty).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub dirty: Option<bool>,
}

/// Compute one pack's provenance record. `repo_path` is the pack's resolved
/// root — a local `path:` checkout, or a remote `uri:` pack's clone-cache
/// dir (`clone_or_update` always leaves a normal, non-bare git working tree
/// there, so [`git_currency`] applies to it exactly the same way).
/// `declared_ref` is the operator's declared `ref:` for a remote pack — pass
/// `Some(gitref)` there and `None` for a local `path:` pack. It takes
/// PRIORITY over the checkout's own branch name when present: `clone_or_update`
/// cold-clones via `git init` (no `-b`), so a remote pack's cache-dir branch
/// name is an internal artifact of the local git's default-branch config, not
/// a meaningful ref — the operator's declared ref is the one that actually
/// answers "which version of this pack is loaded".
///
/// Offline (delegates to [`git_currency`] + a local `git rev-parse HEAD`) and
/// infallible: a pack that isn't a git working tree yields a record with
/// `sha`/`git_ref`/`dirty` all `None` rather than an error — provenance
/// RECORDS, it never blocks a load.
pub fn pack_provenance(
    namespace: &str,
    source: &str,
    repo_path: &Path,
    declared_ref: Option<&str>,
) -> PackProvenance {
    match git_currency(repo_path) {
        Some(currency) => PackProvenance {
            namespace: namespace.to_string(),
            source: source.to_string(),
            sha: resolved_sha(repo_path),
            git_ref: declared_ref.map(str::to_string).or(currency.branch),
            dirty: Some(currency.dirty),
        },
        None => PackProvenance {
            namespace: namespace.to_string(),
            source: source.to_string(),
            sha: None,
            git_ref: None,
            dirty: None,
        },
    }
}

/// `git rev-parse HEAD` in `path` — the exact commit sha driving this pack's
/// currently-loaded content. Offline (no fetch). `None` if `path` isn't a
/// git repo, `HEAD` is unborn, or the command fails to run — never panics.
pub fn resolved_sha(path: &Path) -> Option<String> {
    git_stdout(&["rev-parse", "HEAD"], path).map(|s| s.trim().to_string())
}

/// This checkout's OWN notion of its default branch, per its remote:
/// `refs/remotes/origin/HEAD` (set by `git clone`, or explicitly by `git
/// remote set-head origin -a`) resolved and stripped of the `origin/`
/// prefix — e.g. `"master"`, `"trunk"`, whatever the remote actually uses.
/// `None` if that ref isn't set locally (no `origin` remote, or it was never
/// established) — purely local, no fetch. Never panics.
fn origin_head_branch(cwd: &Path) -> Option<String> {
    let short = git_stdout(
        &["symbolic-ref", "--short", "-q", "refs/remotes/origin/HEAD"],
        cwd,
    )?;
    short.trim().strip_prefix("origin/").map(str::to_string)
}

/// `true` iff `git <args>` (run in `cwd`) exits successfully. Never panics —
/// a failure to even spawn `git` is treated as "no" (offline/degraded
/// environments must degrade to "skip the check", not crash `check`).
fn git_ok(args: &[&str], cwd: &Path) -> bool {
    Command::new("git")
        .arg("-C")
        .arg(cwd)
        .args(args)
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false)
}

/// `git <args>`'s stdout (run in `cwd`), or `None` if the command failed to
/// run or exited non-zero. Never panics.
fn git_stdout(args: &[&str], cwd: &Path) -> Option<String> {
    Command::new("git")
        .arg("-C")
        .arg(cwd)
        .args(args)
        .output()
        .ok()
        .filter(|o| o.status.success())
        .map(|o| String::from_utf8_lossy(&o.stdout).into_owned())
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
    fn cold_clone_a_bare_sha_materializes_exact_commit() {
        // The concrete gap from the pack-currency design spec (increment A):
        // `git clone --branch <ref>` (the pre-fix first-clone path) cannot
        // resolve a bare commit SHA — only a branch or tag name. A lockfile
        // (increment B) needs to seed a cold cache at an exact SHA, so the
        // cold-clone path must support that today. No network: `seed_origin`
        // + a `file://` URI.
        let tmp = tempfile::tempdir().unwrap();
        let origin = tmp.path().join("origin");
        std::fs::create_dir_all(&origin).unwrap();
        seed_origin(&origin);
        let origin_uri = format!("file://{}", origin.display());

        // Pin to the first commit's exact SHA before origin advances any further.
        let sha_out = Command::new("git")
            .arg("-C")
            .arg(&origin)
            .args(["rev-parse", "HEAD"])
            .output()
            .unwrap();
        let pinned_sha = String::from_utf8_lossy(&sha_out.stdout).trim().to_string();
        assert!(!pinned_sha.is_empty(), "expected a resolvable HEAD sha");

        // Advance origin with a second commit AFTER capturing the pin, so a
        // naive "clone the branch tip" would land on the wrong commit.
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

        // Cold-clone: `dest` has never been cloned before, and `gitref` is a
        // bare SHA, not a branch/tag.
        let dest = tmp.path().join("cache").join(cache_dir_name(&origin_uri));
        clone_or_update(&origin_uri, &pinned_sha, &dest)
            .expect("cold-clone pinned to a bare commit SHA must succeed");

        assert!(dest.join("praxec.repo.yaml").exists());
        assert!(
            !dest.join("extra.txt").exists(),
            "cold clone pinned to a bare SHA must not include commits made after that SHA"
        );

        let head_out = Command::new("git")
            .arg("-C")
            .arg(&dest)
            .args(["rev-parse", "HEAD"])
            .output()
            .unwrap();
        let materialized = String::from_utf8_lossy(&head_out.stdout).trim().to_string();
        assert_eq!(
            materialized, pinned_sha,
            "materialized HEAD must equal the pinned SHA exactly"
        );
    }

    #[test]
    fn cold_clone_a_branch_ref_into_a_fresh_cache_still_works() {
        // Regression guard for the fix above: the ordinary branch/tag
        // cold-clone path (the common case) must be unaffected.
        let tmp = tempfile::tempdir().unwrap();
        let origin = tmp.path().join("origin");
        std::fs::create_dir_all(&origin).unwrap();
        seed_origin(&origin);
        let origin_uri = format!("file://{}", origin.display());

        let dest = tmp.path().join("cache").join(cache_dir_name(&origin_uri));
        clone_or_update(&origin_uri, "main", &dest)
            .expect("cold-clone pinned to a branch name must still succeed");
        assert!(dest.join("praxec.repo.yaml").exists());

        let head_out = Command::new("git")
            .arg("-C")
            .arg(&dest)
            .args(["rev-parse", "HEAD"])
            .output()
            .unwrap();
        let branch_out = Command::new("git")
            .arg("-C")
            .arg(&origin)
            .args(["rev-parse", "main"])
            .output()
            .unwrap();
        assert_eq!(
            String::from_utf8_lossy(&head_out.stdout).trim(),
            String::from_utf8_lossy(&branch_out.stdout).trim(),
            "cold clone by branch name must land on that branch's tip"
        );
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

    // ---------- git_currency (pack-staleness-warning) ----------

    /// `git init -b <branch>` + one commit, so `HEAD` is a real ref (not an
    /// unborn branch) and `git status --porcelain` reports clean.
    fn init_repo(dir: &Path, branch: &str) {
        Command::new("git")
            .arg("init")
            .arg("-b")
            .arg(branch)
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
    fn non_git_dir_is_none() {
        let tmp = tempfile::tempdir().unwrap();
        assert_eq!(git_currency(tmp.path()), None);
    }

    #[test]
    fn subdir_of_a_larger_repo_is_none() {
        // This source file lives inside the workspace's own git repo — a
        // subdirectory of it is NOT itself a standalone pack checkout, so it
        // must not inherit the enclosing repo's branch/dirty state.
        let here = Path::new(env!("CARGO_MANIFEST_DIR"));
        assert_eq!(
            git_currency(here),
            None,
            "a subdir of the enclosing workspace repo must not report currency"
        );
    }

    #[test]
    fn clean_checkout_on_dev_has_no_drift_no_dirt_unknown_upstream() {
        let tmp = tempfile::tempdir().unwrap();
        let repo = tmp.path().join("pack");
        std::fs::create_dir_all(&repo).unwrap();
        init_repo(&repo, "dev");

        let c = git_currency(&repo).expect("is a git repo root");
        assert_eq!(c.branch.as_deref(), Some("dev"));
        assert_eq!(c.default_branch, "dev");
        assert!(c.on_default_branch);
        assert!(!c.dirty);
        assert_eq!(
            c.behind_upstream, None,
            "no origin configured — upstream is unknown, not zero"
        );
    }

    #[test]
    fn feature_branch_checkout_is_off_default_branch() {
        let tmp = tempfile::tempdir().unwrap();
        let repo = tmp.path().join("pack");
        std::fs::create_dir_all(&repo).unwrap();
        init_repo(&repo, "dev");
        git(&["checkout", "-b", "feat/react-review"], &repo);

        let c = git_currency(&repo).expect("is a git repo root");
        assert_eq!(c.branch.as_deref(), Some("feat/react-review"));
        assert_eq!(
            c.default_branch, "dev",
            "the repo's local `dev` branch is still the convention default"
        );
        assert!(!c.on_default_branch);
    }

    #[test]
    fn detached_head_reports_no_branch() {
        let tmp = tempfile::tempdir().unwrap();
        let repo = tmp.path().join("pack");
        std::fs::create_dir_all(&repo).unwrap();
        init_repo(&repo, "dev");
        let sha_out = Command::new("git")
            .arg("-C")
            .arg(&repo)
            .args(["rev-parse", "HEAD"])
            .output()
            .unwrap();
        let sha = String::from_utf8_lossy(&sha_out.stdout).trim().to_string();
        git(&["checkout", &sha], &repo);

        let c = git_currency(&repo).expect("is a git repo root");
        assert_eq!(c.branch, None, "detached HEAD has no branch name");
        assert!(!c.on_default_branch);
    }

    #[test]
    fn uncommitted_changes_are_dirty() {
        let tmp = tempfile::tempdir().unwrap();
        let repo = tmp.path().join("pack");
        std::fs::create_dir_all(&repo).unwrap();
        init_repo(&repo, "dev");
        std::fs::write(
            repo.join("praxec.repo.yaml"),
            "schema: praxec.repo/v1\nextra: 1\n",
        )
        .unwrap();

        let c = git_currency(&repo).expect("is a git repo root");
        assert!(c.dirty);
        assert!(
            c.on_default_branch,
            "dirty is independent of branch drift — this checkout IS on dev"
        );
    }

    #[test]
    fn behind_known_upstream_reports_commit_count() {
        let tmp = tempfile::tempdir().unwrap();
        let origin = tmp.path().join("origin");
        std::fs::create_dir_all(&origin).unwrap();
        init_repo(&origin, "dev");
        let origin_uri = format!("file://{}", origin.display());

        let work = tmp.path().join("work");
        Command::new("git")
            .args([
                "clone",
                "--branch",
                "dev",
                &origin_uri,
                &work.display().to_string(),
            ])
            .output()
            .unwrap();

        // Origin advances by one commit.
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
                "advance",
            ],
            &origin,
        );
        // The clone learns about it via a LOCAL fetch (arrange step — the
        // production `git_currency` call below never fetches).
        git(&["fetch", "origin", "dev"], &work);

        let c = git_currency(&work).expect("is a git repo root");
        assert!(c.on_default_branch);
        assert!(!c.dirty);
        assert_eq!(c.behind_upstream, Some(1));
    }

    /// A clean repo whose mainline is `master` (not `dev`) must NOT be
    /// misjudged against praxec's own `dev`-then-`main` convention guess: a
    /// normal (non `--single-branch`) `git clone` sets
    /// `refs/remotes/origin/HEAD`, which `git_currency` must consult FIRST.
    #[test]
    fn clean_master_repo_with_origin_head_is_not_flagged_as_drift() {
        let tmp = tempfile::tempdir().unwrap();
        let origin = tmp.path().join("origin");
        init_repo(&origin, "master");
        let origin_uri = format!("file://{}", origin.display());

        let work = tmp.path().join("work");
        // Deliberately NOT `--single-branch`/`--branch`: a plain clone is
        // exactly what sets `refs/remotes/origin/HEAD` to the remote's own
        // default branch.
        Command::new("git")
            .args(["clone", &origin_uri, &work.display().to_string()])
            .output()
            .unwrap();

        let c = git_currency(&work).expect("is a git repo root");
        assert_eq!(c.branch.as_deref(), Some("master"));
        assert_eq!(
            c.default_branch, "master",
            "origin/HEAD must be consulted before the dev/main convention guess"
        );
        assert!(
            c.on_default_branch,
            "a clean checkout on its own remote's default branch must never drift"
        );
    }

    /// A brand-new repo with NO commits (unborn `HEAD`) must never panic —
    /// every helper here is exercised (toplevel resolution, symbolic-ref,
    /// show-ref, status, rev-list-gating) before any commit exists.
    #[test]
    fn unborn_head_does_not_panic() {
        let tmp = tempfile::tempdir().unwrap();
        let repo = tmp.path().join("pack");
        std::fs::create_dir_all(&repo).unwrap();
        Command::new("git")
            .arg("init")
            .arg("-b")
            .arg("dev")
            .arg(&repo)
            .output()
            .unwrap();
        // No `praxec.repo.yaml`, no `git add`, no commit — HEAD is unborn.

        let result = std::panic::catch_unwind(|| git_currency(&repo));
        assert!(result.is_ok(), "must never panic on an unborn HEAD");
        let currency = result
            .unwrap()
            .expect("an unborn repo is still a repo root");
        // `symbolic-ref` reads HEAD's symbolic target regardless of whether
        // it resolves to a commit yet, so the branch NAME is still known.
        assert_eq!(currency.branch.as_deref(), Some("dev"));
        // No origin, so upstream is unknown, not a crash and not a
        // fabricated zero.
        assert_eq!(currency.behind_upstream, None);
    }

    // ---------- pack_provenance (pack-provenance-recording, P1/P2) ----------

    #[test]
    fn clean_git_pack_provenance_carries_namespace_sha_ref_dirty() {
        let tmp = tempfile::tempdir().unwrap();
        let repo = tmp.path().join("pack");
        std::fs::create_dir_all(&repo).unwrap();
        init_repo(&repo, "dev");
        let expected_sha = git_stdout(&["rev-parse", "HEAD"], &repo)
            .expect("HEAD resolves")
            .trim()
            .to_string();

        let prov = pack_provenance("acme", "/some/path", &repo, None);
        assert_eq!(prov.namespace, "acme");
        assert_eq!(prov.source, "/some/path");
        assert_eq!(prov.sha.as_deref(), Some(expected_sha.as_str()));
        assert_eq!(prov.git_ref.as_deref(), Some("dev"));
        assert_eq!(prov.dirty, Some(false));
    }

    #[test]
    fn dirty_git_pack_provenance_reports_dirty_true() {
        let tmp = tempfile::tempdir().unwrap();
        let repo = tmp.path().join("pack");
        std::fs::create_dir_all(&repo).unwrap();
        init_repo(&repo, "dev");
        std::fs::write(repo.join("NOTES.md"), "uncommitted\n").unwrap();

        let prov = pack_provenance("acme", "/some/path", &repo, None);
        assert_eq!(prov.dirty, Some(true));
        assert!(prov.sha.is_some(), "dirty is independent of sha resolution");
    }

    #[test]
    fn non_git_path_pack_provenance_has_no_sha_ref_or_dirty() {
        let tmp = tempfile::tempdir().unwrap();
        let plain = tmp.path().join("plain-pack");
        std::fs::create_dir_all(&plain).unwrap();

        let prov = pack_provenance("plain", "/plain/path", &plain, None);
        assert_eq!(prov.namespace, "plain");
        assert_eq!(prov.source, "/plain/path");
        assert_eq!(
            prov.sha, None,
            "no git — provenance records absence, never fails"
        );
        assert_eq!(prov.git_ref, None);
        assert_eq!(prov.dirty, None);
    }

    #[test]
    fn remote_pack_provenance_uses_the_declared_ref_not_the_cache_dirs_local_branch_name() {
        // `clone_or_update` cold-clones via `git init` (no `-b`), so the cache
        // dir's own branch name is whatever the local git's
        // `init.defaultBranch` happens to be — an internal artifact, NOT the
        // ref the operator actually pinned. Provenance must report the
        // DECLARED ref, regardless of what that artifact branch is named.
        let tmp = tempfile::tempdir().unwrap();
        let origin = tmp.path().join("origin");
        std::fs::create_dir_all(&origin).unwrap();
        init_repo(&origin, "main");
        let origin_uri = format!("file://{}", origin.display());

        let dest = tmp.path().join("cache").join(cache_dir_name(&origin_uri));
        clone_or_update(&origin_uri, "main", &dest).unwrap();

        let expected_sha = git_stdout(&["rev-parse", "HEAD"], &dest)
            .expect("HEAD resolves")
            .trim()
            .to_string();

        let prov = pack_provenance("remote-pack", &origin_uri, &dest, Some("main"));
        assert_eq!(prov.sha.as_deref(), Some(expected_sha.as_str()));
        assert_eq!(
            prov.git_ref.as_deref(),
            Some("main"),
            "the declared ref wins over the cache dir's own (artifact) branch name"
        );
        assert_eq!(prov.dirty, Some(false));
    }

    #[test]
    fn resolved_sha_matches_git_rev_parse_head() {
        let tmp = tempfile::tempdir().unwrap();
        let repo = tmp.path().join("pack");
        std::fs::create_dir_all(&repo).unwrap();
        init_repo(&repo, "dev");
        let expected = git_stdout(&["rev-parse", "HEAD"], &repo)
            .unwrap()
            .trim()
            .to_string();
        assert_eq!(resolved_sha(&repo), Some(expected));
    }

    #[test]
    fn resolved_sha_is_none_for_a_non_git_dir() {
        let tmp = tempfile::tempdir().unwrap();
        assert_eq!(resolved_sha(tmp.path()), None);
    }
}
