//! onboarding-hardening Cluster 3 (D5) — `lifecycle:` surfacing at the CLI /
//! binary surface: the field-report repro as end-to-end assertions.
//!
//! The D5 bug was that a placeholder (spec-only) executor was structurally
//! indistinguishable from a working one — a `lifecycle: stub` definition "ran to
//! success" with no signal. The fix surfaces the placeholder in BOTH directions:
//!
//! - `check` reports it as a NON-FATAL `PLACEHOLDER_LIFECYCLE` warning — exit 0
//!   (a stub is well-formed) but loud, so it is never silently taken for real.
//! - the `command` (start) RESPONSE echoes the `lifecycle`, so a caller sees
//!   maturity AT run, not only in `describe`.
//! - no false positive: a real (`stable`) lifecycle is not flagged; an
//!   undeclared lifecycle adds no key.
//!
//! The run-response echo is also covered deterministically at the lib level in
//! `praxec-core/tests/lifecycle_surface.rs`; this adds the binary/CLI end-to-end
//! layer so D5 is verified the same way as D1–D4/D7 (the other CLI-level
//! field-report repros in `config_readiness_cli.rs` / `config_structure_cli.rs`).
//! Its sibling D6 (`reference_only`) is verified at the unit level in
//! `praxec-core/src/repo.rs` because its `UNSCANNED_DEFINITION_DIR` signal is a
//! load-time `tracing::warn!`, not a `check` diagnostic.

use std::path::Path;
use std::process::{Command, Output};

fn bin() -> &'static str {
    env!("CARGO_BIN_EXE_praxec")
}

fn run(args: &[&str]) -> Output {
    Command::new(bin()).args(args).output().expect("run praxec")
}

/// A gateway.yaml with one inline workflow. `lifecycle` is set iff `Some`. When
/// `repo_path` is `Some`, a writable repo is declared — a *run* needs a writable
/// `repo_root` (the run-ambient RepoRoot requirement), while `check` does not.
fn stub_config(lifecycle: Option<&str>, repo_path: Option<&Path>) -> String {
    let lc = lifecycle
        .map(|l| format!("    lifecycle: {l}\n"))
        .unwrap_or_default();
    let repos = repo_path
        .map(|p| {
            format!(
                "repos:\n  - path: \"{}\"\n    writable: true\n",
                p.display()
            )
        })
        .unwrap_or_default();
    format!(
        "version: \"1.0.0\"\ngateway:\n  allow_ephemeral: true\n{repos}\
         workflows:\n  wf.stub:\n    title: Stub Demo\n{lc}    initialState: start\n    states:\n      \
         start:\n        transitions:\n          go: {{ target: done, executor: {{ kind: noop }} }}\n      \
         done: {{ terminal: true }}\n"
    )
}

/// A minimal writable repo (just a manifest) so a run has a `repo_root`.
fn write_repo(dir: &Path) {
    std::fs::create_dir_all(dir).unwrap();
    std::fs::write(
        dir.join("praxec.repo.yaml"),
        "schema: praxec.repo/v1\nname: local\nnamespace: local\nversion: 0.1.0\n",
    )
    .unwrap();
}

// ── check: the placeholder is loud but non-fatal ─────────────────────────────

#[test]
fn stub_lifecycle_is_a_nonfatal_check_warning() {
    let td = tempfile::tempdir().unwrap();
    let cfg = td.path().join("gw.yaml");
    std::fs::write(&cfg, stub_config(Some("stub"), None)).unwrap();

    let out = run(&["check", "--config", cfg.to_str().unwrap()]);
    let so = String::from_utf8_lossy(&out.stdout);
    assert!(
        out.status.success(),
        "a stub lifecycle is well-formed — check must exit 0 (non-fatal):\n{so}"
    );
    assert!(
        so.contains("PLACEHOLDER_LIFECYCLE"),
        "check must surface the placeholder so a stub is never silently taken for real:\n{so}"
    );
    assert!(so.contains("wf.stub"), "names the offending definition:\n{so}");
}

#[test]
fn working_lifecycle_is_not_flagged_by_check() {
    // No false positive: a real (non-placeholder) lifecycle draws no warning.
    let td = tempfile::tempdir().unwrap();
    let cfg = td.path().join("gw.yaml");
    std::fs::write(&cfg, stub_config(Some("stable"), None)).unwrap();

    let out = run(&["check", "--config", cfg.to_str().unwrap()]);
    let so = String::from_utf8_lossy(&out.stdout);
    assert!(out.status.success(), "{so}");
    assert!(
        !so.contains("PLACEHOLDER_LIFECYCLE"),
        "a real lifecycle must not be flagged as a placeholder:\n{so}"
    );
}

// ── start: the response echoes lifecycle so a caller sees it AT run ───────────

#[test]
fn command_start_echoes_stub_lifecycle() {
    let td = tempfile::tempdir().unwrap();
    let repo = td.path().join("repo");
    write_repo(&repo);
    let cfg = td.path().join("gw.yaml");
    std::fs::write(&cfg, stub_config(Some("stub"), Some(&repo))).unwrap();

    let out = run(&[
        "command",
        "{\"definitionId\":\"wf.stub\"}",
        "--config",
        cfg.to_str().unwrap(),
    ]);
    let so = String::from_utf8_lossy(&out.stdout);
    assert!(
        out.status.success(),
        "start must succeed:\n{so}{}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(
        so.contains("\"lifecycle\": \"stub\""),
        "the start response must echo the stub lifecycle (the D5 'ran to success' fix):\n{so}"
    );
}

#[test]
fn command_start_omits_lifecycle_when_undeclared() {
    // No false surfacing: a definition that declares no lifecycle adds no key.
    let td = tempfile::tempdir().unwrap();
    let repo = td.path().join("repo");
    write_repo(&repo);
    let cfg = td.path().join("gw.yaml");
    std::fs::write(&cfg, stub_config(None, Some(&repo))).unwrap();

    let out = run(&[
        "command",
        "{\"definitionId\":\"wf.stub\"}",
        "--config",
        cfg.to_str().unwrap(),
    ]);
    let so = String::from_utf8_lossy(&out.stdout);
    assert!(out.status.success(), "{so}");
    assert!(
        !so.contains("\"lifecycle\""),
        "no lifecycle key when the definition declares none:\n{so}"
    );
}
