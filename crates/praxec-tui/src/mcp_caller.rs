//! Production `McpToolCaller` impl — wraps an rmcp client connected to
//! a `praxec` child process via stdio.
//!
//! The interpreter (`walk_workflow`) is the consumer; it only ever
//! issues `workflow.get` and `workflow.submit` against this caller.
//! `workflow.start` happens once at the binary entry point to acquire
//! a `workflowId`, and is exposed as a free function rather than
//! through the trait so the interpreter contract stays minimal.
//!
//! ## Lifecycle
//!
//! - Construct via [`PraxecChildCaller::spawn`]. That spawns the
//!   `praxec` binary (located via `praxec_mcp::find_praxec_binary`)
//!   over `TokioChildProcess` stdio, runs the MCP init handshake, and
//!   returns a caller backed by a long-lived `RunningService`.
//! - The caller owns the service. Drop on the caller cleanly shuts the
//!   child down.
//! - This caller is intentionally NOT cached — `px walk` runs
//!   one workflow per invocation, so a one-shot child is the simplest
//!   correct model. (The executors-crate `McpExecutor` caches across
//!   tool calls because each call is one of many in a long-running
//!   gateway; here, there is exactly one consumer for one walk.)

use std::collections::HashMap;

use anyhow::{Context, Result, anyhow};
use async_trait::async_trait;
use rmcp::ServiceExt;
use rmcp::model::CallToolRequestParams;
use rmcp::service::{RoleClient, RunningService};
use rmcp::transport::TokioChildProcess;
use serde_json::Value;

use crate::interpreter::McpToolCaller;
use crate::praxec_mcp;

/// Production caller. Holds the `RunningService` for the child
/// process's lifetime; drop = clean shutdown.
pub struct PraxecChildCaller {
    service: RunningService<RoleClient, ()>,
}

impl PraxecChildCaller {
    /// Spawn `praxec` as a stdio child and run the MCP init
    /// handshake. `config_path` becomes `PRAXEC_CONFIG` env var on
    /// the child; `extra_env` is merged on top so operators can set
    /// e.g. log levels.
    pub async fn spawn(
        config_path: Option<&str>,
        extra_env: HashMap<String, String>,
    ) -> Result<Self> {
        let binary = praxec_mcp::find_praxec_binary().context("locating praxec binary")?;
        let mut cmd = tokio::process::Command::new(&binary);
        // The gateway CLI requires an explicit `serve` subcommand and reads its
        // config from `--config` (it does NOT consume PRAXEC_CONFIG). Spawn it
        // accordingly; passing the env too is harmless for other code paths.
        cmd.arg("serve");
        // `px walk` spawns a one-shot, single-workflow gateway — ephemeral
        // by design — so opt into the serve durability guard's escape hatch
        // rather than forcing a durable store/audit sink for a throwaway child.
        cmd.env("PRAXEC_ALLOW_EPHEMERAL", "1");
        if let Some(p) = config_path {
            cmd.arg("--config").arg(p);
            cmd.env("PRAXEC_CONFIG", p);
        }
        for (k, v) in extra_env {
            cmd.env(k, v);
        }
        let transport = TokioChildProcess::new(cmd)
            .with_context(|| format!("spawning praxec binary '{binary}'"))?;
        let service = ServiceExt::serve((), transport)
            .await
            .context("rmcp client init against praxec child process")?;
        Ok(Self { service })
    }
}

#[async_trait]
impl McpToolCaller for PraxecChildCaller {
    async fn call(&self, tool: &str, args: Value) -> Result<Value> {
        let mut params = CallToolRequestParams::new(tool.to_string());
        if let Some(obj) = args.as_object() {
            params = params.with_arguments(obj.clone());
        } else if !args.is_null() {
            return Err(anyhow!(
                "McpToolCaller args must be a JSON object or null; got: {}",
                args
            ));
        }
        let result = self
            .service
            .peer()
            .call_tool(params)
            .await
            .map_err(|e| anyhow!("praxec tool '{tool}' call failed: {e}"))?;

        // STUB-108 — `is_error` absent means success, per the MCP spec ("If
        // the tool call fails, isError MUST be set to true"). This also matches
        // how praxec's own gateway behaves: it returns business errors as
        // JSON-RPC errors (surfaced as `Err` from `call_tool` above) and builds
        // success results via `CallToolResult::structured`, which never sets
        // `is_error`. So `unwrap_or(false)` is the correct default here, not a
        // gap — a present `Some(true)` is the only error signal on this path.
        if result.is_error.unwrap_or(false) {
            let body = result
                .structured_content
                .or_else(|| {
                    (!result.content.is_empty())
                        .then(|| serde_json::json!({ "content": result.content }))
                })
                .unwrap_or(Value::Null);
            return Err(anyhow!(
                "praxec tool '{tool}' returned is_error=true: {}",
                body
            ));
        }

        Ok(result
            .structured_content
            .or_else(|| {
                (!result.content.is_empty())
                    .then(|| serde_json::json!({ "content": result.content }))
            })
            .unwrap_or(Value::Null))
    }
}
