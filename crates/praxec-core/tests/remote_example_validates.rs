//! Hermetic offline validation of `examples/remote-packs/gateway.yaml` —
//! the one shipped example whose `repos:` entries source packs via a real
//! network `uri:`. A prior CI change SKIPPED validating this file entirely
//! (it can't be resolved offline in CI as-shipped). That was wrong:
//! skipping means the config's OTHER potential errors (bad workflow refs,
//! schema drift, broken guards) go unvalidated forever. This test proves
//! the FULL example resolves cleanly by mocking only the git *transport* —
//! never the example's content.
//!
//! It builds local bare git repos from the minimal mock pack fixtures
//! shipped alongside the example
//! (`examples/remote-packs/_mock/{cognitive-architectures,praxec-meta}/`),
//! then loads a byte-for-byte copy of the real example with ONLY its two
//! `github.com` `uri:` hosts rewritten to `file://` those bare repos (same
//! `ref:` pins — `main` / `v1.0.0` — untouched), through the exact
//! [`config::load_resolved_with_repos`] path `praxec check` itself uses.
//! Zero network access is possible: every git operation targets a
//! `file://` path on local disk.
//!
//! This does NOT mutate global git config (unlike the CI-side mock in
//! `scripts/mock-remote-packs-git.sh`, which redirects the real
//! `github.com` URLs via `url.insteadOf` so the shipped example validates
//! completely unmodified in CI) — a per-process global-config edit would
//! race other tests in this binary. Rewriting the `uri:` host in a private
//! temp copy proves the same end-to-end resolution hermetically and
//! parallel-test-safely.
//!
//! `repo_git::clone_url` strips only the `git+` scheme prefix off a `uri:`
//! before shelling out to git. That EXACT stripped string is what the CI
//! mock's `insteadOf` redirect must match — locked in below so a future
//! change to that stripping logic trips this test, not just a silent CI
//! mismatch.

use praxec_core::{config, repo_git};
use std::path::{Path, PathBuf};
use std::process::Command;

fn workspace_root() -> PathBuf {
    // CARGO_MANIFEST_DIR is crates/praxec-core; walk up two parents to the
    // workspace root.
    let mut p = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    p.pop();
    p.pop();
    p
}

fn mock_pack_dir(name: &str) -> PathBuf {
    workspace_root()
        .join("examples/remote-packs/_mock")
        .join(name)
}

fn git(args: &[&str], cwd: &Path) {
    let out = Command::new("git")
        .arg("-C")
        .arg(cwd)
        .args(args)
        .output()
        .unwrap_or_else(|e| panic!("spawning `git {args:?}` in {}: {e}", cwd.display()));
    assert!(
        out.status.success(),
        "`git {args:?}` in {} failed: {}",
        cwd.display(),
        String::from_utf8_lossy(&out.stderr)
    );
}

/// Recursively copy `src`'s contents into `dst` (both directories; `dst`
/// must already exist). No external crate — the fixture trees here are
/// tiny (a manifest + one flow file each).
fn copy_dir_recursive(src: &Path, dst: &Path) {
    for entry in std::fs::read_dir(src).unwrap() {
        let entry = entry.unwrap();
        let from = entry.path();
        let to = dst.join(entry.file_name());
        if entry.file_type().unwrap().is_dir() {
            std::fs::create_dir_all(&to).unwrap();
            copy_dir_recursive(&from, &to);
        } else {
            std::fs::copy(&from, &to).unwrap();
        }
    }
}

/// Build a bare git repo at `bare_dir` seeded with a single commit
/// containing `fixture_dir`'s contents, optionally tagging that commit.
fn build_bare_repo(fixture_dir: &Path, bare_dir: &Path, tag: Option<&str>) {
    let seed = tempfile::tempdir().unwrap();
    copy_dir_recursive(fixture_dir, seed.path());
    git(&["init", "--quiet", "-b", "main"], seed.path());
    git(&["add", "-A"], seed.path());
    git(
        &[
            "-c",
            "user.email=t@t",
            "-c",
            "user.name=t",
            "commit",
            "--quiet",
            "-m",
            "mock pack seed",
        ],
        seed.path(),
    );
    if let Some(t) = tag {
        git(&["tag", t], seed.path());
    }
    let out = Command::new("git")
        .args([
            "clone",
            "--quiet",
            "--bare",
            &seed.path().display().to_string(),
            &bare_dir.display().to_string(),
        ])
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "bare-cloning mock pack seed failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );
}

