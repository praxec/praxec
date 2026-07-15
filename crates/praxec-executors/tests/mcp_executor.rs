//! Behavioral coverage for `McpExecutor` via a settable stub at the
//! `McpToolCaller` seam — no real MCP server. The stub lets each test fix
//! the tool result (or a transport error) and records the call it received,
//! so we cover the executor's own logic: config validation, argument
//! mapping, idempotency-key injection, result shaping, and error
//! classification. The only thing NOT covered here is the real rmcp
//! transport construction (documented exclusion in `mcp.rs`).

use std::sync::Mutex;

use async_trait::async_trait;
use praxec_core::error::ExecutorError;
use praxec_core::model::{ExecuteRequest, WorkflowInstance};
use praxec_core::ports::Executor;
use praxec_executors::mcp::{McpExecutor, McpToolCaller};
use rmcp::model::{CallToolResult, Tool};
use serde_json::{Map, Value, json};

/// One recorded `call_tool` invocation.
#[derive(Clone)]
struct RecordedCall {
    connection: String,
    tool: String,
    arguments: Option<Map<String, Value>>,
}

enum StubBehavior {
    /// Return a result carrying this structured content + is_error flag.
    Structured { value: Value, is_error: bool },
    /// Fail as if the transport/connection broke.
    ConnectionError(String),
}

struct StubCaller {
    behavior: StubBehavior,
    calls: Mutex<Vec<RecordedCall>>,
}

impl StubCaller {
    fn ok(value: Value) -> Self {
        Self {
            behavior: StubBehavior::Structured {
                value,
                is_error: false,
            },
            calls: Mutex::new(Vec::new()),
        }
    }

    fn tool_error(value: Value) -> Self {
        Self {
            behavior: StubBehavior::Structured {
                value,
                is_error: true,
            },
            calls: Mutex::new(Vec::new()),
        }
    }

    fn connection_error(msg: &str) -> Self {
        Self {
            behavior: StubBehavior::ConnectionError(msg.into()),
            calls: Mutex::new(Vec::new()),
        }
    }

    fn calls(&self) -> Vec<RecordedCall> {
        self.calls.lock().unwrap().clone()
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
        self.calls.lock().unwrap().push(RecordedCall {
            connection: connection.into(),
            tool: tool.into(),
            arguments: arguments.clone(),
        });
        match &self.behavior {
            StubBehavior::Structured { value, is_error } => {
                // CallToolResult is #[non_exhaustive]: build via a constructor,
                // then attach the structured content.
                let mut result = if *is_error {
                    CallToolResult::error(vec![])
                } else {
                    CallToolResult::success(vec![])
                };
                result.structured_content = Some(value.clone());
                Ok(result)
            }
            StubBehavior::ConnectionError(msg) => Err(ExecutorError::Connection(msg.clone())),
        }
    }

    async fn list_remote_tools(&self, _connection: &str) -> Result<Vec<Tool>, ExecutorError> {
        Ok(vec![])
    }
}

fn instance() -> WorkflowInstance {
    WorkflowInstance {
        id: "wf_mcp".into(),
        definition_id: "demo".into(),
        definition_version: "1.0.0".into(),
        definition: json!({ "initialState": "s", "states": { "s": {} } }),
        state: "s".into(),
        version: 0,
        input: json!({}),
        context: json!({ "ticket": "T-1" }),
        started_at: chrono::Utc::now(),
        run_env: praxec_core::RunEnv::for_test(),
        cancelled_at: None,
        cancelled_reason: None,
        depth: 0,
        parent: None,
    }
}

fn request(executor_config: Value, idempotency_key: Option<&str>) -> ExecuteRequest {
    ExecuteRequest {
        workflow: instance(),
        transition: None,
        arguments: json!({ "raw": "value" }),
        executor_config,
        idempotency_key: idempotency_key.map(String::from),
        correlation_id: None,
    }
}

#[tokio::test]
async fn structured_content_becomes_the_output() {
    let stub = std::sync::Arc::new(StubCaller::ok(json!({ "ok": true, "n": 7 })));
    let exec = McpExecutor::with_caller(stub.clone());
    let result = exec
        .execute(request(
            json!({ "connection": "svc", "tool": "do_thing" }),
            None,
        ))
        .await
        .expect("success path");
    assert_eq!(result.output, json!({ "ok": true, "n": 7 }));
    let calls = stub.calls();
    assert_eq!(calls.len(), 1);
    assert_eq!(calls[0].connection, "svc");
    assert_eq!(calls[0].tool, "do_thing");
}

#[tokio::test]
async fn tool_is_error_flag_maps_to_permanent() {
    let stub = std::sync::Arc::new(StubCaller::tool_error(json!({ "message": "nope" })));
    let exec = McpExecutor::with_caller(stub);
    let err = exec
        .execute(request(json!({ "connection": "svc", "tool": "do" }), None))
        .await
        .expect_err("is_error must surface as an error");
    match err {
        ExecutorError::Permanent(msg) => assert!(msg.contains("do"), "names the tool: {msg}"),
        other => panic!("expected Permanent, got {other:?}"),
    }
}

