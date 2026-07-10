//! D2 (docs/design-0.0.17-tool-source-ecosystem.md) — tool-source executor:
//! surface a D1 tool descriptor's `operations[]` as callable through the
//! gateway's two-tool surface.
//!
//! This is *not* a new runtime path. The executor loads a
//! [`ToolDescriptor`] (via D1's loader — parse → schema-validate →
//! `validate()`), resolves the requested operation, gates on the
//! descriptor's connection being a **live, granted** gateway connection,
//! then delegates the actual call to the existing `cli` / `mcp` / `rest`
//! executor for the descriptor's kind. Zero new transport, zero new trust
//! surface: every byte the tool can reach flows through a granted
//! `/connections` entry (SPEC §9.5).
//!
//! # Executor config
//!
//! ```yaml
//! executor:
//!   kind: tool_source
//!   descriptor_path: packs/tools/github-mcp.tool.json   # XOR `descriptor:` inline
//!   operation: search-issues                            # an operations[].id
//! ```
//!
//! # Argument transport (deterministic, per kind)
//!
//! The transition's `arguments` are validated against the operation's
//! `input_schema` (the descriptor's typed I/O contract), then travel to the
//! underlying executor exactly the way that executor already consumes them:
//!
//! - **mcp** — `arguments` pass through as the remote tool's arguments
//!   (the `mcp` executor's no-`map` path).
//! - **cli** — the dispatch's `args` become the argv; entries may reference
//!   `$.arguments.<x>` and render through the shared `arg_render` rules.
//! - **rest** — the dispatch's `path` interpolates `{var}` from `arguments`;
//!   for body-carrying methods (POST / PUT / PATCH) the `arguments` object
//!   is sent as the JSON body.
//!
//! # Fail-fast boundaries (never a fallback, never an auto-grant)
//!
//! - `TOOL_SOURCE_CONFIG` — the step config is malformed (neither/both of
//!   `descriptor` / `descriptor_path`, missing `operation`).
//! - D1's typed loader errors (`TOOL_DESCRIPTOR_*`, `TOOL_KIND_MISMATCH`,
//!   `TOOL_OPERATION_DISPATCH_MISMATCH`) surface verbatim — no partial load.
//! - `TOOL_SOURCE_UNKNOWN_OPERATION` — the requested id is not one of the
//!   descriptor's `operations[]`.
//! - `UNGRANTED_PACK_CONNECTION` — the connection is declared by a pack but
//!   not granted; the typed error carries the exact grant remedy
//!   (`conn_util`, unchanged D3 gate).
//! - `TOOL_SOURCE_CONNECTION_ABSENT` — the connection is not a live gateway
//!   connection at all. The error names the operator acts (add + grant);
//!   the executor NEVER writes or grants a connection itself (D3/D4a
//!   boundary: onboarding surfaces the grant, never performs it).
//! - `TOOL_SOURCE_CONNECTION_KIND_MISMATCH` — a live connection exists under
//!   the descriptor's `connection_name` but with a different `kind`.
//! - `TOOL_SOURCE_ARG_INVALID` — `arguments` violate the operation's
//!   `input_schema`; refusing to call the tool with the wrong arguments.

use std::collections::HashMap;
use std::path::Path;
use std::sync::Arc;

use async_trait::async_trait;
use praxec_core::error::ExecutorError;
use praxec_core::model::{ExecuteRequest, ExecuteResult};
use praxec_core::ports::Executor;
use praxec_core::tool_descriptor::{Operation, ToolDescriptor, ToolKind};
use serde_json::{Value, json};

/// Dispatches `kind: tool_source` transitions: descriptor load (D1),
/// operation resolution, grant gate, then delegation to the already-wired
/// `cli` / `mcp` / `rest` executor for the descriptor's kind.
pub struct ToolSourceExecutor {
    /// Live `/connections` entries: name → declared `kind`. Captured at
    /// construction from the merged gateway config (same source the
    /// per-kind connection registries parse).
    connection_kinds: HashMap<String, String>,
    /// SPEC §9.5 — pack-declared connections the operator has not granted
    /// (from `/praxec/_ungrantedConnections`). A descriptor reaching for one
    /// fails typed with the grant remedy — never spawnable, never granted here.
    ungranted: HashMap<String, crate::conn_util::UngrantedConnection>,
    cli: Arc<dyn Executor>,
    mcp: Arc<dyn Executor>,
    rest: Arc<dyn Executor>,
}

