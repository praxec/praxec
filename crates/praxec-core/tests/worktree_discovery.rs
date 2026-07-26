//! WS-B B3 — identity-first, worktree-churn-proof writable-repo resolution.
//!
//! A `worktrees_of:` repo is declared by a durable IDENTITY (`name`) plus a
//! stable `anchor` checkout to enumerate git worktrees of. At config-load the
//! live writable root is the worktree carrying a `praxec.repo.yaml` stub whose
//! `name` matches. The headline property: a pruned/switched worktree resolves
//! to nothing (a LEGAL boot state), it NEVER hard-fails boot the way a dead
//! `path:` entry does.
//!
//! These tests drive a REAL temporary git repo + worktrees through the public
//! config-resolution entrypoints. They are gated on `git` being available.

use std::path::Path;
use std::process::Command;

use serde_json::Value;

/// Run `git` in `dir`, asserting success. Returns stdout.
fn git(dir: &Path, args: &[&str]) -> String {
    let out = Command::new("git")
        .arg("-C")
        .arg(dir)
        // Deterministic identity + no reliance on the caller's global config.
        .args(["-c", "user.email=t@example.com", "-c", "user.name=t"])
        .args(args)
        .output()
        .expect("git spawns");
    assert!(
        out.status.success(),
        "git {args:?} failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8_lossy(&out.stdout).into_owned()
}

/// Skip the test (return true) when `git` is unavailable in the environment.
fn git_missing() -> bool {
    Command::new("git")
        .arg("--version")
        .output()
        .map(|o| !o.status.success())
        .unwrap_or(true)
}

/// Write a `praxec.repo.yaml` stub declaring `name` into `dir`.
fn write_stub(dir: &Path, name: &str) {
    std::fs::write(
        dir.join("praxec.repo.yaml"),
        format!("schema: praxec.repo/v1\nname: {name}\nnamespace: {name}\nversion: 0.0.0\n"),
    )
    .expect("write stub");
}

/// Create an anchor git repo at `<base>/anchor` with one commit, so worktrees
/// can be added. Returns the anchor path.
fn init_anchor(base: &Path) -> std::path::PathBuf {
    let anchor = base.join("anchor");
    std::fs::create_dir_all(&anchor).unwrap();
    git(&anchor, &["init", "-q"]);
    std::fs::write(anchor.join("seed.txt"), "seed").unwrap();
    git(&anchor, &["add", "."]);
    git(&anchor, &["commit", "-q", "-m", "seed"]);
    anchor
}

/// Write a gateway config referencing a single `worktrees_of:` repo, resolve it
/// (strict unless `resilient`), and return `(resolved_config, diagnostics)`.
fn resolve_config(
    base: &Path,
    anchor: &Path,
    name: &str,
    resilient: bool,
) -> anyhow::Result<(Value, Vec<praxec_core::config::Diagnostic>)> {
    let cfg_path = base.join("gateway.yaml");
    std::fs::write(
        &cfg_path,
        format!(
            "version: \"1.0.0\"\nworkflows: {{}}\nrepos:\n  - worktrees_of: {}\n    name: {name}\n    writable: true\n",
            anchor.display()
        ),
    )
    .unwrap();
    if resilient {
        praxec_core::config::load_resolved_with_repos_resilient(&cfg_path)
    } else {
        praxec_core::config::load_resolved_with_repos(&cfg_path)
    }
}

/// The stamped writable roots (canonicalized) from a resolved config.
fn writable_roots(config: &Value) -> Vec<std::path::PathBuf> {
    config
        .pointer("/praxec/_writableRepos")
        .and_then(Value::as_array)
        .map(|arr| {
            arr.iter()
                .filter_map(|e| e.pointer("/root").and_then(Value::as_str))
                .map(|r| std::fs::canonicalize(r).unwrap_or_else(|_| std::path::PathBuf::from(r)))
                .collect()
        })
        .unwrap_or_default()
}

/// 1. A `worktrees_of:` + `name` entry discovers the worktree carrying the
///    matching stub → its path becomes the writable root, keyed by name.
#[test]
fn discovers_the_worktree_carrying_the_matching_stub() {
    if git_missing() {
        eprintln!("SKIP: git unavailable");
        return;
    }
    let base = tempfile::TempDir::new().unwrap();
    let anchor = init_anchor(base.path());
    // Add a worktree and give it the identity stub (the anchor itself has none).
    let wt = base.path().join("wt-live");
    git(&anchor, &["worktree", "add", "-q", wt.to_str().unwrap(), "-b", "live"]);
    write_stub(&wt, "autopilot-qa-target");

    let (config, _diags) =
        resolve_config(base.path(), &anchor, "autopilot-qa-target", false).expect("load succeeds");

    let roots = writable_roots(&config);
    assert_eq!(roots.len(), 1, "exactly one writable root, got {roots:?}");
    assert_eq!(roots[0], std::fs::canonicalize(&wt).unwrap());
    // The name is stamped so the gateway can build the name→root index.
    let stamped_name = config
        .pointer("/praxec/_writableRepos/0/name")
        .and_then(Value::as_str);
    assert_eq!(stamped_name, Some("autopilot-qa-target"));
}

/// 2. Zero matching worktrees → config load SUCCEEDS (boot NOT failed), no root
///    stamped, and a REPO_IDENTITY_UNRESOLVED diagnostic is recorded.
#[test]
fn zero_matches_is_a_legal_boot_state_with_a_diagnostic() {
    if git_missing() {
        eprintln!("SKIP: git unavailable");
        return;
    }
    let base = tempfile::TempDir::new().unwrap();
    let anchor = init_anchor(base.path());
    // A worktree WITHOUT the matching stub → no candidate.
    let wt = base.path().join("wt-other");
    git(&anchor, &["worktree", "add", "-q", wt.to_str().unwrap(), "-b", "other"]);
    write_stub(&wt, "some-other-repo");

    let (config, diags) =
        resolve_config(base.path(), &anchor, "autopilot-qa-target", false).expect("load succeeds");

    assert!(
        writable_roots(&config).is_empty(),
        "no writable root must be stamped for an unresolved identity"
    );
    assert!(
        diags.iter().any(|d| d.code == "REPO_IDENTITY_UNRESOLVED"),
        "an unresolved identity must be recorded, got: {:?}",
        diags.iter().map(|d| &d.code).collect::<Vec<_>>()
    );
}

/// 3. Two worktrees with the same stub name → REPO_IDENTITY_AMBIGUOUS: a hard
///    error in Strict, skipped-with-warn (load succeeds) in Resilient.
#[test]
fn two_matches_is_ambiguous_hard_in_strict_skipped_in_resilient() {
    if git_missing() {
        eprintln!("SKIP: git unavailable");
        return;
    }
    let base = tempfile::TempDir::new().unwrap();
    let anchor = init_anchor(base.path());
    for (dir, branch) in [("wt-a", "a"), ("wt-b", "b")] {
        let wt = base.path().join(dir);
        git(&anchor, &["worktree", "add", "-q", wt.to_str().unwrap(), "-b", branch]);
        write_stub(&wt, "dup-identity");
    }

    // Strict → hard error naming the ambiguity.
    let strict = resolve_config(base.path(), &anchor, "dup-identity", false);
    let err = strict.expect_err("ambiguity is a hard error in strict mode");
    let msg = format!("{err:#}");
    assert!(
        msg.contains("REPO_IDENTITY_AMBIGUOUS"),
        "expected ambiguity error, got: {msg}"
    );

    // Resilient → load succeeds, no root, diagnostic recorded (never auto-picked).
    let (config, diags) =
        resolve_config(base.path(), &anchor, "dup-identity", true).expect("resilient load succeeds");
    assert!(
        writable_roots(&config).is_empty(),
        "an ambiguous identity must never auto-pick a root"
    );
    assert!(
        diags.iter().any(|d| d.code == "REPO_IDENTITY_AMBIGUOUS"),
        "resilient mode must record the ambiguity"
    );
}

/// 5. A pruned worktree (created, then `git worktree remove`d) → config load
///    still SUCCEEDS (the headline win). Contrast a dead `path:` entry, which
///    still hard-fails boot.
#[test]
fn a_pruned_worktree_does_not_fail_boot() {
    if git_missing() {
        eprintln!("SKIP: git unavailable");
        return;
    }
    let base = tempfile::TempDir::new().unwrap();
    let anchor = init_anchor(base.path());
    let wt = base.path().join("wt-ephemeral");
    git(&anchor, &["worktree", "add", "-q", wt.to_str().unwrap(), "-b", "eph"]);
    write_stub(&wt, "autopilot-qa-target");

    // Baseline: it resolves while the worktree lives.
    let (before, _) =
        resolve_config(base.path(), &anchor, "autopilot-qa-target", false).expect("resolves live");
    assert_eq!(writable_roots(&before).len(), 1);

    // Prune the worktree — the declared literal path is now GONE.
    git(&anchor, &["worktree", "remove", "--force", wt.to_str().unwrap()]);
    assert!(!wt.exists(), "worktree dir must be gone after remove");

    // The headline win: boot (config load) STILL SUCCEEDS, with no root stamped.
    let (after, diags) = resolve_config(base.path(), &anchor, "autopilot-qa-target", false)
        .expect("a pruned worktree must NOT fail boot");
    assert!(
        writable_roots(&after).is_empty(),
        "the pruned identity stamps no root"
    );
    assert!(
        diags.iter().any(|d| d.code == "REPO_IDENTITY_UNRESOLVED"),
        "the now-unresolved identity is recorded"
    );
}
