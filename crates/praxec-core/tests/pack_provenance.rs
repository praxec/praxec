//! pack-provenance-recording (P1/P2 substrate) — `merge_declared_repos` stamps
//! `/praxec/_packProvenance` with `{ namespace, source, sha, ref, dirty }` for
//! every namespace-bearing loaded pack. This is the durable data BOTH the
//! gateway's `pack.provenance` audit event (P1) and `discovery::home()`'s
//! `loaded_packs` (P2) read — see `crates/praxec/src/gateway.rs` and
//! `crates/praxec-core/src/discovery/discovery_indexer.rs`.
//!
//! Fixtures mirror `pack_staleness_warning.rs`: throwaway git repos under a
//! fresh `tempfile::TempDir`, never the checked-in `tests/fixtures/repos/*`
//! (which live inside this workspace's own git repo and would spuriously
//! inherit ITS branch/dirty state — see `repo_git::git_currency`'s
//! `subdir_of_a_larger_repo_is_none` unit test).
//!
//! Provenance RECORDS, never CONSTRAINS: every fixture here — a feature
//! branch, a dirty tree, even a non-git pack — must still LOAD successfully;
//! only the recorded values differ.

use std::path::{Path, PathBuf};
use std::process::Command;

use praxec_core::config::load_resolved_with_repos;
use serde_json::Value;
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

fn head_sha(dir: &Path) -> String {
    let out = Command::new("git")
        .arg("-C")
        .arg(dir)
        .args(["rev-parse", "HEAD"])
        .output()
        .unwrap();
    String::from_utf8_lossy(&out.stdout).trim().to_string()
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

/// Find this namespace's provenance record in the stamped array.
fn find_pack<'a>(config: &'a Value, namespace: &str) -> &'a Value {
    config
        .pointer("/praxec/_packProvenance")
        .and_then(Value::as_array)
        .unwrap_or_else(|| panic!("expected /praxec/_packProvenance to be stamped: {config:?}"))
        .iter()
        .find(|p| p.get("namespace").and_then(Value::as_str) == Some(namespace))
        .unwrap_or_else(|| panic!("no provenance record for namespace '{namespace}'"))
}

#[test]
fn clean_git_pack_is_recorded_with_namespace_sha_ref_and_dirty_false() {
    let td = TempDir::new().unwrap();
    let repo = td.path().join("pack");
    init_git_pack(&repo, "dev", "prov-clean");
    let expected_sha = head_sha(&repo);

    let path = host_with_one_repo(&td, &repo);
    let (config, _diagnostics) = load_resolved_with_repos(&path).expect("clean pack loads");

    let record = find_pack(&config, "prov-clean");
    assert_eq!(
        record.get("source").and_then(Value::as_str),
        Some(repo.display().to_string().as_str())
    );
    assert_eq!(
        record.get("sha").and_then(Value::as_str),
        Some(expected_sha.as_str())
    );
    assert_eq!(record.get("ref").and_then(Value::as_str), Some("dev"));
    assert_eq!(record.get("dirty").and_then(Value::as_bool), Some(false));
}

#[test]
fn drifted_dirty_pack_is_still_recorded_and_still_loads() {
    // Provenance records, never constrains: a feature-branch, dirty pack
    // triggers the existing PACK_BRANCH_DRIFT/PACK_DIRTY_TREE WARN
    // diagnostics AND still loads AND is still recorded — never a load error.
    let td = TempDir::new().unwrap();
    let repo = td.path().join("pack");
    init_git_pack(&repo, "dev", "prov-drift");
    git(&["checkout", "-b", "feat/wip"], &repo);
    std::fs::write(repo.join("NOTES.md"), "uncommitted\n").unwrap();
    let expected_sha = head_sha(&repo);

    let path = host_with_one_repo(&td, &repo);
    let (config, diagnostics) =
        load_resolved_with_repos(&path).expect("a drifted+dirty pack still LOADS");

    assert!(diagnostics.iter().any(|d| d.code == "PACK_BRANCH_DRIFT"));
    assert!(diagnostics.iter().any(|d| d.code == "PACK_DIRTY_TREE"));

    let record = find_pack(&config, "prov-drift");
    assert_eq!(
        record.get("sha").and_then(Value::as_str),
        Some(expected_sha.as_str())
    );
    assert_eq!(record.get("ref").and_then(Value::as_str), Some("feat/wip"));
    assert_eq!(record.get("dirty").and_then(Value::as_bool), Some(true));
}

