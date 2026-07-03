//! ADR-0006 / ADR-0007 — the **promotion bridge**: apply an agent's diff back
//! onto the live tree, coordinated *at promotion* (not during exploration).
//!
//! It computes the **observed** touched-set from the patch (do-then-declare —
//! no up-front file declaration), acquires a [`RepoLocks`] lock on exactly those
//! files for the brief apply window, **3-way merges** the patch onto the live
//! tree (`git apply --3way`), then releases. Disjoint observed-sets apply
//! concurrently conflict-free; a genuine overlap returns a detected
//! [`PromotionOutcome::Conflict`] (never a silent clobber). The git mechanics were
//! validated by the ADR-0006 sandbox-exec coordination spike.

use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::Duration;

use crate::repo_locks::{LockConflict, RepoLocks};
use crate::sandbox::{Egress, ResourceLimits, SandboxOutput, SandboxProvider, SandboxSpec};

/// The result of promoting a patch onto the live tree.
#[derive(Debug)]
pub enum PromotionOutcome {
    /// The patch 3-way merged cleanly; `files` is the observed touched-set.
    Applied { files: Vec<PathBuf> },
    /// A genuine 3-way conflict — surfaced (conflict markers in the tree), never
    /// silently merged. `files` is the observed touched-set.
    Conflict { files: Vec<PathBuf> },
    /// The observed files are held by another holder — promotion must wait.
    Locked(LockConflict),
}

/// How long the apply-window lock is held before its TTL reaps it.
const APPLY_LOCK_TTL: Duration = Duration::from_secs(120);

/// Promote `patch` (a `git diff`) onto the repo at `live_root`, holding a lock on
/// the observed set for the apply window. `holder` identifies the promoting
/// agent/harness. Shells out to `git` (inherits operator auth; matches the rest
/// of the codebase's git use).
pub async fn promote(
    live_root: &Path,
    patch: &str,
    locks: &dyn RepoLocks,
    holder: &str,
) -> anyhow::Result<PromotionOutcome> {
    let tmp = tempfile::Builder::new().suffix(".patch").tempfile()?;
    std::fs::write(tmp.path(), patch)?;

    // The observed touched-set, discovered from the patch itself.
    let files = observed_files(live_root, tmp.path())?;
    if files.is_empty() {
        anyhow::bail!("PROMOTION_EMPTY_PATCH: the patch touches no files");
    }

    // Lock exactly those files for the apply window. A conflict means another
    // holder owns them — surface it rather than clobbering.
    if let Err(conflict) = locks.acquire(&files, holder, APPLY_LOCK_TTL).await {
        return Ok(PromotionOutcome::Locked(conflict));
    }

    // 3-way merge the patch onto the live tree.
    let out = Command::new("git")
        .arg("-C")
        .arg(live_root)
        .args(["apply", "--3way"])
        .arg(tmp.path())
        .output()
        .map_err(|e| anyhow::anyhow!("running `git apply`: {e} (is git on PATH?)"))?;

    locks.release(&files, holder).await;

    if out.status.success() {
        Ok(PromotionOutcome::Applied { files })
    } else {
        // `git apply --3way` falls back to a 3-way merge and, on overlap, writes
        // the file with conflict markers and exits non-zero — the surfaced case.
        Ok(PromotionOutcome::Conflict { files })
    }
}

/// What an untrusted agent runs inside its disposable, confined copy.
pub struct UntrustedAgentRun {
    /// argv to run confined (the exploratory agent driver).
    pub command: Vec<String>,
    pub env: Vec<(String, String)>,
    pub egress: Egress,
    pub limits: ResourceLimits,
}

/// The result of an untrusted agent run.
#[derive(Debug)]
pub enum UntrustedOutcome {
    /// The agent changed nothing in its copy — nothing to promote.
    NoChanges { sandbox: SandboxOutput },
    /// The agent produced a diff, which was promoted (see `promotion`).
    Promoted {
        promotion: PromotionOutcome,
        sandbox: SandboxOutput,
    },
}

