//! Integration tests for `px mcp init`.
//!
//! The subcommand body lives in `crates/praxec-tui/src/mcp_init.rs`.
//! These tests exercise the file-generation contract end-to-end against
//! a tempdir — they verify the on-disk shape operators get, not just the
//! Rust function signatures.

use std::path::PathBuf;

use praxec_tui::mcp_init::{McpInitArgs, run_init};
use tempfile::TempDir;

fn args(dir: PathBuf) -> McpInitArgs {
    McpInitArgs {
        dir: Some(dir),
        cursor: false,
        claude_desktop: false,
        force: false,
    }
}

#[test]
fn writes_base_mcp_json_to_default_path() {
    let td = TempDir::new().unwrap();
    run_init(&args(td.path().to_path_buf())).expect("init runs");

    let p = td.path().join(".mcp.json");
    assert!(p.exists(), ".mcp.json should land at target dir root");

    let body = std::fs::read_to_string(&p).unwrap();
    let parsed: serde_json::Value =
        serde_json::from_str(&body).expect("generated file is valid JSON");
    assert!(
        parsed.pointer("/mcpServers/praxec/command").is_some(),
        "expected mcpServers.praxec.command in {body}"
    );
}

#[test]
fn cursor_flag_writes_cursor_mcp_json_under_dotcursor() {
    let td = TempDir::new().unwrap();
    let mut a = args(td.path().to_path_buf());
    a.cursor = true;
    run_init(&a).expect("init with --cursor runs");

    let cursor = td.path().join(".cursor").join("mcp.json");
    assert!(cursor.exists(), "--cursor should write .cursor/mcp.json");
}

#[test]
fn claude_desktop_flag_writes_snippet_alongside_base() {
    let td = TempDir::new().unwrap();
    let mut a = args(td.path().to_path_buf());
    a.claude_desktop = true;
    run_init(&a).expect("init with --claude-desktop runs");

    let snippet = td.path().join("claude_desktop_config.json");
    assert!(
        snippet.exists(),
        "--claude-desktop should write a snippet file"
    );
}

#[test]
fn refuses_to_overwrite_existing_file_without_force() {
    let td = TempDir::new().unwrap();
    let p = td.path().join(".mcp.json");
    std::fs::write(&p, "{\"existing\": \"different content\"}").unwrap();

    let err = run_init(&args(td.path().to_path_buf()))
        .expect_err("should refuse to overwrite without --force");
    let msg = format!("{:#}", err);
    assert!(
        msg.contains("refusing to overwrite") && msg.contains(".mcp.json"),
        "error should explain refusal and name the file; got: {msg}"
    );
}

#[test]
fn force_flag_overwrites_existing_file() {
    let td = TempDir::new().unwrap();
    let p = td.path().join(".mcp.json");
    std::fs::write(&p, "{\"existing\": \"different content\"}").unwrap();

    let mut a = args(td.path().to_path_buf());
    a.force = true;
    run_init(&a).expect("--force should overwrite");

    let body = std::fs::read_to_string(&p).unwrap();
    assert!(
        body.contains("mcpServers") && body.contains("praxec"),
        "overwritten file should be the generated shape; got: {body}"
    );
}

#[test]
fn idempotent_when_existing_contents_match_generated() {
    // Two consecutive runs without --force should both succeed; the
    // second one detects the no-op and returns Ok.
    let td = TempDir::new().unwrap();
    run_init(&args(td.path().to_path_buf())).expect("first run");
    run_init(&args(td.path().to_path_buf())).expect("second run idempotent");
}

#[test]
fn errors_when_target_directory_missing() {
    let td = TempDir::new().unwrap();
    let missing = td.path().join("does-not-exist");
    let err = run_init(&args(missing.clone())).expect_err("missing dir should error");
    assert!(
        format!("{err}").contains("does not exist"),
        "error should name the missing dir"
    );
}
