//! CMP-034 — `mcp` executor `map:` binding resolution.
//!
//! A PRESENT-but-unresolvable `map:` binding must fail-fast rather than
//! silently fall back to the raw `arguments` (which would call the downstream
//! tool with the wrong inputs). These tests exercise the error path, which
//! triggers BEFORE any MCP connection is opened — so no live server is needed.

use chrono::Utc;
use praxec_core::model::{ExecuteRequest, WorkflowInstance};
use praxec_core::ports::Executor;
use praxec_executors::{McpConnections, McpExecutor};
use serde_json::{Value, json};

fn instance() -> WorkflowInstance {
    WorkflowInstance {
        id: "wf_map".into(),
        definition_id: "demo".into(),
        definition_version: "0".into(),
        definition: Value::Null,
        state: "running".into(),
        version: 0,
        input: json!({}),
        context: json!({}),
        started_at: Utc::now(),
        run_env: praxec_core::RunEnv::for_test(),
        cancelled_at: None,
        cancelled_reason: None,
        depth: 0,
        parent: None,
    }
}

fn req(executor_config: Value, arguments: Value) -> ExecuteRequest {
    ExecuteRequest {
        workflow: instance(),
        transition: Some("go".into()),
        arguments,
        executor_config,
        idempotency_key: None,
        correlation_id: None,
    }
}

// ── Present `map:` with an unresolvable binding → fail-fast ──────────────────

#[tokio::test]
async fn present_map_with_unresolvable_binding_errors() {
    let exec = McpExecutor::new(McpConnections::default());
    // `map:` is present and references `$.context.missing`, which is absent
    // from every scope. Old behavior: silently pass raw `arguments`. New
    // behavior: MCP_MAP_BINDING_UNRESOLVED.
    let err = exec
        .execute(req(
            json!({
                "kind": "mcp",
                "connection": "anything",
                "tool": "do_thing",
                "map": { "symbol": "$.context.missing" }
            }),
            json!({ "raw": "should-not-be-used" }),
        ))
        .await
        .expect_err("unresolvable map binding must error, not fall back to raw args");
    let s = format!("{err:?}");
    assert!(s.contains("MCP_MAP_BINDING_UNRESOLVED"), "got: {s}");
}

// ── Literal + nested bindings are templated, not rejected ────────────────────
//
// `map:` is a template: a `$.` string is a scope path, everything else is a
// literal, and objects/arrays are walked. So a literal scalar (and a nested
// object mixing a path with a literal) must pass render_args cleanly — it gets
// as far as the connection (which fails here, no live server), but NEVER with a
// map-binding error. This pins the lifted limitation (was INVALID_MCP_MAP).

#[tokio::test]
async fn literal_and_nested_map_bindings_are_templated_not_rejected() {
    let exec = McpExecutor::new(McpConnections::default());
    let result = exec
        .execute(req(
            json!({
                "kind": "mcp",
                "connection": "anything",
                "tool": "do_thing",
                "map": {
                    "count": 42,
                    "params": { "max_groups": 5, "mode": "fast" }
                }
            }),
            json!({}),
        ))
        .await;
    if let Err(e) = result {
        let s = format!("{e:?}");
        assert!(
            !s.contains("INVALID_MCP_MAP") && !s.contains("MCP_MAP_BINDING_UNRESOLVED"),
            "literal + nested bindings must template cleanly; got: {s}"
        );
    }
}

// ── No `map:` declared → raw-args pass-through (no map error) ─────────────────
//
// With no `map:`, render_args returns Ok(None) and the executor passes the raw
// arguments through. There's no live connection, so the call ultimately fails
// at connection time — but it must NOT fail with a map-binding error. This
// distinguishes the legit pass-through case from the broken-binding case.

#[tokio::test]
async fn no_map_does_not_emit_map_binding_error() {
    let exec = McpExecutor::new(McpConnections::default());
    let result = exec
        .execute(req(
            json!({
                "kind": "mcp",
                "connection": "anything",
                "tool": "do_thing"
            }),
            json!({ "raw": "passed-through" }),
        ))
        .await;
    if let Err(e) = result {
        let s = format!("{e:?}");
        assert!(
            !s.contains("MCP_MAP_BINDING_UNRESOLVED") && !s.contains("INVALID_MCP_MAP"),
            "no-map path must not raise a map-binding error; got: {s}"
        );
    }
}
