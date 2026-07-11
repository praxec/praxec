//! D4a — the governed connections-write primitives: `add` stages an UNGRANTED
//! connection; `grant` is the separate explicit trust act. These tests exercise
//! each write path plus every fail-fast, and prove the round-trip through the
//! config-load gate: a staged connection is inert (diverted to
//! `_ungrantedConnections`), and only a granted one is promoted into the live
//! `/connections` registry.

use std::path::Path;

use praxec_executors::conn_write::{
    ConnWriteError, ConnectionSpec, add_connection, grant_connection,
};
use serde_json::Value;

const BASE: &str = "version: \"1.0.0\"\n";

/// Write `contents` to a fresh temp config and return (dir, path). The dir must
/// outlive the path (drops clean up the file).
fn write_config(contents: &str) -> (tempfile::TempDir, std::path::PathBuf) {
    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().join("gateway.yaml");
    std::fs::write(&path, contents).expect("write config");
    (dir, path)
}

/// Resolve the config through the full multi-repo load path (which runs the
/// staged-connection grant gate) and return the resolved value.
fn resolve(path: &Path) -> Value {
    praxec_core::config::load_resolved_with_repos(path)
        .expect("config resolves")
        .0
}

fn mcp_spec() -> ConnectionSpec {
    ConnectionSpec::Mcp {
        command: Some("npx".into()),
        args: vec!["-y".into(), "@modelcontextprotocol/server-github".into()],
        url: None,
        env: vec![("TOKEN".into(), "xyz".into())],
    }
}

// ── happy path: add stages ungranted, grant promotes to live ─────────────────

#[test]
fn add_stages_mcp_as_ungranted_not_live() {
    let (_d, path) = write_config(BASE);
    add_connection(&path, "github", &mcp_spec()).expect("add stages");

    let resolved = resolve(&path);
    // Not in the live registry…
    assert!(
        resolved.pointer("/connections/github").is_none(),
        "a staged connection must NOT be live"
    );
    // …and diverted to the ungranted stamp with a grant remedy.
    let stamp = resolved
        .pointer("/praxec/_ungrantedConnections/github")
        .expect("staged connection is stamped ungranted");
    assert!(
        stamp
            .get("remedy")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .contains("px connections grant github"),
        "ungranted stamp must carry the grant remedy, got: {stamp}"
    );
    // The MCP consumer does not see it as a spawnable connection.
    let conns = praxec_executors::mcp::McpConnections::from_config(&resolved);
    assert!(conns.get("github").is_none(), "staged mcp is not spawnable");
}

#[test]
fn grant_promotes_staged_mcp_to_live() {
    let (_d, path) = write_config(BASE);
    add_connection(&path, "github", &mcp_spec()).expect("add");
    let body = grant_connection(&path, "github").expect("grant");
    assert_eq!(body.get("kind").and_then(Value::as_str), Some("mcp"));

    let resolved = resolve(&path);
    // Promoted into the live registry, and no longer stamped ungranted.
    assert_eq!(
        resolved
            .pointer("/connections/github/kind")
            .and_then(Value::as_str),
        Some("mcp")
    );
    assert!(
        resolved
            .pointer("/praxec/_ungrantedConnections/github")
            .is_none(),
        "a granted connection is not ungranted"
    );
    // The MCP consumer now sees a spawnable connection with the written fields.
    let conns = praxec_executors::mcp::McpConnections::from_config(&resolved);
    let c = conns.get("github").expect("granted mcp is spawnable");
    assert_eq!(c.command.as_deref(), Some("npx"));
    assert_eq!(c.args, vec!["-y", "@modelcontextprotocol/server-github"]);
    assert_eq!(c.env.get("TOKEN").map(String::as_str), Some("xyz"));
}

#[test]
fn add_and_grant_cli_connection() {
    let (_d, path) = write_config(BASE);
    let spec = ConnectionSpec::Cli {
        command: "./build.sh".into(),
        working_directory: Some("/repo".into()),
        env: vec![("CI".into(), "1".into())],
    };
    add_connection(&path, "builder", &spec).expect("add");
    grant_connection(&path, "builder").expect("grant");

    let resolved = resolve(&path);
    assert_eq!(
        resolved
            .pointer("/connections/builder/kind")
            .and_then(Value::as_str),
        Some("cli")
    );
    assert_eq!(
        resolved
            .pointer("/connections/builder/command")
            .and_then(Value::as_str),
        Some("./build.sh")
    );
    assert_eq!(
        resolved
            .pointer("/connections/builder/workingDirectory")
            .and_then(Value::as_str),
        Some("/repo")
    );
}