/// ADR-0007 — the untrusted-exploration tier, end to end: materialize a
/// disposable copy of `source_repo`, run the agent **confined** inside it (via
/// `provider`, workspace = the copy), capture whatever it changed as a patch,
/// and **promote** that patch back onto the live tree (lock the observed set,
/// 3-way merge). The copy is discarded on return — its only durable output is
/// the promoted (reviewed-by-construction) patch. The agent never holds a lock
/// and never touches the live tree directly.
pub async fn run_untrusted_agent(
    source_repo: &Path,
    run: UntrustedAgentRun,
    provider: &dyn SandboxProvider,
    locks: &dyn RepoLocks,
    holder: &str,
) -> anyhow::Result<UntrustedOutcome> {
    let tmp = tempfile::tempdir()?;
    let copy = tmp.path().join("workspace");
    prepare_disposable_copy(source_repo, &copy)?;

    let spec = SandboxSpec {
        workspace: Some(copy.clone()),
        command: run.command,
        ro_binds: Vec::new(),
        env: run.env,
        egress: run.egress,
        env_allowlist: Vec::new(),
        limits: run.limits,
    };
    let sandbox = provider
        .run(&spec)
        .await
        .map_err(|e| anyhow::anyhow!("untrusted agent run failed: {e}"))?;

    let patch = capture_patch(&copy)?;
    if patch.trim().is_empty() {
        return Ok(UntrustedOutcome::NoChanges { sandbox });
    }

    let promotion = promote(source_repo, &patch, locks, holder).await?;
    Ok(UntrustedOutcome::Promoted { promotion, sandbox })
    // `tmp` (the disposable copy) is dropped + removed here.
}

/// The result of a trusted-agent run (ADR-0006 trusted tier).
#[derive(Debug)]
pub enum TrustedOutcome {
    /// The agent changed nothing in its copy — nothing to promote.
    NoChanges,
    /// The agent produced a diff, which was promoted (see `promotion`).
    Promoted { promotion: PromotionOutcome },
}

/// ADR-0006 **trusted tier** — run a trusted (in-process) agent against a
/// disposable copy of `source_repo`, then promote its diff. Unlike
/// [`run_untrusted_agent`] there is no sandbox confinement: the agent is
/// praxec's own governed session, so the only governance needed is the
/// lock-coordinated promotion. `edit` runs the agent against the copy path —
/// it edits files under that root; the resulting diff is captured and promoted
/// onto the live tree (lock the observed set, 3-way merge). The copy is
/// discarded on return — its only durable output is the promoted patch. The
/// agent never touches the live tree directly and never holds a lock during the
/// edit; coordination happens only at the brief apply window.
pub async fn run_trusted_agent<F, Fut>(
    source_repo: &Path,
    locks: &dyn RepoLocks,
    holder: &str,
    edit: F,
) -> anyhow::Result<TrustedOutcome>
where
    F: FnOnce(PathBuf) -> Fut,
    Fut: std::future::Future<Output = anyhow::Result<()>>,
{
    let tmp = tempfile::tempdir()?;
    let copy = tmp.path().join("workspace");
    prepare_disposable_copy(source_repo, &copy)?;

    edit(copy.clone()).await?;

    let patch = capture_patch(&copy)?;
    if patch.trim().is_empty() {
        return Ok(TrustedOutcome::NoChanges);
    }
    let promotion = promote(source_repo, &patch, locks, holder).await?;
    Ok(TrustedOutcome::Promoted { promotion })
    // `tmp` (the disposable copy) is dropped + removed here.
}

/// ADR-0006/0007 — materialize a **disposable working copy** of `source_repo`
/// into `dest`: a separate `git clone` (NOT a shared-`.git` worktree — FMECA §3),
/// the sandbox workspace an untrusted agent explores freely. The agent sees only
/// `dest`; discard it when done. `dest` must not already exist.
pub fn prepare_disposable_copy(source_repo: &Path, dest: &Path) -> anyhow::Result<()> {
    if let Some(parent) = dest.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let out = Command::new("git")
        .args(["clone", "-q"])
        .arg(source_repo)
        .arg(dest)
        .output()
        .map_err(|e| anyhow::anyhow!("running `git clone`: {e} (is git on PATH?)"))?;
    if !out.status.success() {
        anyhow::bail!(
            "DISPOSABLE_COPY_FAILED: cloning {} → {}: {}",
            source_repo.display(),
            dest.display(),
            String::from_utf8_lossy(&out.stderr).trim()
        );
    }
    Ok(())
}