impl ToolSourceExecutor {
    /// Build from the merged gateway config plus the three transport
    /// executors this one composes. The delegates are the SAME instances the
    /// registry wires for `kind: cli` / `mcp` / `rest` (see
    /// `default_registry_with_late_workflow`), so connection caches and
    /// grant gates are shared, not duplicated.
    pub fn new(
        config: &Value,
        cli: Arc<dyn Executor>,
        mcp: Arc<dyn Executor>,
        rest: Arc<dyn Executor>,
    ) -> Self {
        let mut connection_kinds = HashMap::new();
        if let Some(map) = config.pointer("/connections").and_then(Value::as_object) {
            for (name, conn) in map {
                if let Some(kind) = conn.get("kind").and_then(Value::as_str) {
                    connection_kinds.insert(name.clone(), kind.to_string());
                }
            }
        }
        Self {
            connection_kinds,
            ungranted: crate::conn_util::ungranted_from_config(config),
            cli,
            mcp,
            rest,
        }
    }

    /// Load the descriptor named by the step config — exactly one of
    /// `descriptor` (inline object) or `descriptor_path` (file). Both or
    /// neither is a config defect, not a choice: fail typed.
    fn load_descriptor(cfg: &Value) -> Result<ToolDescriptor, ExecutorError> {
        let inline = cfg.get("descriptor");
        let path = cfg.get("descriptor_path").and_then(Value::as_str);
        match (inline, path) {
            (Some(_), Some(_)) => Err(ExecutorError::Permanent(
                "TOOL_SOURCE_CONFIG: `descriptor` and `descriptor_path` are both set; \
                 provide exactly one descriptor source"
                    .into(),
            )),
            (None, None) => Err(ExecutorError::Permanent(
                "TOOL_SOURCE_CONFIG: tool_source executor needs a descriptor — set \
                 `descriptor` (inline object) or `descriptor_path` (file path)"
                    .into(),
            )),
            (Some(value), None) => ToolDescriptor::load_value(value.clone())
                .map_err(|e| ExecutorError::Permanent(e.to_string())),
            (None, Some(p)) => ToolDescriptor::load_file(Path::new(p))
                .map_err(|e| ExecutorError::Permanent(e.to_string())),
        }
    }

    /// The connection gate — the descriptor's `reach.connection_name` must
    /// already be a live, granted `/connections` entry of the descriptor's
    /// kind. Declared-but-ungranted fails with the exact grant remedy
    /// (D3 gate, via `conn_util`); absent fails naming the operator acts.
    /// This executor NEVER installs or grants a connection.
    fn gate_connection(&self, descriptor: &ToolDescriptor) -> Result<(), ExecutorError> {
        let name = &descriptor.reach.connection_name;
        let kind = descriptor.kind.as_token();
        match self.connection_kinds.get(name) {
            Some(live_kind) if live_kind == kind => Ok(()),
            Some(live_kind) => Err(ExecutorError::Permanent(format!(
                "TOOL_SOURCE_CONNECTION_KIND_MISMATCH: descriptor '{tool}' needs a `kind: \
                 {kind}` connection named '{name}', but the live connection '{name}' is \
                 `kind: {live_kind}`",
                tool = descriptor.name,
            ))),
            None if self.ungranted.contains_key(name) => Err(
                crate::conn_util::connection_not_found_error(kind, name, &self.ungranted),
            ),
            None => Err(ExecutorError::Permanent(format!(
                "TOOL_SOURCE_CONNECTION_ABSENT: descriptor '{tool}' requires connection \
                 '{name}' (kind: {kind}) but no such gateway connection exists. Onboarding \
                 never auto-installs or auto-grants: the operator must add the descriptor's \
                 `reach.connection` under `connections:` (e.g. `px connections add \
                 --from-descriptor …`) and, for a pack-declared connection, add \
                 `{grant}` to that repo's `grant_connections:` (SPEC §9.5)",
                tool = descriptor.name,
                grant = descriptor.grant_token(),
            ))),
        }
    }

