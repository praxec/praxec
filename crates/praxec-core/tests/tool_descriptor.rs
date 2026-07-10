//! D1 tool descriptor — load / validate behavior.
//!
//! Covers the design's FMECA rows that live at the descriptor boundary:
//! FM2 (kind ↔ reach.connection.kind mismatch fails typed, no partial
//! install), FM3 (auth is names-only; value-bearing fields fail schema
//! validation), plus grant-token extraction (the descriptor *declares* the
//! grant, never performs it) and the copy-never-transform invariant
//! (`reach.connection` round-trips verbatim against the gateway
//! `$defs/connection` shape).

use serde_json::{Value, json};

use praxec_core::discovery::ScriptVerb;
use praxec_core::tool_descriptor::{
    AuthScheme, ProvisionProvider, ToolDescriptor, ToolDescriptorError, ToolKind,
    validate_gateway_connection,
};

// ── fixtures ──────────────────────────────────────────────────────────────

fn mcp_descriptor() -> Value {
    json!({
        "schema_version": "praxec.tool/v1",
        "name": "github-mcp",
        "version": "1.2.0",
        "source_repo": "https://github.com/github/github-mcp-server",
        "description": "GitHub's official MCP server",
        "tags": ["git", "issues"],
        "aliases": ["gh-mcp"],
        "kind": "mcp",
        "reach": {
            "connection_name": "github",
            "grant_as": "github",
            "connection": {
                "kind": "mcp",
                "command": "docker",
                "args": ["run", "-i", "--rm", "ghcr.io/github/github-mcp-server"],
                "env": {}
            },
            "auth": {
                "scheme": "env",
                "env": ["GITHUB_PERSONAL_ACCESS_TOKEN"]
            }
        },
        "provision": {
            "mcp_registry_id": "dev.praxec/github-mcp",
            "version": "1.2.0",
            "providers": ["docker", "release"]
        },
        "operations": [
            {
                "id": "search-issues",
                "verb": "search",
                "input_schema": { "type": "object" },
                "output_schema": { "type": "object" },
                "mcp_tool": "search_issues"
            }
        ],
        "suggested_workflows": ["github/flow.triage-issues"]
    })
}

fn cli_descriptor() -> Value {
    json!({
        "schema_version": "praxec.tool/v1",
        "name": "ripgrep",
        "version": "14.1.0",
        "kind": "cli",
        "reach": {
            "connection_name": "rg",
            "grant_as": "rg",
            "connection": {
                "kind": "cli",
                "command": "rg",
                "workingDirectory": "."
            }
        },
        "operations": [
            {
                "id": "grep",
                "verb": "search",
                "input_schema": { "type": "object" },
                "output_schema": { "type": "object" },
                "cli": { "args": ["--json"] }
            }
        ]
    })
}

fn rest_descriptor() -> Value {
    json!({
        "schema_version": "praxec.tool/v1",
        "name": "httpbin",
        "version": "0.1.0",
        "kind": "rest",
        "reach": {
            "connection_name": "httpbin",
            "grant_as": "httpbin",
            "connection": {
                "kind": "rest",
                "baseUrl": "https://httpbin.org",
                "headers": {}
            },
            "auth": {
                "scheme": "header",
                "headers": ["X-Api-Key"]
            }
        },
        "operations": [
            {
                "id": "get-anything",
                "verb": "fetch",
                "input_schema": { "type": "object" },
                "output_schema": { "type": "object" },
                "rest": { "method": "GET", "path": "/anything" }
            }
        ]
    })
}

fn load(value: Value) -> Result<ToolDescriptor, ToolDescriptorError> {
    ToolDescriptor::load_str(&value.to_string())
}

// ── valid parse per kind ──────────────────────────────────────────────────

