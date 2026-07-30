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

/// P2.1/P2.3b — a connection body declaring `required_secrets` must actually
/// be GRANTABLE. P2.1 (#169) added the `praxec doctor` check that reads a
/// connection's `required_secrets`, but the gateway config schema's
/// `mcpConnection` `$defs` never gained the matching property —
/// `additionalProperties: false` meant `connections grant` rejected any
/// staged body that declared it with `INVALID_STAGED_CONNECTION`, silently
/// making the P2.1 feature ungrantable. Fixed alongside P2.3b (which needs
/// `required_secrets` to ride in a `--block`-staged body); this pins the
/// schema fix so it can't regress.
#[test]
fn add_block_with_required_secrets_stages_and_grants() {
    let (_d, path) = write_base();

    assert!(
        run(
            &path,
            &[
                "add",
                "figma",
                "--block",
                r#"{"kind":"mcp","command":"figma-mcp","required_secrets":["FIGMA_TOKEN"]}"#,
            ],
        )
        .status
        .success(),
        "stage a connection declaring required_secrets"
    );

    let out = run(&path, &["grant", "figma", "--yes"]);
    assert!(
        out.status.success(),
        "granting a required_secrets-declaring connection must succeed \
         (schema must allow the field): {}",
        String::from_utf8_lossy(&out.stderr)
    );
    let resolved = praxec_core::config::load_resolved_with_repos(&path)
        .expect("resolves after grant")
        .0;
    assert_eq!(
        resolved
            .pointer("/connections/figma/required_secrets/0")
            .and_then(serde_json::Value::as_str),
        Some("FIGMA_TOKEN"),
        "the granted live connection must carry required_secrets through"
    );
}

