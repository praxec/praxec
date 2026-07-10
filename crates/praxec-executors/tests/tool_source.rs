//! D2 — tool-source executor behavior.
//!
//! A D1 descriptor's `operations[]` become callable by delegating to the
//! existing `cli` / `mcp` / `rest` executors. Coverage: descriptor→callable
//! happy path per kind (mcp via the `McpToolCaller` stub seam, cli via a
//! real POSIX command, rest via wiremock — the same strategies those
//! executors' own tests use), the connection gate (absent → fail-fast
//! naming the operator acts, never an auto-grant; ungranted → typed
//! `UNGRANTED_PACK_CONNECTION` with the grant remedy; kind mismatch →
//! typed), operation resolution (unknown id → typed, listing what exists;
//! dispatch-coordinate mismatch → D1's typed loader error surfaces), and
//! the input_schema gate (bad arguments never reach the transport).

use std::io::Write;
use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use praxec_core::error::ExecutorError;
use praxec_core::model::{ExecuteRequest, WorkflowInstance};
use praxec_core::ports::Executor;
use praxec_executors::mcp::{McpExecutor, McpToolCaller};
use praxec_executors::tool_source::ToolSourceExecutor;
use praxec_executors::{
    CliConnections, CliExecutor, McpConnections, RestConnections, RestExecutor,
};
use rmcp::model::{CallToolResult, Tool};
use serde_json::{Map, Value, json};
use wiremock::matchers::{body_json, method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

// ── harness ───────────────────────────────────────────────────────────────

fn instance() -> WorkflowInstance {
    WorkflowInstance {
        id: "wf_tool_source".into(),
        definition_id: "demo".into(),
        definition_version: "1.0.0".into(),
        definition: json!({ "initialState": "s", "states": { "s": {} } }),
        state: "s".into(),
        version: 0,
        input: json!({}),
        context: json!({}),
        started_at: chrono::Utc::now(),
        trace_id: None,
        run_id: None,
        cancelled_at: None,
        cancelled_reason: None,
        depth: 0,
        parent: None,
    }
}

fn req(cfg: Value, arguments: Value) -> ExecuteRequest {
    ExecuteRequest {
        workflow: instance(),
        transition: None,
        arguments,
        executor_config: cfg,
        idempotency_key: None,
        correlation_id: None,
    }
}

/// Build a `ToolSourceExecutor` wired exactly as the default registry wires
/// it — real cli/rest delegates from the same config — with an injectable
/// mcp delegate (the stub seam).
fn tool_source_with_mcp(config: &Value, mcp: Arc<dyn Executor>) -> ToolSourceExecutor {
    ToolSourceExecutor::new(
        config,
        Arc::new(CliExecutor::new(Arc::new(CliConnections::from_config(
            config,
        )))),
        mcp,
        Arc::new(RestExecutor::new(Arc::new(RestConnections::from_config(
            config,
        )))),
    )
}

fn tool_source(config: &Value) -> ToolSourceExecutor {
    let mcp = Arc::new(McpExecutor::new(McpConnections::from_config(config)));
    tool_source_with_mcp(config, mcp)
}

// ── mcp stub at the McpToolCaller seam (as mcp_executor.rs does) ──────────

#[derive(Clone)]
struct RecordedCall {
    connection: String,
    tool: String,
    arguments: Option<Map<String, Value>>,
}

struct StubCaller {
    result: Value,
    calls: Mutex<Vec<RecordedCall>>,
}

impl StubCaller {
    fn ok(result: Value) -> Arc<Self> {
        Arc::new(Self {
            result,
            calls: Mutex::new(Vec::new()),
        })
    }

    fn calls(&self) -> Vec<RecordedCall> {
        self.calls.lock().expect("stub lock").clone()
    }
}

#[async_trait]
impl McpToolCaller for StubCaller {
    async fn call_tool(
        &self,
        connection: &str,
        tool: &str,
        arguments: Option<Map<String, Value>>,
    ) -> Result<CallToolResult, ExecutorError> {
        self.calls.lock().expect("stub lock").push(RecordedCall {
            connection: connection.to_string(),
            tool: tool.to_string(),
            arguments,
        });
        Ok(CallToolResult::structured(self.result.clone()))
    }

    async fn list_remote_tools(&self, _connection: &str) -> Result<Vec<Tool>, ExecutorError> {
        Ok(vec![])
    }
}

// ── descriptor fixtures (same shapes D1's own tests use) ─────────────────

fn mcp_descriptor() -> Value {
    json!({
        "schema_version": "praxec.tool/v1",
        "name": "github-mcp",
        "version": "1.2.0",
        "kind": "mcp",
        "reach": {
            "connection_name": "github",
            "grant_as": "github",
            "connection": { "kind": "mcp", "command": "github-mcp-server" }
        },
        "operations": [
            {
                "id": "search-issues",
                "verb": "search",
                "input_schema": {
                    "type": "object",
                    "required": ["q"],
                    "properties": { "q": { "type": "string" } },
                    "additionalProperties": false
                },
                "output_schema": { "type": "object" },
                "mcp_tool": "search_issues"
            }
        ]
    })
}

fn cli_descriptor() -> Value {
    json!({
        "schema_version": "praxec.tool/v1",
        "name": "printer",
        "version": "0.1.0",
        "kind": "cli",
        "reach": {
            "connection_name": "printer",
            "grant_as": "printer",
            "connection": { "kind": "cli", "command": "printf" }
        },
        "operations": [
            {
                "id": "print",
                "verb": "run",
                "input_schema": {
                    "type": "object",
                    "required": ["name"],
                    "properties": { "name": { "type": "string" } }
                },
                "output_schema": { "type": "object" },
                "cli": { "args": ["ok:%s", "$.arguments.name"] }
            }
        ]
    })
}

fn rest_descriptor() -> Value {
    json!({
        "schema_version": "praxec.tool/v1",
        "name": "things-api",
        "version": "0.1.0",
        "kind": "rest",
        "reach": {
            "connection_name": "things",
            "grant_as": "things",
            "connection": { "kind": "rest", "baseUrl": "https://example.invalid" }
        },
        "operations": [
            {
                "id": "create-thing",
                "verb": "run",
                "input_schema": {
                    "type": "object",
                    "required": ["id", "title"],
                    "properties": {
                        "id": { "type": "string" },
                        "title": { "type": "string" }
                    }
                },
                "output_schema": { "type": "object" },
                "rest": { "method": "POST", "path": "/things/{id}" }
            },
            {
                "id": "get-thing",
                "verb": "fetch",
                "input_schema": { "type": "object" },
                "output_schema": { "type": "object" },
                "rest": { "method": "GET", "path": "/things/{id}" }
            }
        ]
    })
}

// ── happy path per kind ───────────────────────────────────────────────────

#[tokio::test]
async fn mcp_descriptor_operation_dispatches_through_the_mcp_executor() {
    let config = json!({
        "connections": { "github": { "kind": "mcp", "command": "github-mcp-server" } }
    });
    let stub = StubCaller::ok(json!({ "issues": [1, 2] }));
    let mcp = Arc::new(McpExecutor::with_caller(stub.clone()));
    let executor = tool_source_with_mcp(&config, mcp);

    let out = executor
        .execute(req(
            json!({
                "kind": "tool_source",
                "descriptor": mcp_descriptor(),
                "operation": "search-issues"
            }),
            json!({ "q": "bug" }),
        ))
        .await
        .expect("mcp operation dispatches");

    assert_eq!(out.output, json!({ "issues": [1, 2] }));
    let calls = stub.calls();
    assert_eq!(calls.len(), 1, "exactly one tool call");
    assert_eq!(calls[0].connection, "github");
    assert_eq!(calls[0].tool, "search_issues");
    let args = calls[0].arguments.clone().expect("arguments forwarded");
    assert_eq!(args.get("q"), Some(&json!("bug")));
}

#[tokio::test]
async fn cli_descriptor_operation_runs_through_the_cli_executor_from_a_file() {
    let config = json!({
        "connections": { "printer": { "kind": "cli", "command": "printf" } }
    });
    // Load via `descriptor_path` to cover D1's file loader end-to-end.
    let mut file = tempfile::NamedTempFile::new().expect("tempfile");
    file.write_all(cli_descriptor().to_string().as_bytes())
        .expect("write descriptor");
    let executor = tool_source(&config);

    let out = executor
        .execute(req(
            json!({
                "kind": "tool_source",
                "descriptor_path": file.path().to_string_lossy(),
                "operation": "print"
            }),
            json!({ "name": "world" }),
        ))
        .await
        .expect("cli operation runs");

    assert_eq!(out.output["stdout"], json!("ok:world"));
    assert_eq!(out.output["success"], json!(true));
}

#[tokio::test]
async fn rest_descriptor_post_operation_interpolates_path_and_sends_arguments_as_body() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/things/42"))
        .and(body_json(json!({ "id": "42", "title": "a thing" })))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({ "ok": true })))
        .expect(1)
        .mount(&server)
        .await;

    let config = json!({
        "connections": { "things": { "kind": "rest", "baseUrl": server.uri() } }
    });
    let executor = tool_source(&config);

    let out = executor
        .execute(req(
            json!({
                "kind": "tool_source",
                "descriptor": rest_descriptor(),
                "operation": "create-thing"
            }),
            json!({ "id": "42", "title": "a thing" }),
        ))
        .await
        .expect("rest operation dispatches");

    assert_eq!(out.output["status"], json!(200));
    assert_eq!(out.output["body"], json!({ "ok": true }));
}

