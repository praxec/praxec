use std::collections::HashMap;
use std::process::Stdio;
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use praxec_core::error::ExecutorError;
use praxec_core::model::{Evidence, ExecuteRequest, ExecuteResult};
use praxec_core::ports::Executor;
use rmcp::model::CallToolRequestParams;
use rmcp::service::RunningService;
use rmcp::transport::async_rw::AsyncRwTransport;
use rmcp::transport::StreamableHttpClientTransport;
use rmcp::{RoleClient, ServiceExt};
use serde_json::{json, Map, Value};
use tokio::io::AsyncReadExt;
use tokio::sync::Mutex;
use uuid::Uuid;

use crate::idle::{with_idle_timeout, ActivityClock, ActivityTracked};

/// Per-connection idle (no-activity) ceiling on connect/`initialize` and each
/// tool call when the connection does not set `idleTimeoutMs`. This is an
/// **inactivity** bound, not a wall-clock budget: a cold `npx -y …` server that
/// is slowly downloading keeps the connection alive because stdio activity
/// resets the window; only a genuinely *silent* connection trips it. Required
/// default — there is no "unbounded" mode (FM3 / no-silent-default discipline).
pub const DEFAULT_IDLE_TIMEOUT_MS: u64 = 30_000;

/// Connections of `kind: mcp` parsed from gateway config, keyed by name.
#[derive(Default, Clone)]
pub struct McpConnections {
    inner: Arc<HashMap<String, McpConnection>>,
}

#[derive(Debug, Clone)]
pub struct McpConnection {
    pub command: Option<String>,
    pub args: Vec<String>,
    pub env: HashMap<String, String>,
    pub url: Option<String>,
    /// Per-connection inactivity ceiling (ms) for connect + each call. `None`
    /// falls back to [`DEFAULT_IDLE_TIMEOUT_MS`].
    pub idle_timeout_ms: Option<u64>,
}

impl McpConnection {
    fn idle(&self) -> Duration {
        Duration::from_millis(self.idle_timeout_ms.unwrap_or(DEFAULT_IDLE_TIMEOUT_MS))
    }
}

impl McpConnections {
    pub fn from_config(config: &Value) -> Self {
        let mut map = HashMap::new();
        if let Some(conns) = config.pointer("/connections").and_then(Value::as_object) {
            for (name, conn) in conns {
                if conn.get("kind").and_then(Value::as_str) != Some("mcp") {
                    continue;
                }
                let command = conn
                    .get("command")
                    .and_then(Value::as_str)
                    .map(str::to_string);
                let args = conn
                    .get("args")
                    .and_then(Value::as_array)
                    .map(|a| {
                        a.iter()
                            .filter_map(|v| v.as_str().map(str::to_string))
                            .collect()
                    })
                    .unwrap_or_default();
                let env = crate::conn_util::json_object_to_string_map(conn.get("env"));
                let url = conn.get("url").and_then(Value::as_str).map(str::to_string);
                let idle_timeout_ms = conn.get("idleTimeoutMs").and_then(Value::as_u64);
                map.insert(
                    name.clone(),
                    McpConnection {
                        command,
                        args,
                        env,
                        url,
                        idle_timeout_ms,
                    },
                );
            }
        }
        Self {
            inner: Arc::new(map),
        }
    }

    pub fn get(&self, name: &str) -> Option<&McpConnection> {
        self.inner.get(name)
    }
}

/// MCP executor: forwards `executor.kind=mcp` calls to a child MCP server
/// resolved by `executor.connection`. Clients are lazily started per
/// connection on first use and reused for the process lifetime.
/// The external boundary the `McpExecutor` depends on: resolve/connect to a
/// named MCP connection and invoke a tool (or list its tools). Abstracted as
/// a trait so the executor's own logic — config validation, argument mapping,
/// idempotency injection, result shaping, error classification — is unit
/// testable against a settable stub, exactly as the LLM executor's
/// `ProviderFactory` seam allows. Production uses [`RmcpToolCaller`]; only its
/// thin transport-construction is left as a documented test exclusion.
#[async_trait]
pub trait McpToolCaller: Send + Sync {
    /// Call `tool` on `connection` with `arguments`, returning the raw MCP
    /// `CallToolResult` (so the executor owns result shaping) or a classified
    /// transport/protocol error.
    async fn call_tool(
        &self,
        connection: &str,
        tool: &str,
        arguments: Option<Map<String, Value>>,
    ) -> Result<rmcp::model::CallToolResult, ExecutorError>;