#[test]
fn add_and_grant_rest_connection() {
    let (_d, path) = write_config(BASE);
    let spec = ConnectionSpec::Rest {
        base_url: "https://api.example.com".into(),
        headers: vec![("Authorization".into(), "Bearer t".into())],
    };
    add_connection(&path, "api", &spec).expect("add");
    grant_connection(&path, "api").expect("grant");

    let resolved = resolve(&path);
    assert_eq!(
        resolved
            .pointer("/connections/api/baseUrl")
            .and_then(Value::as_str),
        Some("https://api.example.com")
    );
    let conns = praxec_executors::rest::RestConnections::from_config(&resolved);
    let c = conns.get("api").expect("granted rest is live");
    assert_eq!(c.base_url, "https://api.example.com");
    assert_eq!(
        c.headers.get("Authorization").map(String::as_str),
        Some("Bearer t")
    );
}

// ── fail-fast paths ──────────────────────────────────────────────────────────

#[test]
fn duplicate_staged_name_is_rejected() {
    let (_d, path) = write_config(BASE);
    add_connection(&path, "github", &mcp_spec()).expect("first add");
    let err = add_connection(&path, "github", &mcp_spec()).unwrap_err();
    assert!(matches!(err, ConnWriteError::DuplicateName(n) if n == "github"));
}

#[test]
fn name_colliding_with_a_live_connection_is_rejected() {
    let cfg = format!("{BASE}connections:\n  github:\n    kind: cli\n    command: gh\n");
    let (_d, path) = write_config(&cfg);
    let err = add_connection(&path, "github", &mcp_spec()).unwrap_err();
    assert!(matches!(err, ConnWriteError::DuplicateName(_)));
}

#[test]
fn mcp_without_command_or_url_is_rejected() {
    let (_d, path) = write_config(BASE);
    let spec = ConnectionSpec::Mcp {
        command: None,
        args: vec![],
        url: None,
        env: vec![],
    };
    let err = add_connection(&path, "bad", &spec).unwrap_err();
    assert!(matches!(err, ConnWriteError::InvalidFields(_)));
}

#[test]
fn name_with_slash_is_rejected() {
    let (_d, path) = write_config(BASE);
    let err = add_connection(&path, "ns/tool", &mcp_spec()).unwrap_err();
    assert!(matches!(err, ConnWriteError::InvalidName(_)));
}

#[test]
fn grant_of_unstaged_connection_is_rejected() {
    let (_d, path) = write_config(BASE);
    let err = grant_connection(&path, "nope").unwrap_err();
    assert!(matches!(err, ConnWriteError::NotStaged(n) if n == "nope"));
}

#[test]
fn double_grant_is_rejected() {
    let (_d, path) = write_config(BASE);
    add_connection(&path, "github", &mcp_spec()).expect("add");
    grant_connection(&path, "github").expect("first grant");
    let err = grant_connection(&path, "github").unwrap_err();
    assert!(matches!(err, ConnWriteError::AlreadyGranted(n) if n == "github"));
}

#[test]
fn unreadable_config_fails_fast() {
    let err = add_connection(Path::new("/no/such/config.yaml"), "x", &mcp_spec()).unwrap_err();
    assert!(matches!(err, ConnWriteError::Io(_)));
}

// ── F12: writer and reader share one key source of truth ────────────────────

#[test]
fn staged_and_grant_keys_match_the_core_gate() {
    // The key names are OWNED by praxec-core (`config::STAGED_CONNECTIONS_KEY`
    // / `config::GRANT_CONNECTIONS_KEY`) — the reader side of the grant gate.
    // Prove the writer emits exactly those top-level keys, so a future
    // re-hardcoded literal in either crate fails here instead of silently
    // staging connections the gate never sees.
    use praxec_core::config::{GRANT_CONNECTIONS_KEY, STAGED_CONNECTIONS_KEY};

    let (_d, path) = write_config(BASE);
    add_connection(&path, "github", &mcp_spec()).expect("add");
    grant_connection(&path, "github").expect("grant");

    let raw: serde_yaml::Value =
        serde_yaml::from_str(&std::fs::read_to_string(&path).expect("read config"))
            .expect("valid yaml");
    assert!(
        raw.get(STAGED_CONNECTIONS_KEY)
            .and_then(|s| s.get("github"))
            .is_some(),
        "add writes under core's `{STAGED_CONNECTIONS_KEY}:`"
    );
    assert!(
        raw.get(GRANT_CONNECTIONS_KEY)
            .and_then(|g| g.as_sequence())
            .is_some_and(|g| g.iter().any(|v| v.as_str() == Some("github"))),
        "grant writes under core's `{GRANT_CONNECTIONS_KEY}:`"
    );
}