#[tokio::test]
async fn rest_descriptor_get_operation_interpolates_path_and_sends_no_body() {
    let server = MockServer::start().await;
    // wiremock matches GET /things/7 with any (empty) body.
    Mock::given(method("GET"))
        .and(path("/things/7"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({ "id": "7" })))
        .expect(1)
        .mount(&server)
        .await;

    let config = json!({
        "connections": { "things": { "kind": "rest", "baseUrl": server.uri() } }
    });
    let executor = tool_source(&config);

    let out = executor
        .execute(req(
            json!({
                "kind": "tool_source",
                "descriptor": rest_descriptor(),
                "operation": "get-thing"
            }),
            json!({ "id": "7" }),
        ))
        .await
        .expect("rest GET dispatches");

    assert_eq!(out.output["status"], json!(200));
}

// ── connection gate: fail-fast, never auto-grant ─────────────────────────

#[tokio::test]
async fn absent_connection_fails_fast_naming_the_operator_acts() {
    // No live connections at all — the descriptor declares reach, but
    // install + grant are operator acts; the executor must never perform them.
    let config = json!({ "connections": {} });
    let executor = tool_source(&config);

    let err = executor
        .execute(req(
            json!({
                "kind": "tool_source",
                "descriptor": mcp_descriptor(),
                "operation": "search-issues"
            }),
            json!({ "q": "bug" }),
        ))
        .await
        .expect_err("absent connection must fail fast");

    let msg = format!("{err:?}");
    assert!(msg.contains("TOOL_SOURCE_CONNECTION_ABSENT"), "msg: {msg}");
    assert!(msg.contains("'github'"), "names the connection: {msg}");
    assert!(
        msg.contains("grant_connections"),
        "points at the grant act: {msg}"
    );
    assert!(
        msg.contains("never auto-installs or auto-grants"),
        "states the boundary: {msg}"
    );
}