/// Capture everything an agent changed in its disposable copy (incl. new files)
/// as a patch against the copy's `HEAD` — the candidate the promotion bridge
/// 3-way merges onto the live tree. Empty string = the agent changed nothing.
pub fn capture_patch(copy: &Path) -> anyhow::Result<String> {
    // Stage all changes (so new + deleted files are included), then diff the
    // index against HEAD without touching the commit graph.
    let add = Command::new("git")
        .arg("-C")
        .arg(copy)
        .args(["add", "-A"])
        .output()?;
    if !add.status.success() {
        anyhow::bail!(
            "CAPTURE_PATCH_FAILED (add): {}",
            String::from_utf8_lossy(&add.stderr).trim()
        );
    }
    let out = Command::new("git")
        .arg("-C")
        .arg(copy)
        .args(["diff", "--cached", "HEAD"])
        .output()?;
    if !out.status.success() {
        anyhow::bail!(
            "CAPTURE_PATCH_FAILED (diff): {}",
            String::from_utf8_lossy(&out.stderr).trim()
        );
    }
    Ok(String::from_utf8_lossy(&out.stdout).into_owned())
}

/// The files a patch touches, via `git apply --numstat` (parses the patch; does
/// not apply it). The path is the third whitespace-separated column.
fn observed_files(live_root: &Path, patch: &Path) -> anyhow::Result<Vec<PathBuf>> {
    let out = Command::new("git")
        .arg("-C")
        .arg(live_root)
        .args(["apply", "--numstat"])
        .arg(patch)
        .output()
        .map_err(|e| anyhow::anyhow!("running `git apply --numstat`: {e}"))?;
    if !out.status.success() {
        anyhow::bail!(
            "PROMOTION_PATCH_UNREADABLE: {}",
            String::from_utf8_lossy(&out.stderr).trim()
        );
    }
    let mut files = Vec::new();
    for line in String::from_utf8_lossy(&out.stdout).lines() {
        if let Some(path) = line.split_whitespace().nth(2) {
            files.push(PathBuf::from(path));
        }
    }
    Ok(files)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::repo_locks::RepoLockSpace;

    fn gca(cwd: &Path, args: &[&str]) {
        let mut full = vec!["-c", "user.email=t@t", "-c", "user.name=t"];
        full.extend_from_slice(args);
        assert!(
            Command::new("git")
                .arg("-C")
                .arg(cwd)
                .args(&full)
                .output()
                .unwrap()
                .status
                .success(),
            "git {args:?} in {} failed",
            cwd.display()
        );
    }

    fn setup_live(d: &Path) {
        Command::new("git")
            .args(["init", "-q", "-b", "main"])
            .arg(d)
            .output()
            .unwrap();
        for (f, c) in [("a.txt", "a0\n"), ("b.txt", "b0\n"), ("c.txt", "c0\n")] {
            std::fs::write(d.join(f), c).unwrap();
        }
        gca(d, &["add", "."]);
        gca(d, &["commit", "-qm", "C0"]);
    }

    /// A patch (git diff) that rewrites a.txt, generated from a clone at the
    /// live repo's current commit.
    fn patch_editing_a(live: &Path, new_a: &str) -> String {
        let wt = tempfile::tempdir().unwrap();
        let dest = wt.path().join("clone");
        Command::new("git")
            .args(["clone", "-q"])
            .arg(live)
            .arg(&dest)
            .output()
            .unwrap();
        std::fs::write(dest.join("a.txt"), new_a).unwrap();
        let out = Command::new("git")
            .arg("-C")
            .arg(&dest)
            .args(["diff"])
            .output()
            .unwrap();
        String::from_utf8_lossy(&out.stdout).into_owned()
    }

    #[tokio::test]
    async fn disjoint_patch_applies_clean_and_releases_the_lock() {
        let live = tempfile::tempdir().unwrap();
        setup_live(live.path());
        let patch = patch_editing_a(live.path(), "a0\nagent-added\n");
        // Live moves on a DISJOINT file.
        std::fs::write(live.path().join("c.txt"), "c0\nlive-change\n").unwrap();
        gca(live.path(), &["commit", "-aqm", "live edits c"]);

        let locks = RepoLockSpace::new();
        let outcome = promote(live.path(), &patch, &locks, "agent").await.unwrap();
        assert!(
            matches!(outcome, PromotionOutcome::Applied { .. }),
            "{outcome:?}"
        );
        assert!(
            std::fs::read_to_string(live.path().join("a.txt"))
                .unwrap()
                .contains("agent-added")
        );
        assert!(
            locks.held().await.is_empty(),
            "the apply-window lock is released"
        );
    }

    #[tokio::test]
    async fn overlapping_patch_conflicts_not_silently_merged() {
        let live = tempfile::tempdir().unwrap();
        setup_live(live.path());
        let patch = patch_editing_a(live.path(), "a-AGENT\n");
        // Live rewrites the SAME line differently.
        std::fs::write(live.path().join("a.txt"), "a-LIVE\n").unwrap();
        gca(live.path(), &["commit", "-aqm", "live edits a"]);

        let locks = RepoLockSpace::new();
        let outcome = promote(live.path(), &patch, &locks, "agent").await.unwrap();
        assert!(
            matches!(outcome, PromotionOutcome::Conflict { .. }),
            "{outcome:?}"
        );
        assert!(
            locks.held().await.is_empty(),
            "lock released even on conflict"
        );
    }

    /// A SandboxProvider that, when run, edits its workspace — standing in for
    /// the confined agent exploring the disposable copy (no bwrap needed here).
    struct EditingProvider {
        new_a: &'static str,
    }
    #[async_trait::async_trait]
    impl SandboxProvider for EditingProvider {
        fn preflight(&self) -> crate::sandbox::Preflight {
            crate::sandbox::Preflight {
                usable: true,
                detail: "editing".into(),
                install_hint: None,
            }
        }
        async fn run(&self, spec: &SandboxSpec) -> anyhow::Result<SandboxOutput> {
            let ws = spec.workspace.clone().expect("workspace");
            std::fs::write(ws.join("a.txt"), self.new_a).unwrap();
            Ok(SandboxOutput {
                code: Some(0),
                success: true,
                stdout: b"explored".to_vec(),
                stderr: vec![],
            })
        }
    }

    fn run_spec() -> UntrustedAgentRun {
        UntrustedAgentRun {
            command: vec!["true".into()],
            env: vec![],
            egress: Egress::DenyAll,
            limits: ResourceLimits::default(),
        }
    }

    #[tokio::test]
    async fn untrusted_run_explores_confined_then_promotes() {
        let live = tempfile::tempdir().unwrap();
        setup_live(live.path());
        let provider = EditingProvider {
            new_a: "a0\nagent-was-here\n",
        };
        let locks = RepoLockSpace::new();

        let outcome = run_untrusted_agent(live.path(), run_spec(), &provider, &locks, "agent")
            .await
            .unwrap();
        match outcome {
            UntrustedOutcome::Promoted {
                promotion: PromotionOutcome::Applied { .. },
                ..
            } => {
                assert!(
                    std::fs::read_to_string(live.path().join("a.txt"))
                        .unwrap()
                        .contains("agent-was-here")
                );
            }
            other => panic!("expected Applied promotion, got {other:?}"),
        }
        assert!(locks.held().await.is_empty(), "no lock left held");
    }

    #[tokio::test]
    async fn untrusted_run_with_no_edits_reports_no_changes() {
        // A provider that touches nothing → the agent changed nothing → NoChanges.
        struct NoopProvider;
        #[async_trait::async_trait]
        impl SandboxProvider for NoopProvider {
            fn preflight(&self) -> crate::sandbox::Preflight {
                crate::sandbox::Preflight {
                    usable: true,
                    detail: "noop".into(),
                    install_hint: None,
                }
            }
            async fn run(&self, _spec: &SandboxSpec) -> anyhow::Result<SandboxOutput> {
                Ok(SandboxOutput {
                    code: Some(0),
                    success: true,
                    stdout: vec![],
                    stderr: vec![],
                })
            }
        }
        let live = tempfile::tempdir().unwrap();
        setup_live(live.path());
        let outcome = run_untrusted_agent(
            live.path(),
            run_spec(),
            &NoopProvider,
            &RepoLockSpace::new(),
            "agent",
        )
        .await
        .unwrap();
        assert!(
            matches!(outcome, UntrustedOutcome::NoChanges { .. }),
            "{outcome:?}"
        );
    }

    #[tokio::test]
    async fn disposable_copy_to_promotion_end_to_end() {
        // The full untrusted-side pipeline: copy the repo, an agent edits freely
        // in the copy, capture its diff, promote it back onto the (moved) live
        // tree — disjoint, so it 3-way merges clean.
        let live = tempfile::tempdir().unwrap();
        setup_live(live.path());

        // Disposable copy (a separate clone — the agent's confined workspace).
        let tmp = tempfile::tempdir().unwrap();
        let copy = tmp.path().join("agent-workspace");
        prepare_disposable_copy(live.path(), &copy).unwrap();
        assert!(copy.join("a.txt").exists(), "the copy is a full clone");

        // The agent explores freely in the copy (here: edit a.txt + add a file).
        std::fs::write(copy.join("a.txt"), "a0\nagent-explored\n").unwrap();
        std::fs::write(copy.join("new.txt"), "created by the agent\n").unwrap();
        let patch = capture_patch(&copy).unwrap();
        assert!(patch.contains("agent-explored"));
        assert!(patch.contains("new.txt"), "new files are captured");

        // Meanwhile live moves on a disjoint file.
        std::fs::write(live.path().join("c.txt"), "c0\nlive\n").unwrap();
        gca(live.path(), &["commit", "-aqm", "live edits c"]);

        // Promote the agent's patch back.
        let locks = RepoLockSpace::new();
        let outcome = promote(live.path(), &patch, &locks, "agent").await.unwrap();
        assert!(
            matches!(outcome, PromotionOutcome::Applied { .. }),
            "{outcome:?}"
        );
        assert!(
            std::fs::read_to_string(live.path().join("a.txt"))
                .unwrap()
                .contains("agent-explored")
        );
        assert!(
            live.path().join("new.txt").exists(),
            "the agent's new file landed"
        );
    }

    #[tokio::test]
    async fn trusted_agent_edit_is_promoted_to_live() {
        // ADR-0006 trusted tier: a trusted (in-process) agent edits a disposable
        // copy via the injected `edit` step; its diff is promoted onto the live
        // tree under a lock — no sandbox confinement.
        let live = tempfile::tempdir().unwrap();
        setup_live(live.path());
        let locks = RepoLockSpace::new();

        let outcome = run_trusted_agent(live.path(), &locks, "rig-agent", |copy| async move {
            std::fs::write(copy.join("a.txt"), "a0\ntrusted-edit\n")?;
            std::fs::write(copy.join("new.txt"), "made by the trusted agent\n")?;
            Ok(())
        })
        .await
        .unwrap();

        match outcome {
            TrustedOutcome::Promoted {
                promotion: PromotionOutcome::Applied { .. },
            } => {
                assert!(
                    std::fs::read_to_string(live.path().join("a.txt"))
                        .unwrap()
                        .contains("trusted-edit")
                );
                assert!(live.path().join("new.txt").exists(), "new file landed");
            }
            other => panic!("expected Applied promotion, got {other:?}"),
        }
        assert!(locks.held().await.is_empty(), "apply-window lock released");
    }

    #[tokio::test]
    async fn trusted_agent_no_changes_promotes_nothing() {
        // The trusted agent touches nothing → NoChanges, no promotion.
        let live = tempfile::tempdir().unwrap();
        setup_live(live.path());
        let locks = RepoLockSpace::new();

        let outcome = run_trusted_agent(
            live.path(),
            &locks,
            "rig-agent",
            |_copy| async move { Ok(()) },
        )
        .await
        .unwrap();

        assert!(matches!(outcome, TrustedOutcome::NoChanges), "{outcome:?}");
        assert!(locks.held().await.is_empty(), "no lock acquired");
    }

    #[tokio::test]
    async fn observed_files_held_by_another_holder_return_locked() {
        let live = tempfile::tempdir().unwrap();
        setup_live(live.path());
        let patch = patch_editing_a(live.path(), "a0\nx\n");
        let locks = RepoLockSpace::new();
        locks
            .acquire(&[PathBuf::from("a.txt")], "other", Duration::from_secs(60))
            .await
            .unwrap();

        let outcome = promote(live.path(), &patch, &locks, "agent").await.unwrap();
        assert!(
            matches!(outcome, PromotionOutcome::Locked(_)),
            "{outcome:?}"
        );
    }
}
