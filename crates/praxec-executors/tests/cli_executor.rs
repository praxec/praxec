//! Behavioral coverage for `CliExecutor`. The executor runs real
//! subprocesses, so these drive it with deterministic POSIX commands
//! (`printf`, `false`, `sh -c …`) — no mock needed, and every branch
//! (success, JSON parse, non-zero exit, the `treatNonZeroAsFailure`
//! override, missing-command config, spawn failure, idempotency-key env)
//! is asserted from observable output.

use std::sync::Arc;

use praxec_core::error::ExecutorError;
use praxec_core::model::{ExecuteRequest, WorkflowInstance};
use praxec_core::ports::Executor;
use praxec_executors::{CliConnections, CliExecutor};
use serde_json::{Value, json};

fn executor() -> CliExecutor {
    CliExecutor::new(Arc::new(CliConnections::from_config(&json!({}))))
}

fn instance() -> WorkflowInstance {
    WorkflowInstance {
        id: "wf_cli".into(),
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

fn req(cfg: Value, idempotency_key: Option<&str>) -> ExecuteRequest {
    ExecuteRequest {
        workflow: instance(),
        transition: None,
        arguments: json!({}),
        executor_config: cfg,
        idempotency_key: idempotency_key.map(String::from),
        correlation_id: None,
    }
}

#[tokio::test]
async fn json_stdout_is_auto_parsed_into_output_json() {
    let out = executor()
        .execute(req(
            json!({ "command": "printf", "args": ["{\"k\":7}"] }),
            None,
        ))
        .await
        .expect("printf succeeds");
    assert_eq!(out.output["json"], json!({ "k": 7 }));
    assert_eq!(out.output["success"], json!(true));
    assert_eq!(out.output["exitCode"], json!(0));
}

#[tokio::test]
async fn non_json_stdout_leaves_json_null() {
    let out = executor()
        .execute(req(json!({ "command": "printf", "args": ["hello"] }), None))
        .await
        .expect("printf succeeds");
    assert_eq!(out.output["json"], Value::Null);
    assert_eq!(out.output["stdout"], json!("hello"));
}

#[tokio::test]
async fn nonzero_exit_is_permanent_by_default() {
    let err = executor()
        .execute(req(json!({ "command": "false" }), None))
        .await
        .expect_err("`false` exits non-zero");
    match err {
        ExecutorError::Permanent(msg) => assert!(msg.contains("false"), "names cmd: {msg}"),
        other => panic!("expected Permanent, got {other:?}"),
    }
}

#[tokio::test]
async fn treat_nonzero_as_failure_false_succeeds_with_success_flag() {
    let out = executor()
        .execute(req(
            json!({ "command": "false", "treatNonZeroAsFailure": false }),
            None,
        ))
        .await
        .expect("override makes non-zero a success");
    assert_eq!(out.output["success"], json!(false));
    assert_eq!(out.output["exitCode"], json!(1));
}

#[tokio::test]
async fn missing_command_is_permanent() {
    let err = executor()
        .execute(req(json!({ "args": ["x"] }), None))
        .await
        .expect_err("no command anywhere");
    assert!(matches!(err, ExecutorError::Permanent(_)));
}

#[tokio::test]
async fn spawn_failure_is_connection() {
    let err = executor()
        .execute(req(
            json!({ "command": "this-binary-does-not-exist-praxec" }),
            None,
        ))
        .await
        .expect_err("nonexistent binary cannot spawn");
    assert!(matches!(err, ExecutorError::Connection(_)), "got {err:?}");
}

#[tokio::test]
async fn idempotency_key_is_exported_as_env() {
    let out = executor()
        .execute(req(
            json!({ "command": "sh", "args": ["-c", "printf %s \"$IDEMPOTENCY_KEY\""] }),
            Some("idem-xyz"),
        ))
        .await
        .expect("sh succeeds");
    assert_eq!(out.output["stdout"], json!("idem-xyz"));
}

/// SPEC §9.5 — a cli executor naming a pack connection the operator never
/// granted must fail typed with the grant remedy, EVEN when an inline
/// `command` would otherwise let execution proceed. Silently running without
/// the connection's env/working-directory would be a fallback across the
/// trust boundary.
#[tokio::test]
async fn ungranted_pack_connection_fails_typed_even_with_inline_command() {
    let config = json!({
        "connections": {},
        "praxec": {
            "_ungrantedConnections": {
                "packns/gh-cli": {
                    "repo": "conn-pack",
                    "namespace": "packns",
                    "remedy": "add `grant_connections: [gh-cli]` to the `repos:` entry \
                               for conn-pack to activate this connection"
                }
            }
        }
    });
    let executor = CliExecutor::new(Arc::new(CliConnections::from_config(&config)));
    let err = executor
        .execute(req(
            json!({ "connection": "packns/gh-cli", "command": "printf", "args": ["hi"] }),
            None,
        ))
        .await
        .expect_err("ungranted connection must fail, not silently fall back to inline command");
    let msg = format!("{err:?}");
    assert!(msg.contains("UNGRANTED_PACK_CONNECTION"), "msg: {msg}");
    assert!(msg.contains("conn-pack"), "msg names the pack: {msg}");
    assert!(
        msg.contains("grant_connections"),
        "msg carries the remedy: {msg}"
    );
}

/// Back-compat: an unknown (not ungranted) connection name with an inline
/// command keeps today's behavior — the inline command runs.
#[tokio::test]
async fn unknown_connection_with_inline_command_still_runs_inline() {
    let out = executor()
        .execute(req(
            json!({ "connection": "not-declared", "command": "printf", "args": ["ok"] }),
            None,
        ))
        .await
        .expect("inline command still runs for a merely-unknown connection");
    assert_eq!(out.output["stdout"], json!("ok"));
}