    /// List the tools a connection exposes (used by capability import).
    async fn list_remote_tools(
        &self,
        connection: &str,
    ) -> Result<Vec<rmcp::model::Tool>, ExecutorError>;
}

/// A live, pooled MCP connection: the rmcp service plus the shared
/// [`ActivityClock`] its transport bumps on every byte (so [`with_idle_timeout`]
/// can tell a slow-but-alive connection from a hung one) and, for the
/// child-process transport, the owned child handle (spawned with
/// `kill_on_drop`, so dropping this entry reaps the server).
struct Conn {
    service: RunningService<RoleClient, ()>,
    clock: ActivityClock,
    idle: Duration,
    /// Kept alive for the connection's lifetime; `None` for the HTTP transport.
    /// `kill_on_drop(true)` means dropping it terminates + reaps the child.
    _child: Option<tokio::process::Child>,
}

/// Production [`McpToolCaller`]: owns the connection registry + the connection
/// cache and speaks rmcp over the configured transport.
pub struct RmcpToolCaller {
    connections: McpConnections,
    cache: Mutex<HashMap<String, Arc<Conn>>>,
}

impl RmcpToolCaller {
    pub fn new(connections: McpConnections) -> Self {
        Self {
            connections,
            cache: Mutex::new(HashMap::new()),
        }
    }

    async fn client_for(&self, name: &str) -> Result<Arc<Conn>, ExecutorError> {
        {
            let g = self.cache.lock().await;
            if let Some(c) = g.get(name) {
                return Ok(c.clone());
            }
        }

        let conn = self.connections.get(name).ok_or_else(|| {
            ExecutorError::Permanent(format!("mcp connection '{name}' not found"))
        })?;
        let idle = conn.idle();
        let clock = ActivityClock::new();

        // Two transports, picked by which connection field is set. URL wins
        // when both are present (since URL implies a hosted server, not a
        // process to launch). Both bound connect/`initialize` by the idle
        // window so an unreachable or silent server can never hang establishment.
        let (service, child): (
            RunningService<RoleClient, ()>,
            Option<tokio::process::Child>,
        ) = if let Some(url) = &conn.url {
            let transport = StreamableHttpClientTransport::<reqwest::Client>::from_uri(url.clone());
            // HTTP has no child stdio to tap, so the clock isn't byte-bumped:
            // the idle window acts as a connect timeout here.
            clock.mark();
            let client = with_idle_timeout(idle, &clock, ServiceExt::serve((), transport))
                .await
                .map_err(|e| ExecutorError::Connection(format!("mcp http connect '{name}': {e}")))?
                .map_err(|e| ExecutorError::Connection(format!("mcp http init '{name}': {e}")))?;
            (client, None)
        } else {
            let command = conn.command.as_deref().ok_or_else(|| {
                ExecutorError::Permanent(format!(
                    "mcp connection '{name}' has neither `command` nor `url`"
                ))
            })?;

            let mut cmd = tokio::process::Command::new(command);
            cmd.args(&conn.args);
            for (k, v) in &conn.env {
                cmd.env(k, v);
            }
            // Own the pipes (so we can observe stdio activity) and the child
            // (so we can reap it) — rmcp's `TokioChildProcess` hides both.
            cmd.stdin(Stdio::piped())
                .stdout(Stdio::piped())
                .stderr(Stdio::piped())
                .kill_on_drop(true);

            let mut child = cmd
                .spawn()
                .map_err(|e| ExecutorError::Connection(format!("spawn '{command}': {e}")))?;
            let stdout = child.stdout.take().ok_or_else(|| {
                ExecutorError::Connection(format!("mcp '{name}': child stdout unavailable"))
            })?;
            let stdin = child.stdin.take().ok_or_else(|| {
                ExecutorError::Connection(format!("mcp '{name}': child stdin unavailable"))
            })?;
            let stderr = child.stderr.take();

            // Drain stderr in the background: it carries the npm/npx download
            // progress that proves a cold start is *alive* (resetting the idle
            // window), and server logs we tee to tracing rather than lose.
            if let Some(stderr) = stderr {
                let clock = clock.clone();
                let name = name.to_string();
                tokio::spawn(async move {
                    let mut stderr = stderr;
                    let mut buf = [0u8; 4096];
                    loop {
                        match stderr.read(&mut buf).await {
                            Ok(0) | Err(_) => break,
                            Ok(n) => {
                                clock.mark();
                                tracing::debug!(
                                    connection = %name,
                                    "mcp stderr: {}",
                                    String::from_utf8_lossy(&buf[..n]).trim_end()
                                );
                            }
                        }
                    }
                });
            }

            // The transport reads through `ActivityTracked`, so every inbound
            // MCP byte (initialize response, tool results, progress
            // notifications) also resets the idle window.
            let transport =
                AsyncRwTransport::new_client(ActivityTracked::new(stdout, clock.clone()), stdin);
            clock.mark();
            let client = with_idle_timeout(idle, &clock, ServiceExt::serve((), transport))
                .await
                .map_err(|e| {
                    ExecutorError::Connection(format!("mcp connect '{name}': {e} (idle timeout)"))
                })?
                .map_err(|e| ExecutorError::Connection(format!("mcp init '{name}': {e}")))?;
            (client, Some(child))
        };

        let entry = Arc::new(Conn {
            service,
            clock,
            idle,
            _child: child,
        });
        let mut g = self.cache.lock().await;
        // Another task may have populated the cache while we connected; keep the
        // first winner (ours drops here, reaping its child via kill_on_drop).
        Ok(g.entry(name.to_string()).or_insert(entry).clone())
    }