#[test]
fn valid_mcp_descriptor_parses() {
    let d = load(mcp_descriptor()).expect("valid mcp descriptor loads");
    assert_eq!(d.kind, ToolKind::Mcp);
    assert_eq!(d.name, "github-mcp");
    assert_eq!(d.reach.connection_name, "github");
    assert_eq!(d.operations.len(), 1);
    assert_eq!(d.operations[0].verb, Some(ScriptVerb::Search));
    assert_eq!(d.operations[0].mcp_tool.as_deref(), Some("search_issues"));
    let provision = d.provision.as_ref().expect("provision present");
    assert_eq!(
        provision.providers,
        vec![ProvisionProvider::Docker, ProvisionProvider::Release]
    );
    let auth = d.reach.auth.as_ref().expect("auth present");
    assert_eq!(auth.scheme, AuthScheme::Env);
    assert_eq!(auth.env, vec!["GITHUB_PERSONAL_ACCESS_TOKEN"]);
    assert_eq!(d.suggested_workflows, vec!["github/flow.triage-issues"]);
}

#[test]
fn valid_cli_descriptor_parses() {
    let d = load(cli_descriptor()).expect("valid cli descriptor loads");
    assert_eq!(d.kind, ToolKind::Cli);
    let cli = d.operations[0].cli.as_ref().expect("cli dispatch present");
    assert_eq!(cli.args, vec!["--json"]);
    assert!(d.provision.is_none(), "absent provision stays None");
}

#[test]
fn valid_rest_descriptor_parses() {
    let d = load(rest_descriptor()).expect("valid rest descriptor loads");
    assert_eq!(d.kind, ToolKind::Rest);
    let rest = d.operations[0]
        .rest
        .as_ref()
        .expect("rest dispatch present");
    assert_eq!(rest.method, "GET");
    assert_eq!(rest.path, "/anything");
}

// ── FM2 — kind / connection mismatch fails typed ──────────────────────────

#[test]
fn kind_connection_mismatch_fails_typed() {
    let mut doc = cli_descriptor();
    // cli descriptor, but the reach embeds an mcp connection.
    doc["reach"]["connection"] = json!({ "kind": "mcp", "command": "npx" });
    let err = load(doc).expect_err("kind mismatch must fail");
    assert!(
        matches!(
            &err,
            ToolDescriptorError::KindMismatch { kind: "cli", connection_kind } if connection_kind == "mcp"
        ),
        "expected KindMismatch, got: {err}"
    );
    assert!(err.to_string().contains("TOOL_KIND_MISMATCH"));
}

// ── operation dispatch coordinates match kind ─────────────────────────────

#[test]
fn foreign_dispatch_coordinate_fails_typed() {
    let mut doc = mcp_descriptor();
    // mcp descriptor whose operation also carries a cli coordinate.
    doc["operations"][0]["cli"] = json!({ "args": [] });
    let err = load(doc).expect_err("foreign dispatch must fail");
    assert!(
        matches!(&err, ToolDescriptorError::OperationDispatchMismatch { .. }),
        "expected OperationDispatchMismatch, got: {err}"
    );
    assert!(err.to_string().contains("TOOL_OPERATION_DISPATCH_MISMATCH"));
}

#[test]
fn missing_dispatch_coordinate_fails_typed() {
    let mut doc = rest_descriptor();
    doc["operations"][0]
        .as_object_mut()
        .expect("operation is an object")
        .remove("rest");
    let err = load(doc).expect_err("missing dispatch must fail");
    assert!(
        matches!(&err, ToolDescriptorError::OperationDispatchMismatch { .. }),
        "expected OperationDispatchMismatch, got: {err}"
    );
    assert!(err.to_string().contains("`rest` dispatch coordinate"));
}

#[test]
fn duplicate_operation_id_fails_typed() {
    let mut doc = cli_descriptor();
    let dup = doc["operations"][0].clone();
    doc["operations"]
        .as_array_mut()
        .expect("operations is an array")
        .push(dup);
    let err = load(doc).expect_err("duplicate op id must fail");
    assert!(
        matches!(&err, ToolDescriptorError::DuplicateOperationId(id) if id == "grep"),
        "expected DuplicateOperationId, got: {err}"
    );
}

// ── grant-token extraction — declares, never grants ───────────────────────

#[test]
fn grant_token_is_the_bare_grant_as_name() {
    let d = load(mcp_descriptor()).expect("valid descriptor loads");
    assert_eq!(d.grant_token(), "github");
    // The token is exactly reach.grant_as — the string the operator writes
    // in `grant_connections:`. Nothing else on the descriptor grants.
    assert_eq!(d.grant_token(), d.reach.grant_as);
}

