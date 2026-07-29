//! pack-staleness-warning — end-to-end through the real `praxec` binary:
//! `praxec check` (and `praxec doctor`) on a config whose `path:` pack is on
//! a feature branch must (a) print the branch-drift warning naming the
//! branch, and (b) still exit success — staleness is a WARNING, never a load
//! error (see `crates/praxec/src/gateway.rs`'s `check()`/`doctor()`, and the
//! underlying `git_currency_diagnostics` in
//! `crates/praxec-core/src/config.rs`).
//!
//! `doctor()` used to load via `load_config()`, which DISCARDS the soft
//! diagnostics `check()` prints from — so a git-currency warning was visible
//! from `check` but silently swallowed by `doctor`. `doctor()` now loads via
//! the diagnostics-returning resilient loader directly and prints them
//! through the same shared `print_soft_diagnostics` helper `check()` uses;
//! the tests below are the regression proof.

use std::path::{Path, PathBuf};
use std::process::{Command, Output};

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

fn init_git_pack(dir: &Path, branch: &str) {
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
        "schema: praxec.repo/v1\nname: cli-pack\nnamespace: clipack\nversion: 0.0.1\n",
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

fn run_check(config: &Path) -> Output {
    let bin = env!("CARGO_BIN_EXE_praxec");
    Command::new(bin)
        .arg("check")
        .arg("--config")
        .arg(config)
        .output()
        .expect("run praxec check")
}

fn run_doctor(config: &Path) -> Output {
    let bin = env!("CARGO_BIN_EXE_praxec");
    Command::new(bin)
        .arg("doctor")
        .arg("--config")
        .arg(config)
        .output()
        .expect("run praxec doctor")
}

/// `gateway.allow_ephemeral: true` keeps `doctor`'s durability gate out of
/// the way — these tests are specifically about the pack-staleness signal,
/// not storage durability (a separate, already-covered concern).
fn write_config_with_one_repo(td: &tempfile::TempDir, repo: &Path) -> PathBuf {
    let config_path = td.path().join("gateway.yaml");
    std::fs::write(
        &config_path,
        format!(
            "version: \"1.0.0\"\ngateway:\n  allow_ephemeral: true\nrepos:\n  - path: \"{}\"\n",
            repo.display()
        ),
    )
    .unwrap();
    config_path
}

#[test]
fn check_warns_on_feature_branch_pack_but_still_exits_success() {
    let td = tempfile::tempdir().unwrap();
    let repo = td.path().join("pack");
    init_git_pack(&repo, "dev");
    git(
        &[
            "checkout",
            "-b",
            "feat/react-review-external-sync-discriminator",
        ],
        &repo,
    );

    let config_path = td.path().join("gateway.yaml");
    std::fs::write(
        &config_path,
        format!(
            "version: \"1.0.0\"\nrepos:\n  - path: \"{}\"\n",
            repo.display()
        ),
    )
    .unwrap();

    let out = run_check(&config_path);
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(
        out.status.success(),
        "check must still exit success with a staleness warning present:\nstdout:\n{stdout}\nstderr:\n{}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(
        stdout.contains("PACK_BRANCH_DRIFT"),
        "expected the branch-drift warning in check's output:\n{stdout}"
    );
    assert!(
        stdout.contains("feat/react-review-external-sync-discriminator"),
        "expected the actual branch name in check's output:\n{stdout}"
    );
}

#[test]
fn check_on_clean_dev_pack_has_no_staleness_warning() {
    let td = tempfile::tempdir().unwrap();
    let repo = td.path().join("pack");
    init_git_pack(&repo, "dev");

    let config_path = td.path().join("gateway.yaml");
    std::fs::write(
        &config_path,
        format!(
            "version: \"1.0.0\"\nrepos:\n  - path: \"{}\"\n",
            repo.display()
        ),
    )
    .unwrap();

    let out = run_check(&config_path);
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(out.status.success(), "stdout:\n{stdout}");
    assert!(
        !stdout.contains("PACK_BRANCH_DRIFT")
            && !stdout.contains("PACK_DIRTY_TREE")
            && !stdout.contains("PACK_BEHIND_UPSTREAM"),
        "a clean on-dev pack must not print any staleness warning:\n{stdout}"
    );
}

#[test]
fn doctor_warns_on_feature_branch_pack_but_still_exits_success() {
    let td = tempfile::tempdir().unwrap();
    let repo = td.path().join("pack");
    init_git_pack(&repo, "dev");
    git(
        &[
            "checkout",
            "-b",
            "feat/react-review-external-sync-discriminator",
        ],
        &repo,
    );

    let config_path = write_config_with_one_repo(&td, &repo);

    let out = run_doctor(&config_path);
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(
        out.status.success(),
        "doctor must still exit success with a staleness warning present:\nstdout:\n{stdout}\nstderr:\n{}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(
        stdout.contains("PACK_BRANCH_DRIFT"),
        "doctor must print the branch-drift warning too — it must never discard the soft \
         diagnostics `check` surfaces:\n{stdout}"
    );
    assert!(
        stdout.contains("feat/react-review-external-sync-discriminator"),
        "expected the actual branch name in doctor's output:\n{stdout}"
    );
}

#[test]
fn doctor_on_clean_dev_pack_has_no_staleness_warning() {
    let td = tempfile::tempdir().unwrap();
    let repo = td.path().join("pack");
    init_git_pack(&repo, "dev");

    let config_path = write_config_with_one_repo(&td, &repo);

    let out = run_doctor(&config_path);
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(out.status.success(), "stdout:\n{stdout}");
    assert!(
        !stdout.contains("PACK_BRANCH_DRIFT")
            && !stdout.contains("PACK_DIRTY_TREE")
            && !stdout.contains("PACK_BEHIND_UPSTREAM"),
        "a clean on-dev pack must not print any staleness warning:\n{stdout}"
    );
}