/// The two real `uri:` hosts the shipped example pins, and the exact URL
/// string `repo_git::clone_url` derives from each (the `git+` prefix
/// stripped, nothing else) — this is what a CI-side `insteadOf` mock MUST
/// match.
#[test]
fn clone_url_strips_git_prefix_for_the_remote_packs_example_uris() {
    assert_eq!(
        repo_git::clone_url("git+https://github.com/praxec/cognitive-architectures"),
        "https://github.com/praxec/cognitive-architectures"
    );
    assert_eq!(
        repo_git::clone_url("git+https://github.com/praxec/praxec-meta"),
        "https://github.com/praxec/praxec-meta"
    );
}

#[test]
fn remote_packs_example_validates_fully_offline_via_mocked_git_transport() {
    let example_path = workspace_root().join("examples/remote-packs/gateway.yaml");
    let real_text = std::fs::read_to_string(&example_path)
        .unwrap_or_else(|e| panic!("reading {}: {e}", example_path.display()));
    assert!(
        real_text.contains("git+https://github.com/praxec/cognitive-architectures"),
        "example no longer sources cognitive-architectures the expected way; update this test"
    );
    assert!(
        real_text.contains("git+https://github.com/praxec/praxec-meta"),
        "example no longer sources praxec-meta the expected way; update this test"
    );

    let host_dir = tempfile::tempdir().unwrap();
    let cog_bare = host_dir.path().join("bare-cognitive-architectures.git");
    let meta_bare = host_dir.path().join("bare-praxec-meta.git");

    // A byte-for-byte copy of the real example, with ONLY the two
    // `github.com` `uri:` hosts rewritten to local `file://` bare repos.
    // `ref:` (`main` / `v1.0.0`) is untouched — the mock bare repos carry
    // matching refs, so the example's real pins are exercised as-is.
    let mocked_text = real_text
        .replace(
            "git+https://github.com/praxec/cognitive-architectures",
            &format!("file://{}", cog_bare.display()),
        )
        .replace(
            "git+https://github.com/praxec/praxec-meta",
            &format!("file://{}", meta_bare.display()),
        );
    let mocked_path = host_dir.path().join("gateway.yaml");
    std::fs::write(&mocked_path, &mocked_text).unwrap();

    // RED — before the mock bare repos exist, the exact same load must
    // fail (proves this isn't vacuously green: the mock is load-bearing).
    let before_mock = config::load_resolved_with_repos(&mocked_path);
    assert!(
        before_mock.is_err(),
        "expected load to fail before the mock bare repos exist (got Ok) — \
         the test would be vacuous otherwise"
    );

    // GREEN — build the mock bare repos (minimal-but-valid packs;
    // `cognitive-architectures` under namespace `cognitive`, `praxec-meta`
    // under namespace `meta`), then the identical load must fully resolve
    // through the SAME `load_resolved_with_repos` path `praxec check` uses.
    build_bare_repo(&mock_pack_dir("cognitive-architectures"), &cog_bare, None);
    build_bare_repo(&mock_pack_dir("praxec-meta"), &meta_bare, Some("v1.0.0"));

    let (resolved, diagnostics) = config::load_resolved_with_repos(&mocked_path)
        .unwrap_or_else(|e| panic!("remote-packs example failed to resolve offline: {e:#}"));
    assert!(
        diagnostics.is_empty(),
        "expected 0 diagnostics validating the mocked remote-packs example, got: {diagnostics:?}"
    );

    let workflows = resolved
        .get("workflows")
        .and_then(|v| v.as_object())
        .unwrap_or_else(|| panic!("resolved config carries no `workflows` object: {resolved:#?}"));
    assert!(
        workflows.contains_key("cognitive/flow.mock-hello"),
        "expected the cognitive-architectures mock pack's workflow, namespaced `cognitive/`; \
         got keys: {:?}",
        workflows.keys().collect::<Vec<_>>()
    );
    assert!(
        workflows.contains_key("meta/flow.mock-hello"),
        "expected the praxec-meta mock pack's workflow, namespaced `meta/`; got keys: {:?}",
        workflows.keys().collect::<Vec<_>>()
    );
}
