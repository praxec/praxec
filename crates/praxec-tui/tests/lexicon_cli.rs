//! CLI tests for `px lexicon` subcommand suite.
//!
//! Each test asserts ONE behavior.
//! Pattern: spawn the real `praxec` binary via
//! `std::process::Command::new(env!("CARGO_BIN_EXE_px"))`.

use std::process::Command;

fn binary() -> String {
    env!("CARGO_BIN_EXE_px").to_string()
}

/// Write a minimal `praxec.yaml` with:
/// - a defined term `evidence-pack`
/// - a workflow executor referencing `build.evidence-foo` (an undefined,
///   non-lexicon subject), which creates a PENDING_DEFINITION placeholder
///   for `evidence-foo`
fn write_fixture_config(dir: &std::path::Path) -> std::path::PathBuf {
    let path = dir.join("praxec.yaml");
    let content = r#"
version: "1.0.0"
praxec:
  strict_namespacing: false
lexicon:
  evidence-pack:
    definition_short: "A structured bundle of supporting artefacts."
    governance: human-only
workflows:
  test-workflow:
    initial_state: start
    states:
      start:
        transitions:
          go:
            target: done
            executor:
              kind: script
              subject: build.evidence-foo
      done:
        kind: terminal
"#;
    std::fs::write(&path, content).expect("write fixture config");
    path
}

// ── A. lexicon define ─────────────────────────────────────────────────────────

#[test]
fn define_exits_zero_with_definition_short() {
    let dir = tempfile::tempdir().expect("tempdir");
    let cfg = write_fixture_config(dir.path());

    let out = Command::new(binary())
        .args([
            "lexicon",
            "define",
            "my-term",
            "--definition-short",
            "A test term.",
            "--config",
            cfg.to_str().expect("path"),
        ])
        .output()
        .expect("run define");

    assert!(
        out.status.success(),
        "define should exit 0; stderr:\n{}",
        String::from_utf8_lossy(&out.stderr)
    );
}

#[test]
fn define_stdout_contains_term_name() {
    let dir = tempfile::tempdir().expect("tempdir");
    let cfg = write_fixture_config(dir.path());

    let out = Command::new(binary())
        .args([
            "lexicon",
            "define",
            "my-term",
            "--definition-short",
            "A test term.",
            "--config",
            cfg.to_str().expect("path"),
        ])
        .output()
        .expect("run define");

    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(
        stdout.contains("my-term"),
        "stdout should contain the term name; got:\n{stdout}"
    );
}

#[test]
fn define_without_definition_short_exits_nonzero() {
    let dir = tempfile::tempdir().expect("tempdir");
    let cfg = write_fixture_config(dir.path());

    let out = Command::new(binary())
        .args([
            "lexicon",
            "define",
            "my-term",
            "--config",
            cfg.to_str().expect("path"),
        ])
        .output()
        .expect("run define (no --definition-short)");

    assert!(
        !out.status.success(),
        "define without --definition-short should exit nonzero"
    );
}

#[test]
fn define_stderr_mentions_error_on_missing_definition_short() {
    let dir = tempfile::tempdir().expect("tempdir");
    let cfg = write_fixture_config(dir.path());

    let out = Command::new(binary())
        .args([
            "lexicon",
            "define",
            "my-term",
            "--config",
            cfg.to_str().expect("path"),
        ])
        .output()
        .expect("run define (no --definition-short)");

    let stderr = String::from_utf8_lossy(&out.stderr);
    // Clap emits an error about the required argument; check it mentions the flag.
    assert!(
        stderr.contains("definition-short") || stderr.contains("required"),
        "stderr should mention missing --definition-short; got:\n{stderr}"
    );
}

// ── B. lexicon alias ──────────────────────────────────────────────────────────

#[test]
fn alias_add_to_existing_term_exits_zero() {
    let dir = tempfile::tempdir().expect("tempdir");
    let cfg = write_fixture_config(dir.path());

    let out = Command::new(binary())
        .args([
            "lexicon",
            "alias",
            "evidence-pack",
            "--add",
            "ep",
            "--config",
            cfg.to_str().expect("path"),
        ])
        .output()
        .expect("run alias add");

    assert!(
        out.status.success(),
        "alias add to existing term should exit 0; stderr:\n{}",
        String::from_utf8_lossy(&out.stderr)
    );
}

#[test]
fn alias_add_to_unknown_term_exits_nonzero() {
    let dir = tempfile::tempdir().expect("tempdir");
    let cfg = write_fixture_config(dir.path());

    let out = Command::new(binary())
        .args([
            "lexicon",
            "alias",
            "does-not-exist",
            "--add",
            "x",
            "--config",
            cfg.to_str().expect("path"),
        ])
        .output()
        .expect("run alias add to unknown term");

    assert!(
        !out.status.success(),
        "alias add to unknown term should exit nonzero"
    );
}

#[test]
fn alias_add_stderr_mentions_not_found_on_unknown_term() {
    let dir = tempfile::tempdir().expect("tempdir");
    let cfg = write_fixture_config(dir.path());

    let out = Command::new(binary())
        .args([
            "lexicon",
            "alias",
            "does-not-exist",
            "--add",
            "x",
            "--config",
            cfg.to_str().expect("path"),
        ])
        .output()
        .expect("run alias add to unknown term");

    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        stderr.contains("NOT_FOUND") || stderr.contains("no real entry"),
        "stderr should mention NOT_FOUND; got:\n{stderr}"
    );
}