#[tokio::test]
async fn ungranted_pack_connection_fails_typed_with_the_grant_remedy() {
    // SPEC §9.5 — the connection is declared by a pack but diverted to the
    // ungranted stamp; the D3 gate's typed error (with the exact remedy)
    // must surface through tool_source unchanged.
    let config = json!({
        "connections": {},
        "praxec": {
            "_ungrantedConnections": {
                "github": {
                    "repo": "tool-pack",
                    "namespace": "packns",
                    "remedy": "add `grant_connections: [github]` to the `repos:` entry \
                               for tool-pack to activate this connection"
                }
            }
        }
    });
    let executor = tool_source(&config);

    let err = executor
        .execute(req(
            json!({
                "kind": "tool_source",
                "descriptor": mcp_descriptor(),
                "operation": "search-issues"
            }),
            json!({ "q": "bug" }),
        ))
        .await
        .expect_err("ungranted connection must not be reachable");

    let msg = format!("{err:?}");
    assert!(msg.contains("UNGRANTED_PACK_CONNECTION"), "msg: {msg}");
    assert!(msg.contains("tool-pack"), "names the pack: {msg}");
    assert!(msg.contains("grant_connections"), "carries remedy: {msg}");
}

#[tokio::test]
async fn live_connection_of_the_wrong_kind_fails_typed() {
    // A live connection exists under the descriptor's name — but as a cli
    // connection while the descriptor is kind: mcp. No silent cross-kind
    // dispatch.
    let config = json!({
        "connections": { "github": { "kind": "cli", "command": "gh" } }
    });
    let executor = tool_source(&config);

    let err = executor
        .execute(req(
            json!({
                "kind": "tool_source",
                "descriptor": mcp_descriptor(),
                "operation": "search-issues"
            }),
            json!({ "q": "bug" }),
        ))
        .await
        .expect_err("kind mismatch must fail");

    let msg = format!("{err:?}");
    assert!(
        msg.contains("TOOL_SOURCE_CONNECTION_KIND_MISMATCH"),
        "msg: {msg}"
    );
    assert!(msg.contains("`kind: cli`"), "names the live kind: {msg}");
}

// ── operation resolution + dispatch mismatch ─────────────────────────────