    /// Graceful shutdown — drain every pooled connection so its child MCP server
    /// is reaped. The cache pools connections for reuse (no per-call teardown), so
    /// without this they live for the caller's whole lifetime. For the sole owner
    /// we `await` an explicit `cancel()` for synchronous, guaranteed teardown;
    /// dropping the entry afterwards reaps the child (`kill_on_drop`).
    pub async fn close(&self) {
        let drained: Vec<Arc<Conn>> = {
            let mut g = self.cache.lock().await;
            g.drain().map(|(_, arc)| arc).collect()
        };
        for arc in drained {
            // Sole owner → cancel synchronously; otherwise an in-flight call still
            // holds a ref and the last owner's drop reaps it.
            if let Some(conn) = Arc::into_inner(arc) {
                let _ = conn.service.cancel().await;
            }
        }
    }
}

#[async_trait]
impl McpToolCaller for RmcpToolCaller {
    async fn call_tool(
        &self,
        connection: &str,
        tool: &str,
        arguments: Option<Map<String, Value>>,
    ) -> Result<rmcp::model::CallToolResult, ExecutorError> {
        let conn = self.client_for(connection).await?;
        let mut params = CallToolRequestParams::new(tool.to_string());
        if let Some(args) = arguments {
            params = params.with_arguments(args);
        }
        // Reset the window before waiting (the cached clock may be stale from a
        // prior call); inbound bytes during the call keep resetting it, so a tool
        // streaming progress survives while a silent one is cut off.
        conn.clock.mark();
        with_idle_timeout(
            conn.idle,
            &conn.clock,
            conn.service.peer().call_tool(params),
        )
        .await
        .map_err(|e| ExecutorError::Connection(format!("mcp call '{connection}.{tool}': {e}")))?
        .map_err(|e| classify(e.to_string()))
    }

    /// Connect (or reuse a cached connection) and list the connection's tools
    /// via the standard `tools/list` MCP method. Vendor-neutral — works for any
    /// process the connection can spawn (native binary, `npx -y …`, `uvx …`,
    /// `docker run …`, etc.).
    async fn list_remote_tools(
        &self,
        connection: &str,
    ) -> Result<Vec<rmcp::model::Tool>, ExecutorError> {
        let conn = self.client_for(connection).await?;
        conn.clock.mark();
        with_idle_timeout(conn.idle, &conn.clock, conn.service.peer().list_all_tools())
            .await
            .map_err(|e| ExecutorError::Connection(format!("mcp list '{connection}': {e}")))?
            .map_err(|e| classify(e.to_string()))
    }
}