    /// Resolve the requested operation id against the descriptor.
    fn resolve_operation<'d>(
        descriptor: &'d ToolDescriptor,
        cfg: &Value,
    ) -> Result<&'d Operation, ExecutorError> {
        let requested = cfg
            .get("operation")
            .and_then(Value::as_str)
            .ok_or_else(|| {
                ExecutorError::Permanent(
                    "TOOL_SOURCE_CONFIG: tool_source executor needs `operation` (an \
                     operations[].id from the descriptor)"
                        .into(),
                )
            })?;
        descriptor
            .operations
            .iter()
            .find(|op| op.id == requested)
            .ok_or_else(|| {
                let available: Vec<&str> = descriptor
                    .operations
                    .iter()
                    .map(|op| op.id.as_str())
                    .collect();
                ExecutorError::Permanent(format!(
                    "TOOL_SOURCE_UNKNOWN_OPERATION: descriptor '{tool}' has no operation \
                     `{requested}`; available operations: {ops}",
                    tool = descriptor.name,
                    ops = available.join(", "),
                ))
            })
    }

    /// Validate the transition's `arguments` against the operation's
    /// `input_schema` — the descriptor's typed I/O contract. Refuse to call
    /// the tool with arguments the descriptor says are wrong (fail-fast; no
    /// best-effort pass-through). A schema that does not compile is a
    /// descriptor defect, also fail-fast.
    fn check_input(operation: &Operation, arguments: &Value) -> Result<(), ExecutorError> {
        let validator = jsonschema::validator_for(&operation.input_schema).map_err(|e| {
            ExecutorError::Permanent(format!(
                "TOOL_SOURCE_BAD_INPUT_SCHEMA: operation `{id}` carries an input_schema that \
                 does not compile: {e}",
                id = operation.id,
            ))
        })?;
        if validator.is_valid(arguments) {
            return Ok(());
        }
        let errs: Vec<String> = validator
            .iter_errors(arguments)
            .map(|e| e.to_string())
            .collect();
        Err(ExecutorError::Permanent(format!(
            "TOOL_SOURCE_ARG_INVALID: arguments for operation `{id}` were rejected by the \
             descriptor's input_schema before the call: {errs}",
            id = operation.id,
            errs = errs.join("; "),
        )))
    }

    /// A dispatch coordinate the D1 loader guarantees present is missing —
    /// unreachable after `ToolDescriptor::validate()`, kept as a typed
    /// defect (no `unwrap` in production code).
    fn dispatch_defect(kind: &str, op: &Operation) -> ExecutorError {
        ExecutorError::Permanent(format!(
            "TOOL_SOURCE_DISPATCH_DEFECT: operation `{id}` on a `{kind}` descriptor has no \
             `{kind}` dispatch coordinate — the D1 loader should have rejected this descriptor",
            id = op.id,
        ))
    }
}

#[async_trait]
impl Executor for ToolSourceExecutor {
    async fn execute(&self, request: ExecuteRequest) -> Result<ExecuteResult, ExecutorError> {
        let cfg = &request.executor_config;
        let descriptor = Self::load_descriptor(cfg)?;
        let operation = Self::resolve_operation(&descriptor, cfg)?;
        self.gate_connection(&descriptor)?;
        Self::check_input(operation, &request.arguments)?;

        let connection = &descriptor.reach.connection_name;
        // Exhaustive match over the closed kind — a fourth ToolKind fails to
        // compile until its dispatch is decided here (poka-yoke).
        let (delegate, executor_config): (&Arc<dyn Executor>, Value) = match descriptor.kind {
            ToolKind::Mcp => {
                let tool = operation
                    .mcp_tool
                    .as_deref()
                    .ok_or_else(|| Self::dispatch_defect("mcp", operation))?;
                (
                    &self.mcp,
                    json!({ "kind": "mcp", "connection": connection, "tool": tool }),
                )
            }
            ToolKind::Cli => {
                let dispatch = operation
                    .cli
                    .as_ref()
                    .ok_or_else(|| Self::dispatch_defect("cli", operation))?;
                (
                    &self.cli,
                    json!({ "kind": "cli", "connection": connection, "args": dispatch.args }),
                )
            }
            ToolKind::Rest => {
                let dispatch = operation
                    .rest
                    .as_ref()
                    .ok_or_else(|| Self::dispatch_defect("rest", operation))?;
                let mut rest_cfg = json!({
                    "kind": "rest",
                    "connection": connection,
                    "method": dispatch.method,
                    "path": dispatch.path,
                });
                // Body-carrying methods send the validated arguments object
                // as the JSON body — the deterministic argument-transport
                // rule for rest (module docs). Read-shaped methods carry
                // arguments only through `{var}` path interpolation.
                let carries_body = matches!(
                    dispatch.method.to_uppercase().as_str(),
                    "POST" | "PUT" | "PATCH"
                );
                if carries_body && request.arguments.is_object() {
                    if let Some(obj) = rest_cfg.as_object_mut() {
                        obj.insert("body".into(), request.arguments.clone());
                    }
                }
                (&self.rest, rest_cfg)
            }
        };

        let delegated = ExecuteRequest {
            executor_config,
            ..request
        };
        delegate.execute(delegated).await
    }
}
