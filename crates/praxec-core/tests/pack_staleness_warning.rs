//! pack-staleness-warning — non-blocking currency WARNING for a git-backed
//! `path:` pack, surfaced through `load_resolved_with_repos`'s soft
//! diagnostics (the exact channel `praxec check`/`doctor` already prints
//! from — see `crates/praxec/src/gateway.rs`'s `check()`).
//!
//! Fixtures are throwaway git repos built under a fresh `tempfile::TempDir`
//! for each test (NOT the checked-in `tests/fixtures/repos/*` used by
//! `multi_repo_loading.rs` — those live *inside* this workspace's own git
//! repo, and a git-currency check must never accidentally inherit an
//! enclosing repo's branch/dirty state; see `repo_git::git_currency`'s
//! `subdir_of_a_larger_repo_is_none` unit test for that guard).

use std::path::{Path, PathBuf};
use std::process::Command;

use praxec_core::config::load_resolved_with_repos;
use tempfile::TempDir;

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

/// A minimal git-backed pack: `git init -b <branch>`, a valid
/// `praxec.repo.yaml` under a unique `namespace`, committed.
fn init_git_pack(dir: &Path, branch: &str, namespace: &str) {
    std::fs::create_dir_all(dir).unwrap();
    Command::new("git")
        .arg("init")
        .arg("-b")
        .arg(branch)
        .arg(dir)
        .output()
        .unwrap();
    std::fs::write(
        dir.join("praxec.repo.yaml"),
        format!(
            "schema: praxec.repo/v1\nname: {namespace}-pack\nnamespace: {namespace}\nversion: 0.0.1\n"
        ),
    )
    .unwrap();
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

fn write_host(td: &TempDir, body: &str) -> PathBuf {
    let p = td.path().join("praxec.yaml");
    std::fs::write(&p, body).unwrap();
    p
}

fn host_with_one_repo(td: &TempDir, repo: &Path) -> PathBuf {
    write_host(
        td,
        &format!(
            "version: \"1.0.0\"\nrepos:\n  - path: \"{}\"\n",
            repo.display()
        ),
    )
}

#[test]
fn clean_pack_on_dev_has_no_currency_warning() {
    let td = TempDir::new().unwrap();
    let repo = td.path().join("pack");
    init_git_pack(&repo, "dev", "clean");

    let path = host_with_one_repo(&td, &repo);
    let (_config, diagnostics) = load_resolved_with_repos(&path).expect("clean pack loads");
    assert!(
        diagnostics.is_empty(),
        "a clean on-dev pack must not warn: {diagnostics:?}"
    );
}

#[test]
fn feature_branch_pack_warns_branch_drift_naming_the_branch() {
    let td = TempDir::new().unwrap();
    let repo = td.path().join("pack");
    init_git_pack(&repo, "dev", "drifted");
    git(&["checkout", "-b", "feat/react-review"], &repo);

    let path = host_with_one_repo(&td, &repo);
    let (_config, diagnostics) =
        load_resolved_with_repos(&path).expect("a drifted pack still LOADS (warning, not error)");
    let drift = diagnostics
        .iter()
        .find(|d| d.code == "PACK_BRANCH_DRIFT")
        .unwrap_or_else(|| panic!("expected a PACK_BRANCH_DRIFT warning: {diagnostics:?}"));
    assert!(
        drift.message.contains("feat/react-review"),
        "warning must name the actual branch: {}",
        drift.message
    );
    assert!(
        drift.message.contains("dev"),
        "warning must name the default branch: {}",
        drift.message
    );
}

#[test]
fn detached_head_pack_warns_branch_drift() {
    let td = TempDir::new().unwrap();
    let repo = td.path().join("pack");
    init_git_pack(&repo, "dev", "detached");
    let sha_out = Command::new("git")
        .arg("-C")
        .arg(&repo)
        .args(["rev-parse", "HEAD"])
        .output()
        .unwrap();
    let sha = String::from_utf8_lossy(&sha_out.stdout).trim().to_string();
    git(&["checkout", &sha], &repo);

    let path = host_with_one_repo(&td, &repo);
    let (_config, diagnostics) =
        load_resolved_with_repos(&path).expect("a detached-HEAD pack still loads");
    assert!(
        diagnostics.iter().any(|d| d.code == "PACK_BRANCH_DRIFT"),
        "{diagnostics:?}"
    );
}

#[test]
fn dirty_pack_warns_dirty_tree() {
    let td = TempDir::new().unwrap();
    let repo = td.path().join("pack");
    init_git_pack(&repo, "dev", "dirty");
    // An untracked file is enough to make `git status --porcelain`
    // non-empty without disturbing the (deny-unknown-fields) manifest.
    std::fs::write(repo.join("NOTES.md"), "uncommitted local note\n").unwrap();

    let path = host_with_one_repo(&td, &repo);
    let (_config, diagnostics) = load_resolved_with_repos(&path).expect("dirty pack still loads");
    assert!(
        diagnostics.iter().any(|d| d.code == "PACK_DIRTY_TREE"),
        "{diagnostics:?}"
    );
}

#[test]
fn behind_upstream_pack_warns_with_commit_count() {
    let td = TempDir::new().unwrap();
    let origin = td.path().join("origin");
    init_git_pack(&origin, "dev", "behind");
    let origin_uri = format!("file://{}", origin.display());

    let work = td.path().join("work");
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

    // Origin advances by one commit; the clone learns of it via a LOCAL
    // fetch (arrange step only — production `git_currency` never fetches).
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
    git(&["fetch", "origin", "dev"], &work);

    let path = host_with_one_repo(&td, &work);
    let (_config, diagnostics) = load_resolved_with_repos(&path).expect("behind pack still loads");
    let behind = diagnostics
        .iter()
        .find(|d| d.code == "PACK_BEHIND_UPSTREAM")
        .unwrap_or_else(|| panic!("expected a PACK_BEHIND_UPSTREAM warning: {diagnostics:?}"));
    assert!(
        behind.message.contains("1 commit"),
        "warning must name the commit count: {}",
        behind.message
    );
    // On its default branch and clean — no other warning should fire.
    assert!(!diagnostics.iter().any(|d| d.code == "PACK_BRANCH_DRIFT"));
    assert!(!diagnostics.iter().any(|d| d.code == "PACK_DIRTY_TREE"));
}

#[test]
fn composed_packs_on_different_branches_warn_mismatch() {
    let td = TempDir::new().unwrap();
    let base = td.path().join("base");
    let overlay = td.path().join("overlay");
    init_git_pack(&base, "dev", "base");
    init_git_pack(&overlay, "dev", "overlay");
    git(&["checkout", "-b", "feat/overlay-wip"], &overlay);

    let path = write_host(
        &td,
        &format!(
            "version: \"1.0.0\"\nrepos:\n  - path: \"{}\"\n  - path: \"{}\"\n",
            base.display(),
            overlay.display()
        ),
    );
    let (_config, diagnostics) =
        load_resolved_with_repos(&path).expect("mismatched composed packs still load");
    assert!(
        diagnostics
            .iter()
            .any(|d| d.code == "PACK_COMPOSITION_BRANCH_MISMATCH"),
        "{diagnostics:?}"
    );
    // The overlay itself also independently drifted off dev — both classes
    // of warning can legitimately fire together.
    assert!(diagnostics.iter().any(|d| d.code == "PACK_BRANCH_DRIFT"));
}

#[test]
fn composed_packs_on_the_same_non_default_branch_have_no_mismatch_warning() {
    let td = TempDir::new().unwrap();
    let base = td.path().join("base");
    let overlay = td.path().join("overlay");
    init_git_pack(&base, "dev", "base2");
    init_git_pack(&overlay, "dev", "overlay2");
    git(&["checkout", "-b", "feat/shared-wip"], &base);
    git(&["checkout", "-b", "feat/shared-wip"], &overlay);

    let path = write_host(
        &td,
        &format!(
            "version: \"1.0.0\"\nrepos:\n  - path: \"{}\"\n  - path: \"{}\"\n",
            base.display(),
            overlay.display()
        ),
    );
    let (_config, diagnostics) =
        load_resolved_with_repos(&path).expect("consistently-branched composed packs load");
    assert!(
        !diagnostics
            .iter()
            .any(|d| d.code == "PACK_COMPOSITION_BRANCH_MISMATCH"),
        "both packs agree on branch — no cross-pack mismatch expected: {diagnostics:?}"
    );
}

#[test]
fn non_git_path_pack_has_no_warning_and_no_error() {
    let td = TempDir::new().unwrap();
    let repo = td.path().join("plain-pack");
    std::fs::create_dir_all(&repo).unwrap();
    std::fs::write(
        repo.join("praxec.repo.yaml"),
        "schema: praxec.repo/v1\nname: plain-pack\nnamespace: plain\nversion: 0.0.1\n",
    )
    .unwrap();

    let path = host_with_one_repo(&td, &repo);
    let (_config, diagnostics) = load_resolved_with_repos(&path).expect("plain-dir pack loads");
    assert!(
        diagnostics.is_empty(),
        "a non-git path: pack must not warn or error: {diagnostics:?}"
    );
}