// ── copy-never-transform: reach.connection round-trips gateway shape ──────

#[test]
fn reach_connection_round_trips_against_gateway_connection_shape() {
    for doc in [mcp_descriptor(), cli_descriptor(), rest_descriptor()] {
        let raw_connection = doc["reach"]["connection"].clone();
        let d = load(doc).expect("valid descriptor loads");
        // Verbatim: the parsed reach.connection is byte-for-byte the input
        // block (install = copy, never transform).
        assert_eq!(d.reach.connection, raw_connection);
        // And it validates standalone against the gateway config's
        // $defs/connection — i.e. it IS a legal /connections entry.
        validate_gateway_connection(&d.reach.connection)
            .expect("reach.connection is a valid gateway connection");
        // Serialize the whole descriptor back: the connection block is
        // still identical.
        let reserialized: Value = serde_json::to_value(&d).expect("descriptor serializes");
        assert_eq!(reserialized["reach"]["connection"], raw_connection);
    }
}

#[test]
fn gateway_connection_validator_rejects_non_connection_shapes() {
    let err = validate_gateway_connection(&json!({ "kind": "carrier-pigeon" }))
        .expect_err("unknown connection kind must fail");
    assert!(err.starts_with("connection:"), "got: {err}");
}

// ── schema gate: malformed descriptors fail before deserialize ────────────

#[test]
fn unknown_top_level_field_fails_schema() {
    let mut doc = cli_descriptor();
    doc["surprise"] = json!(true);
    let err = load(doc).expect_err("additionalProperties: false must reject");
    assert!(
        matches!(&err, ToolDescriptorError::Schema(_)),
        "expected Schema error, got: {err}"
    );
}

#[test]
fn wrong_schema_version_fails_schema() {
    let mut doc = cli_descriptor();
    doc["schema_version"] = json!("praxec.tool/v0");
    let err = load(doc).expect_err("schema_version const must reject");
    assert!(matches!(&err, ToolDescriptorError::Schema(_)));
}

#[test]
fn empty_operations_fails_schema() {
    let mut doc = cli_descriptor();
    doc["operations"] = json!([]);
    let err = load(doc).expect_err("minItems: 1 must reject");
    assert!(matches!(&err, ToolDescriptorError::Schema(_)));
}

#[test]
fn unknown_verb_fails_schema() {
    let mut doc = cli_descriptor();
    doc["operations"][0]["verb"] = json!("cogitate");
    let err = load(doc).expect_err("closed verb enum must reject");
    assert!(matches!(&err, ToolDescriptorError::Schema(_)));
}

// ── FM3 — auth is names-only; value-shaped fields fail ────────────────────

#[test]
fn value_bearing_auth_field_fails_schema() {
    let mut doc = mcp_descriptor();
    // A descriptor trying to smuggle a secret VALUE alongside the names.
    doc["reach"]["auth"]["values"] = json!({ "GITHUB_PERSONAL_ACCESS_TOKEN": "ghp_secret" });
    let err = load(doc).expect_err("authRequirement additionalProperties: false must reject");
    assert!(
        matches!(&err, ToolDescriptorError::Schema(_)),
        "expected Schema error, got: {err}"
    );
}

// ── loader ergonomics ─────────────────────────────────────────────────────

#[test]
fn load_file_reads_and_validates() {
    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().join("ripgrep.tool.json");
    std::fs::write(&path, cli_descriptor().to_string()).expect("write fixture");
    let d = ToolDescriptor::load_file(&path).expect("file loads");
    assert_eq!(d.name, "ripgrep");
}

#[test]
fn load_file_missing_path_fails_typed() {
    let err = ToolDescriptor::load_file(std::path::Path::new("/nonexistent/nope.tool.json"))
        .expect_err("missing file must fail");
    assert!(matches!(&err, ToolDescriptorError::Io { .. }));
    assert!(err.to_string().contains("TOOL_DESCRIPTOR_IO"));
}

#[test]
fn non_json_input_fails_typed() {
    let err = ToolDescriptor::load_str("kind: cli\n").expect_err("yaml is not json");
    assert!(matches!(&err, ToolDescriptorError::Parse(_)));
}