/// P2.3b — `--block <json>` stages the WHOLE connection body (env: map
/// included) in one token, so a workflow's `kind: cli` step (static `args:`
/// array) can wire an arbitrary-length set of collected secrets/config into
/// the staged connection's `env:` by building the body in a prior step.
#[test]
fn add_block_stages_whole_body_with_env() {
    let (_d, path) = write_base();

    let out = run(
        &path,
        &[
            "add",
            "figma",
            "--block",
            r#"{"kind":"mcp","command":"figma-mcp","env":{"FIGMA_TOKEN":"$FIGMA_TOKEN","OTHER_SECRET":"$OTHER_SECRET"}}"#,
        ],
    );
    assert!(
        out.status.success(),
        "add --block failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );

    let resolved = praxec_core::config::load_resolved_with_repos(&path)
        .expect("resolves after add --block")
        .0;
    assert!(
        resolved.pointer("/connections/figma").is_none(),
        "a --block-staged connection must not be live before grant"
    );
    assert!(
        resolved
            .pointer("/praxec/_ungrantedConnections/figma")
            .is_some(),
        "staged connection must be stamped ungranted"
    );
    // `_ungrantedConnections` only stamps {repo, namespace, remedy} — the
    // staged BODY (env: included) lives under `stagedConnections:` in the
    // raw on-disk YAML (not the resolved/gated config), so read it back
    // there to confirm the arbitrary-length env map from --block survived.
    let raw = std::fs::read_to_string(&path).expect("read back config");
    let doc: serde_yaml::Value = serde_yaml::from_str(&raw).expect("parse written yaml");
    assert_eq!(
        doc["stagedConnections"]["figma"]["env"]["FIGMA_TOKEN"].as_str(),
        Some("$FIGMA_TOKEN"),
        "the arbitrary-length env map from --block must survive to the staged body"
    );
    assert_eq!(
        doc["stagedConnections"]["figma"]["env"]["OTHER_SECRET"].as_str(),
        Some("$OTHER_SECRET")
    );

    // grant — --block-staged connections go live exactly like flag-staged ones.
    let out = run(&path, &["grant", "figma", "--yes"]);
    assert!(
        out.status.success(),
        "grant of a --block-staged connection failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    let resolved = praxec_core::config::load_resolved_with_repos(&path)
        .expect("resolves after grant")
        .0;
    assert_eq!(
        resolved
            .pointer("/connections/figma/env/FIGMA_TOKEN")
            .and_then(serde_json::Value::as_str),
        Some("$FIGMA_TOKEN")
    );
}

#[test]
fn add_block_duplicate_name_exits_non_zero() {
    let (_d, path) = write_base();
    assert!(
        run(
            &path,
            &["add", "c", "--block", r#"{"kind":"cli","command":"gh"}"#]
        )
        .status
        .success()
    );
    let out = run(
        &path,
        &["add", "c", "--block", r#"{"kind":"cli","command":"gh"}"#],
    );
    assert!(
        !out.status.success(),
        "a duplicate --block add must exit non-zero"
    );
}

#[test]
fn add_block_invalid_json_exits_non_zero() {
    let (_d, path) = write_base();
    let out = run(&path, &["add", "bad", "--block", "{not json"]);
    assert!(
        !out.status.success(),
        "invalid --block JSON must exit non-zero"
    );
    assert!(
        String::from_utf8_lossy(&out.stderr).contains("INVALID_CONNECTION_BLOCK"),
        "stderr must carry the typed INVALID_CONNECTION_BLOCK code, got: {}",
        String::from_utf8_lossy(&out.stderr)
    );
}

#[test]
fn add_without_block_or_kind_exits_non_zero() {
    let (_d, path) = write_base();
    let out = run(&path, &["add", "neither"]);
    assert!(
        !out.status.success(),
        "add with neither --block nor --kind must exit non-zero"
    );
}

/// P2.4 — `connections revoke` is the explicit, auditable MIRROR of `grant`:
/// it removes the name from `grant_connections:`, demoting the connection
/// back to inert/staged (never live), while the staged body itself survives.
#[test]
fn revoke_demotes_granted_connection_back_to_staged() {
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
    assert!(
        run(&path, &["grant", "github", "--yes"]).status.success(),
        "grant the connection"
    );
    let resolved = praxec_core::config::load_resolved_with_repos(&path)
        .expect("resolves after grant")
        .0;
    assert_eq!(
        resolved
            .pointer("/connections/github/kind")
            .and_then(serde_json::Value::as_str),
        Some("mcp"),
        "sanity: granted connection is live before revoke"
    );

    let out = run(&path, &["revoke", "github"]);
    assert!(
        out.status.success(),
        "revoke failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );

    let resolved = praxec_core::config::load_resolved_with_repos(&path)
        .expect("resolves after revoke")
        .0;
    assert!(
        resolved.pointer("/connections/github").is_none(),
        "a revoked connection must no longer be live"
    );
    assert!(
        resolved
            .pointer("/praxec/_ungrantedConnections/github")
            .is_some(),
        "a revoked connection must fall back to staged/ungranted"
    );
}

/// Revoking a connection that was never granted (only staged, or not present
/// at all) is a fail-fast, not a silent no-op.
#[test]
fn revoke_of_ungranted_exits_non_zero() {
    let (_d, path) = write_base();

    // Never mentioned at all.
    let out = run(&path, &["revoke", "ghost"]);
    assert!(
        !out.status.success(),
        "revoking an unmentioned connection must exit non-zero"
    );
    assert!(
        String::from_utf8_lossy(&out.stderr).contains("CONNECTION_NOT_GRANTED"),
        "stderr must carry the typed CONNECTION_NOT_GRANTED code, got: {}",
        String::from_utf8_lossy(&out.stderr)
    );

    // Staged but never granted.
    assert!(
        run(&path, &["add", "c", "--kind", "cli", "--command", "gh"])
            .status
            .success()
    );
    let out = run(&path, &["revoke", "c"]);
    assert!(
        !out.status.success(),
        "revoking a merely-staged (never-granted) connection must exit non-zero"
    );
    assert!(
        String::from_utf8_lossy(&out.stderr).contains("CONNECTION_NOT_GRANTED"),
        "stderr must carry the typed CONNECTION_NOT_GRANTED code, got: {}",
        String::from_utf8_lossy(&out.stderr)
    );
}

/// Revoking twice fails the second time — revoke is not idempotent, it fails
/// fast rather than silently accepting a no-op re-revoke.
#[test]
fn double_revoke_exits_non_zero_second_time() {
    let (_d, path) = write_base();
    assert!(
        run(&path, &["add", "c", "--kind", "cli", "--command", "gh"])
            .status
            .success()
    );
    assert!(run(&path, &["grant", "c", "--yes"]).status.success());
    assert!(run(&path, &["revoke", "c"]).status.success());

    let out = run(&path, &["revoke", "c"]);
    assert!(
        !out.status.success(),
        "a second revoke of the same connection must exit non-zero"
    );
    assert!(String::from_utf8_lossy(&out.stderr).contains("CONNECTION_NOT_GRANTED"));
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