#[test]
fn non_git_path_pack_is_recorded_with_no_sha_ref_or_dirty_and_no_error() {
    let td = TempDir::new().unwrap();
    let repo = td.path().join("plain-pack");
    std::fs::create_dir_all(&repo).unwrap();
    std::fs::write(
        repo.join("praxec.repo.yaml"),
        "schema: praxec.repo/v1\nname: plain-pack\nnamespace: prov-plain\nversion: 0.0.1\n",
    )
    .unwrap();

    let path = host_with_one_repo(&td, &repo);
    let (config, diagnostics) = load_resolved_with_repos(&path).expect("plain-dir pack loads");
    assert!(
        diagnostics.is_empty(),
        "no warning/error for a plain-dir pack"
    );

    let record = find_pack(&config, "prov-plain");
    assert!(
        record.get("sha").is_none(),
        "non-git pack must record absence, not an error: {record:?}"
    );
    assert!(record.get("ref").is_none());
    assert!(record.get("dirty").is_none());
}

#[test]
fn remote_uri_pack_is_recorded_with_the_declared_ref_and_resolved_sha() {
    let td = TempDir::new().unwrap();
    let origin = td.path().join("origin");
    init_git_pack(&origin, "main", "prov-remote");
    let origin_uri = format!("file://{}", origin.display());
    let expected_sha = head_sha(&origin);

    let path = write_host(
        &td,
        &format!("version: \"1.0.0\"\nrepos:\n  - uri: \"{origin_uri}\"\n    ref: main\n"),
    );
    let (config, _diagnostics) =
        load_resolved_with_repos(&path).expect("a remote uri: pack imports and loads");

    let record = find_pack(&config, "prov-remote");
    assert_eq!(
        record.get("source").and_then(Value::as_str),
        Some(origin_uri.as_str())
    );
    assert_eq!(
        record.get("sha").and_then(Value::as_str),
        Some(expected_sha.as_str())
    );
    assert_eq!(
        record.get("ref").and_then(Value::as_str),
        Some("main"),
        "the declared ref, not the cache dir's own artifact branch name"
    );
    assert_eq!(record.get("dirty").and_then(Value::as_bool), Some(false));
}

#[test]
fn bare_writable_run_target_contributes_no_provenance_record() {
    // FB-2: `definitions: false` ships no `praxec.repo.yaml` — no namespace,
    // so it's out of scope for "which workflow version drove this run".
    let td = TempDir::new().unwrap();
    let repo = td.path().join("bare-writable");
    std::fs::create_dir_all(&repo).unwrap();
    Command::new("git")
        .arg("init")
        .arg("-b")
        .arg("dev")
        .arg(&repo)
        .output()
        .unwrap();

    let path = write_host(
        &td,
        &format!(
            "version: \"1.0.0\"\nrepos:\n  - path: \"{}\"\n    writable: true\n    definitions: false\n",
            repo.display()
        ),
    );
    let (config, _diagnostics) =
        load_resolved_with_repos(&path).expect("a bare writable run target loads");

    let packs = config.pointer("/praxec/_packProvenance");
    assert!(
        packs.is_none(),
        "a namespace-less bare-writable target must not appear in provenance: {packs:?}"
    );
}

#[test]
fn multiple_packs_are_all_recorded_independently() {
    let td = TempDir::new().unwrap();
    let base = td.path().join("base");
    let overlay = td.path().join("overlay");
    init_git_pack(&base, "dev", "prov-base");
    init_git_pack(&overlay, "dev", "prov-overlay");

    let path = write_host(
        &td,
        &format!(
            "version: \"1.0.0\"\nrepos:\n  - path: \"{}\"\n  - path: \"{}\"\n",
            base.display(),
            overlay.display()
        ),
    );
    let (config, _diagnostics) = load_resolved_with_repos(&path).expect("composed packs load");

    let packs = config
        .pointer("/praxec/_packProvenance")
        .and_then(Value::as_array)
        .expect("stamped");
    assert_eq!(packs.len(), 2);
    find_pack(&config, "prov-base");
    find_pack(&config, "prov-overlay");
}
