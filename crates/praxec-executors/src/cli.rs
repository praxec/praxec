use std::collections::HashMap;
use std::sync::Arc;

use async_trait::async_trait;
use praxec_core::error::ExecutorError;
use praxec_core::model::{Evidence, ExecuteRequest, ExecuteResult};
use praxec_core::ports::Executor;
use serde_json::{Value, json};
use tokio::process::Command;
use uuid::Uuid;

/// Connection definitions parsed from the `connections:` block, keyed by name.
/// Used to resolve `executor.connection: foo` references at execute time.
#[derive(Default, Clone)]
pub struct CliConnections {
    inner: HashMap<String, CliConnection>,
}

#[derive(Debug, Clone)]
pub struct CliConnection {
    pub command: String,
    pub working_directory: Option<String>,
    pub env: HashMap<String, String>,
}

impl CliConnections {
    pub fn from_config(config: &Value) -> Self {
        let mut inner = HashMap::new();
        if let Some(map) = config.pointer("/connections").and_then(Value::as_object) {
            for (name, conn) in map {
                if conn.get("kind").and_then(Value::as_str) == Some("cli") {
                    let command = conn
                        .get("command")
                        .and_then(Value::as_str)
                        .unwrap_or_default()
                        .to_string();
                    let working_directory = conn
                        .get("workingDirectory")
                        .and_then(Value::as_str)
                        .map(str::to_string);
                    let env = crate::conn_util::json_object_to_string_map(conn.get("env"));
                    inner.insert(
                        name.clone(),
                        CliConnection {
                            command,
                            working_directory,
                            env,
                        },
                    );
                }
            }
        }
        Self { inner }
    }
}

pub struct CliExecutor {
    connections: Arc<CliConnections>,
}

impl CliExecutor {
    pub fn new(connections: Arc<CliConnections>) -> Self {
        Self { connections }
    }
}

#[async_trait]
impl Executor for CliExecutor {
    async fn execute(&self, request: ExecuteRequest) -> Result<ExecuteResult, ExecutorError> {
        let cfg = &request.executor_config;

        // Resolve command + working dir + env from either the connection or inline overrides.
        let connection_name = cfg.get("connection").and_then(Value::as_str);
        let connection = connection_name.and_then(|n| self.connections.inner.get(n));

        let command = cfg
            .get("command")
            .and_then(Value::as_str)
            .map(str::to_string)
            .or_else(|| connection.map(|c| c.command.clone()))
            .ok_or_else(|| {
                ExecutorError::Permanent(
                    "cli executor requires a command (inline or via connection)".into(),
                )
            })?;

        let raw_args = cfg
            .get("args")
            .and_then(Value::as_array)
            .cloned()
            .unwrap_or_default();

        let mut cmd = Command::new(&command);
        for arg in &raw_args {
            cmd.arg(crate::arg_render::render_arg(arg, &request)?);
        }

        if let Some(c) = connection {
            for (k, v) in &c.env {
                cmd.env(k, v);
            }
            if let Some(wd) = &c.working_directory {
                cmd.current_dir(wd);
            }
        }
        if let Some(extra) = cfg.get("env").and_then(Value::as_object) {
            for (k, v) in extra {
                if let Some(s) = v.as_str() {
                    cmd.env(k, s);
                }
            }
        }
        // Idempotency key, when configured, becomes IDEMPOTENCY_KEY in the
        // child process environment.
        if let Some(key) = &request.idempotency_key {
            cmd.env("IDEMPOTENCY_KEY", key);
        }

        let output = cmd
            .output()
            .await
            .map_err(|e| ExecutorError::Connection(format!("spawn failed: {e}")))?;

        let stdout_str = String::from_utf8_lossy(&output.stdout).to_string();
        // Auto-parse stdout as JSON when it parses cleanly. This makes
        // structured output from wrapper scripts available under
        // `$.output.json` so workflows can read counts, status fields,
        // arrays, etc. without needing a separate parsing step. When
        // stdout isn't JSON, `json` is null (existing string `stdout`
        // remains the source of truth).
        let parsed_json: Value = serde_json::from_str(stdout_str.trim()).unwrap_or(Value::Null);

        let result = json!({
            "exitCode": output.status.code(),
            "success": output.status.success(),
            "stdout": stdout_str,
            "stderr": String::from_utf8_lossy(&output.stderr).to_string(),
            "json": parsed_json,
        });

        // By default a non-zero exit becomes a failed transition, which is
        // the right default for "run X or fail loud." For TDD-style flows
        // where exit code IS the data ("did the test fail as expected?"),
        // set `treatNonZeroAsFailure: false` and read `output.success`
        // from the workflow.
        let treat_nonzero_as_failure = cfg
            .get("treatNonZeroAsFailure")
            .and_then(Value::as_bool)
            .unwrap_or(true);

        if treat_nonzero_as_failure && !output.status.success() {
            return Err(ExecutorError::Permanent(format!(
                "cli '{}' exited with code {:?}",
                command,
                output.status.code()
            )));
        }

        Ok(ExecuteResult {
            output: result,
            evidence: vec![Evidence {
                kind: "cli_output".to_string(),
                id: Uuid::new_v4().to_string(),
                uri: None,
                summary: Some(format!("Executed '{}'", command)),
                digest: None,
                confidence: None,
            }],
            child_workflow_id: None,
            next_transition: None,
            suspend: None,
            telemetry: None,
        })
    }
}
