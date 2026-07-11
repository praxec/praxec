//! D4a — `px connections add` (stage) + `px connections grant` (the separate
//! explicit trust act) end-to-end through the built binary. Each invocation is
//! its own process editing the on-disk config; the assertions read the resulting
//! file back and resolve it through the load gate.

use std::path::Path;
use std::process::{Command, Output};

const BASE: &str = "version: \"1.0.0\"\n";

fn run(config: &Path, args: &[&str]) -> Output {
    let bin = env!("CARGO_BIN_EXE_praxec");
    Command::new(bin)
        .arg("connections")
        .args(args)
        .arg("--config")
        .arg(config)
        .output()
        .expect("run praxec connections")
}

fn write_base() -> (tempfile::TempDir, std::path::PathBuf) {
    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().join("gateway.yaml");
    std::fs::write(&path, BASE).expect("write base config");
    (dir, path)
}

#[test]
fn add_stages_ungranted_then_grant_promotes_live() {
    let (_d, path) = write_base();

    // add — stages the connection (NOT live).
    let out = run(
        &path,
        &[
            "add",
            "github",
            "--kind",
            "mcp",
            "--command",
            "npx",
            "--arg",
            "-y",
            "--arg",
            "pkg",
        ],
    );
    assert!(
        out.status.success(),
        "add failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    let resolved = praxec_core::config::load_resolved_with_repos(&path)
        .expect("resolves after add")
        .0;
    assert!(
        resolved.pointer("/connections/github").is_none(),
        "staged connection must not be live before grant"
    );
    assert!(
        resolved
            .pointer("/praxec/_ungrantedConnections/github")
            .is_some(),
        "staged connection must be stamped ungranted"
    );

    // grant — the separate explicit trust act; promotes it live. The test
    // harness is non-interactive (stdin is not a TTY), so the F13 origin gate
    // requires the explicit `--yes` operator-intent flag.
    let out = run(&path, &["grant", "github", "--yes"]);
    assert!(
        out.status.success(),
        "grant failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    let resolved = praxec_core::config::load_resolved_with_repos(&path)
        .expect("resolves after grant")
        .0;
    assert_eq!(
        resolved
            .pointer("/connections/github/kind")
            .and_then(serde_json::Value::as_str),
        Some("mcp"),
        "granted connection must be live"
    );
}

#[test]
fn duplicate_add_exits_non_zero() {
    let (_d, path) = write_base();
    assert!(
        run(&path, &["add", "c", "--kind", "cli", "--command", "gh"])
            .status
            .success()
    );
    let out = run(&path, &["add", "c", "--kind", "cli", "--command", "gh"]);
    assert!(!out.status.success(), "a duplicate add must exit non-zero");
}

#[test]
fn grant_of_unstaged_exits_non_zero() {
    let (_d, path) = write_base();
    // `--yes` clears the F13 origin gate so this exercises the unstaged
    // fail-fast, not the operator check.
    let out = run(&path, &["grant", "ghost", "--yes"]);
    assert!(
        !out.status.success(),
        "granting an unstaged connection must exit non-zero"
    );
    assert!(
        !String::from_utf8_lossy(&out.stderr).contains("GRANT_REQUIRES_OPERATOR"),
        "with --yes the failure must be the unstaged fail-fast, not the origin gate"
    );
}

/// F13 — the operator-origin gate on `grant` (the CLI mirror of the P16
/// human-origin rule): a NON-INTERACTIVE grant (stdin not a TTY — this test
/// spawns the binary with a null stdin) is refused fail-closed with
/// `GRANT_REQUIRES_OPERATOR` and writes NOTHING, unless explicit operator
/// intent is stated with `--yes`.
#[test]
fn non_interactive_grant_is_refused_without_yes_and_succeeds_with_it() {
    let (_d, path) = write_base();
    assert!(
        run(
            &path,
            &["add", "github", "--kind", "mcp", "--command", "npx"]
        )
        .status
        .success(),
        "stage the connection"
    );

    // Without --yes: refused with the typed code, connection stays staged.
    let out = run(&path, &["grant", "github"]);
    assert!(
        !out.status.success(),
        "a non-interactive grant without --yes must exit non-zero"
    );
    assert!(
        String::from_utf8_lossy(&out.stderr).contains("GRANT_REQUIRES_OPERATOR"),
        "the refusal must carry the typed GRANT_REQUIRES_OPERATOR code, got: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    let resolved = praxec_core::config::load_resolved_with_repos(&path)
        .expect("resolves after refused grant")
        .0;
    assert!(
        resolved.pointer("/connections/github").is_none(),
        "a refused grant must not promote the connection live"
    );
    assert!(
        resolved
            .pointer("/praxec/_ungrantedConnections/github")
            .is_some(),
        "a refused grant must leave the connection staged"
    );

    // With --yes: explicit operator intent — the grant proceeds.
    let out = run(&path, &["grant", "github", "--yes"]);
    assert!(
        out.status.success(),
        "grant --yes failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    let resolved = praxec_core::config::load_resolved_with_repos(&path)
        .expect("resolves after grant")
        .0;
    assert_eq!(
        resolved
            .pointer("/connections/github/kind")
            .and_then(serde_json::Value::as_str),
        Some("mcp"),
        "an explicit --yes grant must promote the connection live"
    );
}

#[test]
fn inapplicable_flag_for_kind_exits_non_zero() {
    let (_d, path) = write_base();
    // --command does not apply to a rest connection.
    let out = run(
        &path,
        &[
            "add",
            "api",
            "--kind",
            "rest",
            "--url",
            "https://x",
            "--command",
            "oops",
        ],
    );
    assert!(
        !out.status.success(),
        "an inapplicable flag must be rejected"
    );
}