#[tokio::test]
async fn transport_error_is_classified_not_swallowed() {
    let stub = std::sync::Arc::new(StubCaller::connection_error("socket reset"));
    let exec = McpExecutor::with_caller(stub);
    let err = exec
        .execute(request(json!({ "connection": "svc", "tool": "do" }), None))
        .await
        .expect_err("transport failure must surface");
    assert!(matches!(err, ExecutorError::Connection(_)), "got {err:?}");
}

#[tokio::test]
async fn missing_connection_config_fails_without_calling() {
    let stub = std::sync::Arc::new(StubCaller::ok(json!({})));
    let exec = McpExecutor::with_caller(stub.clone());
    let err = exec
        .execute(request(json!({ "tool": "do" }), None))
        .await
        .expect_err("missing connection must fail");
    assert!(matches!(err, ExecutorError::Permanent(_)));
    assert!(stub.calls().is_empty(), "caller must not be invoked");
}

#[tokio::test]
async fn missing_tool_config_fails_without_calling() {
    let stub = std::sync::Arc::new(StubCaller::ok(json!({})));
    let exec = McpExecutor::with_caller(stub.clone());
    let err = exec
        .execute(request(json!({ "connection": "svc" }), None))
        .await
        .expect_err("missing tool must fail");
    assert!(matches!(err, ExecutorError::Permanent(_)));
    assert!(stub.calls().is_empty(), "caller must not be invoked");
}

#[tokio::test]
async fn idempotency_key_is_injected_into_arguments() {
    let stub = std::sync::Arc::new(StubCaller::ok(json!({})));
    let exec = McpExecutor::with_caller(stub.clone());
    exec.execute(request(
        json!({ "connection": "svc", "tool": "do" }),
        Some("idem-123"),
    ))
    .await
    .expect("success");
    let args = stub.calls()[0]
        .arguments
        .clone()
        .expect("arguments present");
    assert_eq!(args.get("_idempotencyKey"), Some(&json!("idem-123")));
}

// --- pre-validate args against the connected tool's input schema ------------
// A deterministic kind:mcp call that passes an out-of-vocabulary value (e.g. an
// FMECA `domain` the engine doesn't model) must fail fast HERE, naming the
// server's own constraint, NOT deep in the server as a cryptic -32602.

struct SchemaStub {
    calls: Mutex<u32>,
}

#[async_trait]
impl McpToolCaller for SchemaStub {
    async fn call_tool(
        &self,
        _connection: &str,
        _tool: &str,
        _arguments: Option<Map<String, Value>>,
    ) -> Result<CallToolResult, ExecutorError> {
        *self.calls.lock().unwrap() += 1;
        Ok(CallToolResult::success(vec![]))
    }

    async fn list_remote_tools(&self, _connection: &str) -> Result<Vec<Tool>, ExecutorError> {
        let schema: Map<String, Value> = serde_json::from_value(json!({
            "type": "object",
            "properties": { "domain": { "type": "string", "enum": ["ux", "runtime"] } },
            "required": ["domain"]
        }))
        .unwrap();
        Ok(vec![Tool::new("analyze", "", std::sync::Arc::new(schema))])
    }
}

fn req_with_args(args: Value) -> ExecuteRequest {
    ExecuteRequest {
        workflow: instance(),
        transition: None,
        arguments: args,
        executor_config: json!({ "connection": "fmeca", "tool": "analyze" }),
        idempotency_key: None,
        correlation_id: None,
    }
}

#[tokio::test]
async fn out_of_schema_arg_fails_fast_before_the_call() {
    let stub = std::sync::Arc::new(SchemaStub {
        calls: Mutex::new(0),
    });
    let exec = McpExecutor::with_caller(stub.clone());
    let err = exec
        .execute(req_with_args(json!({ "domain": "security" })))
        .await
        .expect_err("an out-of-enum domain must be rejected before the call");
    assert!(format!("{err:?}").contains("MCP_ARG_INVALID"), "{err:?}");
    assert_eq!(
        *stub.calls.lock().unwrap(),
        0,
        "the tool must NOT be invoked when args fail its schema"
    );
}

#[tokio::test]
async fn in_schema_arg_passes_validation_and_calls_through() {
    let stub = std::sync::Arc::new(SchemaStub {
        calls: Mutex::new(0),
    });
    let exec = McpExecutor::with_caller(stub.clone());
    exec.execute(req_with_args(json!({ "domain": "ux" })))
        .await
        .expect("valid args call through");
    assert_eq!(*stub.calls.lock().unwrap(), 1);
}