#[tokio::test]
async fn unknown_operation_fails_typed_listing_what_exists() {
    let config = json!({
        "connections": { "github": { "kind": "mcp", "command": "github-mcp-server" } }
    });
    let executor = tool_source(&config);

    let err = executor
        .execute(req(
            json!({
                "kind": "tool_source",
                "descriptor": mcp_descriptor(),
                "operation": "close-issue"
            }),
            json!({}),
        ))
        .await
        .expect_err("unknown operation must fail");

    let msg = format!("{err:?}");
    assert!(msg.contains("TOOL_SOURCE_UNKNOWN_OPERATION"), "msg: {msg}");
    assert!(
        msg.contains("search-issues"),
        "lists available operations: {msg}"
    );
}

#[tokio::test]
async fn operation_dispatch_coordinate_mismatch_surfaces_the_d1_typed_error() {
    // An mcp descriptor whose operation carries a `rest` coordinate is
    // rejected by D1's loader — tool_source surfaces that typed error, it
    // never re-parses or repairs.
    let mut descriptor = mcp_descriptor();
    descriptor["operations"][0]["rest"] = json!({ "method": "GET", "path": "/x" });
    let config = json!({
        "connections": { "github": { "kind": "mcp", "command": "github-mcp-server" } }
    });
    let executor = tool_source(&config);

    let err = executor
        .execute(req(
            json!({
                "kind": "tool_source",
                "descriptor": descriptor,
                "operation": "search-issues"
            }),
            json!({ "q": "bug" }),
        ))
        .await
        .expect_err("dispatch mismatch must fail at load");

    let msg = format!("{err:?}");
    assert!(
        msg.contains("TOOL_OPERATION_DISPATCH_MISMATCH") || msg.contains("TOOL_DESCRIPTOR_SCHEMA"),
        "msg: {msg}"
    );
}

// ── input contract + config shape ─────────────────────────────────────────

#[tokio::test]
async fn arguments_violating_the_input_schema_never_reach_the_transport() {
    let config = json!({
        "connections": { "github": { "kind": "mcp", "command": "github-mcp-server" } }
    });
    let stub = StubCaller::ok(json!({ "never": "called" }));
    let mcp = Arc::new(McpExecutor::with_caller(stub.clone()));
    let executor = tool_source_with_mcp(&config, mcp);

    let err = executor
        .execute(req(
            json!({
                "kind": "tool_source",
                "descriptor": mcp_descriptor(),
                "operation": "search-issues"
            }),
            json!({ "wrong_field": 1 }),
        ))
        .await
        .expect_err("schema-invalid arguments must fail before the call");

    let msg = format!("{err:?}");
    assert!(msg.contains("TOOL_SOURCE_ARG_INVALID"), "msg: {msg}");
    assert!(
        stub.calls().is_empty(),
        "the transport must never see invalid arguments"
    );
}

#[tokio::test]
async fn descriptor_source_must_be_exactly_one_of_inline_or_path() {
    let config = json!({
        "connections": { "github": { "kind": "mcp", "command": "github-mcp-server" } }
    });
    let executor = tool_source(&config);

    // Neither source.
    let err = executor
        .execute(req(
            json!({ "kind": "tool_source", "operation": "search-issues" }),
            json!({}),
        ))
        .await
        .expect_err("missing descriptor source must fail");
    assert!(format!("{err:?}").contains("TOOL_SOURCE_CONFIG"));

    // Both sources.
    let err = executor
        .execute(req(
            json!({
                "kind": "tool_source",
                "descriptor": mcp_descriptor(),
                "descriptor_path": "/tmp/whatever.json",
                "operation": "search-issues"
            }),
            json!({}),
        ))
        .await
        .expect_err("ambiguous descriptor source must fail");
    assert!(format!("{err:?}").contains("TOOL_SOURCE_CONFIG"));
}

#[tokio::test]
async fn missing_operation_key_is_a_config_error() {
    let config = json!({
        "connections": { "github": { "kind": "mcp", "command": "github-mcp-server" } }
    });
    let executor = tool_source(&config);

    let err = executor
        .execute(req(
            json!({ "kind": "tool_source", "descriptor": mcp_descriptor() }),
            json!({}),
        ))
        .await
        .expect_err("missing `operation` must fail");
    let msg = format!("{err:?}");
    assert!(msg.contains("TOOL_SOURCE_CONFIG"), "msg: {msg}");
    assert!(msg.contains("operation"), "names the missing key: {msg}");
}

// ── registry wiring ───────────────────────────────────────────────────────

#[test]
fn tool_source_is_wired_into_the_default_registry() {
    let registry = praxec_executors::default_registry(&json!({}));
    assert!(
        registry.get("tool_source").is_some(),
        "D2: `tool_source` must resolve from the production registry shape"
    );
}