/// Dispatches `kind: mcp` transitions: validates config, maps arguments,
/// injects the idempotency key, then delegates the actual tool call to an
/// injected [`McpToolCaller`] (production: [`RmcpToolCaller`]).
pub struct McpExecutor {
    caller: Arc<dyn McpToolCaller>,
}

impl McpExecutor {
    /// Production constructor: speaks rmcp over the configured transports.
    pub fn new(connections: McpConnections) -> Self {
        Self {
            caller: Arc::new(RmcpToolCaller::new(connections)),
        }
    }

    /// Inject a custom caller (a stub in tests, or an alternate transport).
    pub fn with_caller(caller: Arc<dyn McpToolCaller>) -> Self {
        Self { caller }
    }

    /// List a connection's tools (used by capability import).
    pub async fn list_remote_tools(
        &self,
        connection: &str,
    ) -> Result<Vec<rmcp::model::Tool>, ExecutorError> {
        self.caller.list_remote_tools(connection).await
    }
}

#[async_trait]
impl Executor for McpExecutor {
    async fn execute(&self, request: ExecuteRequest) -> Result<ExecuteResult, ExecutorError> {
        let cfg = &request.executor_config;
        let connection = cfg
            .get("connection")
            .and_then(Value::as_str)
            .ok_or_else(|| ExecutorError::Permanent("mcp executor needs `connection`".into()))?;
        let tool = cfg
            .get("tool")
            .and_then(Value::as_str)
            .ok_or_else(|| ExecutorError::Permanent("mcp executor needs `tool`".into()))?;

        // No `map:` declared → pass raw `arguments` through unchanged.
        // `map:` present but a binding fails to resolve → fail-fast rather
        // than silently calling the tool with the wrong (raw) arguments.
        let mapped_args = match render_args(cfg.get("map"), &request)? {
            Some(mapped) => mapped,
            None => request.arguments.clone(),
        };
        let mut arguments = mapped_args.as_object().cloned();

        // Pre-validate the arguments against the CONNECTED tool's own input
        // schema BEFORE calling — so an out-of-vocabulary value (e.g. an enum
        // the server doesn't accept, like an FMECA `domain` the engine doesn't
        // model) fails fast HERE, naming the server's own constraint ("X is not
        // one of [...]"), instead of deep in the server as a cryptic -32602.
        // Validated before the internal `_idempotencyKey` is injected so an
        // `additionalProperties: false` schema doesn't reject our own field.
        // BEST-EFFORT: if the schema can't be listed/parsed we skip and let the
        // call proceed — we never block a valid call on our own fetch failure.
        if let Some(args_obj) = &arguments {
            if let Ok(remote_tools) = self.caller.list_remote_tools(connection).await {
                if let Some(t) = remote_tools.iter().find(|t| t.name.as_ref() == tool) {
                    let schema = Value::Object((*t.input_schema).clone());
                    if let Ok(validator) = jsonschema::validator_for(&schema) {
                        let value = Value::Object(args_obj.clone());
                        if !validator.is_valid(&value) {
                            let errs: Vec<String> = validator
                                .iter_errors(&value)
                                .map(|e| e.to_string())
                                .collect();
                            return Err(ExecutorError::Permanent(format!(
                                "MCP_ARG_INVALID: '{connection}.{tool}' arguments were rejected by \
                                 the tool's input schema before the call: {}",
                                errs.join("; ")
                            )));
                        }
                    }
                }
            }
        }

        // If the runtime computed an idempotency key, surface it as a
        // `_idempotencyKey` field in the tool arguments. Downstream MCP
        // tools that honor the convention can dedupe; tools that don't
        // simply ignore the extra field.
        if let Some(key) = &request.idempotency_key {
            let mut a = arguments.unwrap_or_default();
            a.insert("_idempotencyKey".into(), Value::String(key.clone()));
            arguments = Some(a);
        }

        let result = self.caller.call_tool(connection, tool, arguments).await?;

        let output = if let Some(structured) = result.structured_content {
            structured
        } else if !result.content.is_empty() {
            json!({ "content": result.content })
        } else {
            json!({})
        };

        if result.is_error.unwrap_or(false) {
            return Err(ExecutorError::Permanent(format!(
                "mcp tool '{}' returned error: {}",
                tool,
                serde_json::to_string(&output).unwrap_or_default()
            )));
        }

        Ok(ExecuteResult {
            output,
            evidence: vec![Evidence {
                kind: "mcp_tool_result".to_string(),
                id: Uuid::new_v4().to_string(),
                uri: None,
                summary: Some(format!("Called {connection}.{tool}")),
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

/// Apply an executor `map:` block against the available scopes.
///
/// `map:` is a TEMPLATE, resolved recursively:
/// - a string starting with `$.` is a **scope path** — resolved against
///   arguments / context / input, and **fail-fast** if it does not resolve
///   (silently dropping it would call the tool with the wrong args, CMP-034);
/// - any other string, and every number / bool / null, is a **literal**;
/// - **objects and arrays** are walked, so a binding can assemble a nested
///   shape mixing paths and literals
///   (e.g. `params: { path: "$.workflow.input.target_path", max_groups: 5 }`)
///   — which a flat string-only binding could not express.
///
/// Returns:
/// - `Ok(None)` when no `map:` is declared — raw `arguments` pass through.
/// - `Ok(Some(obj))` when every `$.` path resolved.
/// - `Err(..)` when `map:` is not an object, or a `$.` path is unresolvable.
fn render_args(
    map: Option<&Value>,
    request: &ExecuteRequest,
) -> Result<Option<Value>, ExecutorError> {
    let Some(map) = map else {
        return Ok(None);
    };
    let map = map.as_object().ok_or_else(|| {
        ExecutorError::Permanent(
            "INVALID_MCP_MAP: executor `map` must be an object of `{ targetArg: <template> }` \
             bindings, where a template is a `$.scope.path` string, a literal, or a nested \
             object/array of those."
                .into(),
        )
    })?;
    let mut out = serde_json::Map::new();
    for (target, source) in map {
        out.insert(target.clone(), resolve_template(source, request, target)?);
    }
    Ok(Some(Value::Object(out)))
}

/// Recursively resolve a `map:` template value. `$.`-prefixed strings are scope
/// paths (resolved or fail-fast); objects/arrays are walked; everything else is
/// a literal. `path` is the dotted binding location, used only for error
/// messages.
fn resolve_template(
    value: &Value,
    request: &ExecuteRequest,
    path: &str,
) -> Result<Value, ExecutorError> {
    match value {
        Value::String(s) if s.starts_with("$.") => praxec_core::mapping::read_in_scopes(
            s,
            &request.arguments,
            &request.workflow.context,
            &request.workflow.input,
            None,
        )
        .ok_or_else(|| {
            ExecutorError::Permanent(format!(
                "MCP_MAP_BINDING_UNRESOLVED: `map` binding `{path}: {s}` did not resolve \
                 against the available scopes (arguments / context / input). Refusing to call \
                 the tool with the wrong arguments."
            ))
        }),
        // Literal string / number / bool / null — passed through verbatim.
        Value::String(_) | Value::Number(_) | Value::Bool(_) | Value::Null => Ok(value.clone()),
        Value::Array(items) => {
            let mut out = Vec::with_capacity(items.len());
            for (i, item) in items.iter().enumerate() {
                out.push(resolve_template(item, request, &format!("{path}[{i}]"))?);
            }
            Ok(Value::Array(out))
        }
        Value::Object(obj) => {
            let mut out = serde_json::Map::new();
            for (k, v) in obj {
                out.insert(
                    k.clone(),
                    resolve_template(v, request, &format!("{path}.{k}"))?,
                );
            }
            Ok(Value::Object(out))
        }
    }
}

fn classify(message: String) -> ExecutorError {
    let lc = message.to_lowercase();
    if lc.contains("timeout") || lc.contains("timed out") {
        ExecutorError::Timeout(0)
    } else if lc.contains("rate limit") {
        ExecutorError::RateLimited(message)
    } else if lc.contains("unauthorized")
        || lc.contains("forbidden")
        || lc.contains("401")
        || lc.contains("403")
    {
        // NFR R1 — auth failures from MCP-over-HTTP transports must not be retried.
        ExecutorError::Auth(message)
    } else if lc.contains("connection") || lc.contains("closed") || lc.contains("broken pipe") {
        ExecutorError::Connection(message)
    } else {
        ExecutorError::Transient(message)
    }
}

#[cfg(test)]
mod idle_wiring_tests {
    use super::*;
    use std::time::{Duration, Instant};

    /// A child that spawns but never speaks MCP (here `sleep`) used to hang the
    /// connect/`initialize` handshake forever — the original >10-minute hang.
    /// Establishment must now surface an idle-timeout error shortly after the
    /// per-connection idle window, never wait the process out.
    #[tokio::test]
    async fn establishing_a_silent_child_times_out_on_idle_rather_than_hanging() {
        let config = json!({
            "connections": {
                "silent": {
                    "kind": "mcp",
                    "command": "sleep",
                    "args": ["30"],
                    "idleTimeoutMs": 300
                }
            }
        });
        let caller = RmcpToolCaller::new(McpConnections::from_config(&config));

        let start = Instant::now();
        // Harness backstop: if establishment still hangs, this fires and the
        // `.expect` below fails the test (RED) rather than wedging CI.
        let result =
            tokio::time::timeout(Duration::from_secs(5), caller.list_remote_tools("silent"))
                .await
                .expect("establishment must not hang past the idle window");

        assert!(
            result.is_err(),
            "a silent child must surface an error, not a tool list"
        );
        assert!(
            start.elapsed() < Duration::from_secs(2),
            "must fire near the 300ms idle window, got {:?}",
            start.elapsed()
        );
    }
}

#[cfg(test)]
mod render_args_tests {
    use super::*;
    use praxec_core::model::WorkflowInstance;

    fn req(context: Value, input: Value, arguments: Value) -> ExecuteRequest {
        ExecuteRequest {
            workflow: WorkflowInstance {
                id: "wf".into(),
                definition_id: "demo".into(),
                definition_version: "0".into(),
                definition: Value::Null,
                state: "running".into(),
                version: 0,
                input,
                context,
                started_at: chrono::Utc::now(),
                trace_id: None,
                run_id: None,
                cancelled_at: None,
                cancelled_reason: None,
                depth: 0,
                parent: None,
            },
            transition: Some("go".into()),
            arguments,
            executor_config: Value::Null,
            idempotency_key: None,
            correlation_id: None,
        }
    }

    // A nested object binding assembles a shape mixing a resolved `$.` path with
    // literals — the whole point of the lift (a flat string binding could not
    // express `{ path: <resolved>, max_groups: 5 }`).
    #[test]
    fn a_nested_binding_mixes_resolved_paths_and_literals() {
        let r = req(
            json!({ "target": "crates/x/src/big.rs" }),
            json!({}),
            json!({}),
        );
        let map = json!({
            "params": { "path": "$.context.target", "max_groups": 5, "mode": "safe" }
        });
        let out = render_args(Some(&map), &r).unwrap().unwrap();
        assert_eq!(
            out["params"],
            json!({ "path": "crates/x/src/big.rs", "max_groups": 5, "mode": "safe" })
        );
    }

    // A `$.` path inside a nested binding that does not resolve still fails fast
    // (the recursion preserves CMP-034, it does not swallow a typo'd path).
    #[test]
    fn an_unresolvable_path_inside_a_nested_binding_fails_fast() {
        let r = req(json!({}), json!({}), json!({}));
        let map = json!({ "params": { "path": "$.context.missing" } });
        let err = render_args(Some(&map), &r).unwrap_err();
        assert!(format!("{err:?}").contains("MCP_MAP_BINDING_UNRESOLVED"));
    }

    // A non-`$.` string is a literal, not a (failed) path lookup.
    #[test]
    fn a_plain_string_is_a_literal_not_a_path() {
        let r = req(json!({}), json!({}), json!({}));
        let map = json!({ "action": "propose_decomposition" });
        let out = render_args(Some(&map), &r).unwrap().unwrap();
        assert_eq!(out["action"], json!("propose_decomposition"));
    }

    // An array binding resolves element-wise (paths + literals interleaved).
    #[test]
    fn an_array_binding_resolves_element_wise() {
        let r = req(json!({ "b": "two" }), json!({}), json!({}));
        let map = json!({ "items": ["one", "$.context.b", 3] });
        let out = render_args(Some(&map), &r).unwrap().unwrap();
        assert_eq!(out["items"], json!(["one", "two", 3]));
    }
}

/// The repo mocks the `McpToolCaller` seam, so the real rmcp transport was
/// previously untested — and this change *replaces* that transport (rmcp's
/// `TokioChildProcess` → our own pipes + `ActivityTracked` + `AsyncRwTransport`).
/// This drives the exact client stack `client_for` composes against a real
/// rmcp server over an in-process duplex, proving the swap still completes a
/// genuine `initialize` → `tools/list` → `call_tool` session.
#[cfg(test)]
mod transport_happy_path_tests {
    use super::*;
    use rmcp::model::{
        CallToolResult, InitializeResult, ListToolsResult, PaginatedRequestParams, ProtocolVersion,
        ServerCapabilities, ServerInfo, Tool,
    };
    use rmcp::service::{RequestContext, RoleServer};
    use rmcp::{serve_server, ErrorData as McpError, ServerHandler};
    use std::time::Duration;

    #[derive(Clone)]
    struct PingServer;

    impl ServerHandler for PingServer {
        fn get_info(&self) -> ServerInfo {
            let mut info = InitializeResult::default();
            info.protocol_version = ProtocolVersion::default();
            info.capabilities = ServerCapabilities::builder().enable_tools().build();
            info
        }

        async fn list_tools(
            &self,
            _request: Option<PaginatedRequestParams>,
            _context: RequestContext<RoleServer>,
        ) -> Result<ListToolsResult, McpError> {
            let tool = Tool::new("ping", "returns pong", Arc::new(serde_json::Map::new()));
            Ok(ListToolsResult::with_all_items(vec![tool]))
        }

        async fn call_tool(
            &self,
            _request: CallToolRequestParams,
            _context: RequestContext<RoleServer>,
        ) -> Result<CallToolResult, McpError> {
            Ok(CallToolResult::structured(json!({ "pong": true })))
        }
    }

    #[tokio::test]
    async fn the_tracked_transport_completes_a_real_initialize_list_and_call() {
        let (client_io, server_io) = tokio::io::duplex(8192);
        let (cr, cw) = tokio::io::split(client_io);
        let (sr, sw) = tokio::io::split(server_io);

        // Real rmcp server on one end, driven concurrently.
        let server_task = tokio::spawn(async move {
            let server = serve_server(PingServer, AsyncRwTransport::new_server(sr, sw))
                .await
                .expect("server initialize");
            let _ = server.waiting().await;
        });

        // Client through the exact stack `client_for` builds.
        let clock = ActivityClock::new();
        let idle = Duration::from_secs(5);
        let transport = AsyncRwTransport::new_client(ActivityTracked::new(cr, clock.clone()), cw);
        clock.mark();
        let client = with_idle_timeout(idle, &clock, ServiceExt::serve((), transport))
            .await
            .expect("client not idle during connect")
            .expect("client initialize");

        clock.mark();
        let tools = with_idle_timeout(idle, &clock, client.peer().list_all_tools())
            .await
            .expect("not idle during list")
            .expect("list tools");
        assert!(
            tools.iter().any(|t| t.name == "ping"),
            "the server's tool must come back through the tracked transport"
        );

        clock.mark();
        let result = with_idle_timeout(
            idle,
            &clock,
            client
                .peer()
                .call_tool(CallToolRequestParams::new("ping".to_string())),
        )
        .await
        .expect("not idle during call")
        .expect("call tool");
        assert_eq!(
            result.structured_content,
            Some(json!({ "pong": true })),
            "the call result must round-trip through the tracked transport"
        );

        let _ = client.cancel().await;
        server_task.abort();
    }
}