// ── C. lexicon cancel ─────────────────────────────────────────────────────────

#[test]
fn cancel_pending_subject_exits_zero() {
    let dir = tempfile::tempdir().expect("tempdir");
    let cfg = write_fixture_config(dir.path());

    // `evidence-foo` is created as PENDING_DEFINITION by the `build.evidence-foo` script.
    let out = Command::new(binary())
        .args([
            "lexicon",
            "cancel",
            "evidence-foo",
            "--config",
            cfg.to_str().expect("path"),
        ])
        .output()
        .expect("run cancel");

    assert!(
        out.status.success(),
        "cancel pending subject should exit 0; stderr:\n{}",
        String::from_utf8_lossy(&out.stderr)
    );
}

#[test]
fn cancel_non_pending_term_exits_nonzero() {
    let dir = tempfile::tempdir().expect("tempdir");
    let cfg = write_fixture_config(dir.path());

    // `evidence-pack` is an authored (real) entry — not a pending placeholder.
    let out = Command::new(binary())
        .args([
            "lexicon",
            "cancel",
            "evidence-pack",
            "--config",
            cfg.to_str().expect("path"),
        ])
        .output()
        .expect("run cancel on non-pending term");

    assert!(
        !out.status.success(),
        "cancel on non-pending term should exit nonzero"
    );
}

#[test]
fn cancel_stderr_mentions_invalid_resolution_on_non_pending() {
    let dir = tempfile::tempdir().expect("tempdir");
    let cfg = write_fixture_config(dir.path());

    let out = Command::new(binary())
        .args([
            "lexicon",
            "cancel",
            "evidence-pack",
            "--config",
            cfg.to_str().expect("path"),
        ])
        .output()
        .expect("run cancel on non-pending term");

    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        stderr.contains("INVALID_RESOLUTION") || stderr.contains("not a pending"),
        "stderr should mention INVALID_RESOLUTION; got:\n{stderr}"
    );
}

// ── D. lexicon list ───────────────────────────────────────────────────────────

#[test]
fn list_outputs_authored_entry() {
    let dir = tempfile::tempdir().expect("tempdir");
    let cfg = write_fixture_config(dir.path());

    let out = Command::new(binary())
        .args(["lexicon", "list", "--config", cfg.to_str().expect("path")])
        .output()
        .expect("run list");

    assert!(
        out.status.success(),
        "list should exit 0; stderr:\n{}",
        String::from_utf8_lossy(&out.stderr)
    );

    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(
        stdout.contains("evidence-pack"),
        "list should include 'evidence-pack'; got:\n{stdout}"
    );
}

#[test]
fn list_output_is_json_lines() {
    let dir = tempfile::tempdir().expect("tempdir");
    let cfg = write_fixture_config(dir.path());

    let out = Command::new(binary())
        .args(["lexicon", "list", "--config", cfg.to_str().expect("path")])
        .output()
        .expect("run list");

    let stdout = String::from_utf8_lossy(&out.stdout);
    // Each non-empty line must parse as valid JSON.
    for line in stdout.lines().filter(|l| !l.is_empty()) {
        let _: serde_json::Value = serde_json::from_str(line)
            .unwrap_or_else(|e| panic!("line is not JSON: {e}\nline: {line}"));
    }
}

#[test]
fn list_exits_nonzero_without_config() {
    let out = Command::new(binary())
        .args(["lexicon", "list"])
        // Ensure PRAXEC_CONFIG is not set in environment.
        .env_remove("PRAXEC_CONFIG")
        .output()
        .expect("run list without config");

    assert!(
        !out.status.success(),
        "list without config should exit nonzero"
    );
}

// ── E. lexicon pending ────────────────────────────────────────────────────────

#[test]
fn pending_lists_placeholder_subjects() {
    let dir = tempfile::tempdir().expect("tempdir");
    let cfg = write_fixture_config(dir.path());

    let out = Command::new(binary())
        .args([
            "lexicon",
            "pending",
            "--config",
            cfg.to_str().expect("path"),
        ])
        .output()
        .expect("run pending");

    assert!(
        out.status.success(),
        "pending should exit 0; stderr:\n{}",
        String::from_utf8_lossy(&out.stderr)
    );

    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(
        stdout.contains("evidence-foo"),
        "pending should include 'evidence-foo'; got:\n{stdout}"
    );
}

#[test]
fn pending_output_is_json_lines() {
    let dir = tempfile::tempdir().expect("tempdir");
    let cfg = write_fixture_config(dir.path());

    let out = Command::new(binary())
        .args([
            "lexicon",
            "pending",
            "--config",
            cfg.to_str().expect("path"),
        ])
        .output()
        .expect("run pending");

    let stdout = String::from_utf8_lossy(&out.stdout);
    for line in stdout.lines().filter(|l| !l.is_empty()) {
        let _: serde_json::Value = serde_json::from_str(line)
            .unwrap_or_else(|e| panic!("line is not JSON: {e}\nline: {line}"));
    }
}

// ── F. help surface ───────────────────────────────────────────────────────────

#[test]
fn lexicon_appears_in_top_level_help() {
    let out = Command::new(binary())
        .arg("--help")
        .output()
        .expect("run --help");

    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(
        stdout.contains("lexicon"),
        "top-level --help should mention 'lexicon'; got:\n{stdout}"
    );
}
