#![cfg_attr(not(test), warn(clippy::unwrap_used))]

//! MCP representation of the Mission Control cockpit's interaction model.
//!
//! The cockpit (`praxec-cockpit`) is a self-contained orchestrator: its
//! interaction model is a small set of navigation [`CockpitOp`]s, and the chat
//! LLM drives them. This crate is the **MCP face** of that interaction model —
//! it exposes the same ops as MCP tools so an *external* agent (another model,
//! Claude Code, a custom orchestrator) can drive the cockpit over the standard
//! MCP protocol, exactly as the in-process chat LLM does.
//!
//! The op catalog and the tool→op mapping are defined **once** in
//! `praxec_cockpit::op` ([`op_tools`] / [`op_from_tool_call`]); this
//! server is a thin transport binding over them. rmcp 1.7 has no in-process
//! transport, so the in-process chat loop calls that dispatch core directly
//! (approach B); this crate is the stdio seam for everyone else, built now so
//! the external-drive story (ADR-0002 fleet runtime) meets no resistance later.
//!
//! # Tool surface
//!
//! | Tool name    | Effect                                   |
//! |--------------|------------------------------------------|
//! | `zoom_into`  | Zoom the map into a mission by name      |
//! | `zoom_out`   | Zoom back out to the Fleet               |
//! | `pan`        | Move the Fleet cursor by a signed delta  |
//! | `quit`       | Quit the cockpit                         |
//!
//! # State
//!
//! The server resolves a tool call against a [`Fleet`] + [`Level`] snapshot and
//! returns the [`CockpitOp`] it *resolves to* (structured JSON). Applying that
//! op to a *live* running cockpit is the fleet-runtime increment (ADR-0002);
//! today the binding + dispatch are real and tested, the live handle is not yet
//! wired.

use std::borrow::Cow;
use std::sync::Arc;

use praxec_cockpit::map::Level;
use praxec_cockpit::map::fleet::Fleet;
use praxec_cockpit::op::{self, CockpitOp, OpTool};
use rmcp::ErrorData as McpError;
use rmcp::model::{
    CallToolRequestParams, CallToolResult, Implementation, InitializeRequestParams,
    InitializeResult, ListToolsResult, PaginatedRequestParams, ProtocolVersion, ServerCapabilities,
    ServerInfo, Tool,
};
use rmcp::service::{NotificationContext, RequestContext, RoleServer};
use rmcp::transport::stdio;
use rmcp::{ServerHandler, ServiceExt};
use serde_json::{Value, json};

/// The cockpit ops as MCP [`Tool`]s, projected from the canonical
/// [`op::op_tools`] catalog.
pub fn cockpit_tool_definitions() -> Vec<Tool> {
    op::op_tools().into_iter().map(op_tool_to_mcp).collect()
}

fn op_tool_to_mcp(t: OpTool) -> Tool {
    Tool::new(
        Cow::Owned(t.name.to_string()),
        Cow::Owned(t.description.to_string()),
        schema_object(t.schema),
    )
}

/// Wrap a JSON-schema object literal as the `Arc<JsonObject>` rmcp wants.
fn schema_object(value: Value) -> Arc<rmcp::model::JsonObject> {
    debug_assert!(
        value.is_object(),
        "schema_object expects an object literal; got non-object"
    );
    let obj = match value.as_object() {
        Some(o) => o.clone(),
        None => serde_json::Map::new(),
    };
    Arc::new(obj)
}

// ---------------------------------------------------------------------------
// CockpitServer
// ---------------------------------------------------------------------------

/// MCP server façade exposing the cockpit's navigation ops. Holds a fleet +
/// altitude snapshot to resolve mission-name tool calls into concrete ops.
#[derive(Clone)]
pub struct CockpitServer {
    fleet: Arc<Fleet>,
    level: Level,
    server_name: String,
    server_version: String,
}

impl CockpitServer {
    /// Build a server resolving tool calls against the given fleet snapshot at
    /// the given altitude.
    pub fn new(fleet: Fleet, level: Level) -> Self {
        Self {
            fleet: Arc::new(fleet),
            level,
            server_name: "praxec-cockpit".to_string(),
            server_version: env!("CARGO_PKG_VERSION").to_string(),
        }
    }

    /// A server over the demo fleet at the Fleet altitude — for smoke tests and
    /// `praxec-cockpit-mcp` without a live cockpit attached.
    pub fn demo() -> Self {
        Self::new(Fleet::demo(), Level::Fleet)
    }

    /// Override the advertised server identity.
    pub fn with_identity(mut self, name: impl Into<String>, version: impl Into<String>) -> Self {
        self.server_name = name.into();
        self.server_version = version.into();
        self
    }

    /// Serve the MCP surface over stdio. Blocks until the peer disconnects.
    pub async fn serve_stdio(self) -> anyhow::Result<()> {
        let service = self.serve(stdio()).await?;
        service.waiting().await?;
        Ok(())
    }

    /// Transport-free dispatch. Resolves a tool call into the [`CockpitOp`] it
    /// maps to (as structured JSON) using the fleet/altitude snapshot. Tests
    /// call this directly; `ServerHandler::call_tool` is a thin wrapper.
    pub async fn dispatch_call(&self, request: CallToolRequestParams) -> Result<Value, McpError> {
        let args: Value = request
            .arguments
            .as_ref()
            .map(|m| Value::Object(m.clone()))
            .unwrap_or_else(|| json!({}));

        let op = op::op_from_tool_call(request.name.as_ref(), &args, &self.fleet, self.level)
            .map_err(|reason| McpError::invalid_params(reason, None))?;

        Ok(self.op_to_value(op))
    }

    /// Structured JSON for the op a tool call resolved to — the wire contract an
    /// external driver reads back.
    fn op_to_value(&self, op: CockpitOp) -> Value {
        match op {
            CockpitOp::Pan(delta) => json!({ "op": op::TOOL_PAN, "delta": delta }),
            CockpitOp::ZoomInto(index) => {
                let mission = self.fleet.missions.get(index).map(|m| m.name.as_str());
                json!({ "op": op::TOOL_ZOOM_INTO, "mission_index": index, "mission": mission })
            }
            CockpitOp::ZoomOut => json!({ "op": op::TOOL_ZOOM_OUT }),
            CockpitOp::Quit => json!({ "op": op::TOOL_QUIT }),
        }
    }
}

impl ServerHandler for CockpitServer {
    fn get_info(&self) -> ServerInfo {
        let mut server_info =
            Implementation::new(self.server_name.clone(), self.server_version.clone());
        server_info.title = Some("Mission Control cockpit".to_string());
        server_info.description = Some(
            "MCP face of the cockpit's interaction model: drive the map's \
             navigation ops (zoom_into / zoom_out / pan / quit) as MCP tools."
                .to_string(),
        );

        let mut info = InitializeResult::default();
        info.protocol_version = ProtocolVersion::default();
        info.capabilities = ServerCapabilities::builder().enable_tools().build();
        info.server_info = server_info;
        info.instructions = Some(
            "Call one navigation tool per turn to move the cockpit's map. \
             `zoom_into` takes a mission name fragment; `pan` takes a signed \
             tile delta; `zoom_out` and `quit` take no arguments."
                .to_string(),
        );
        info
    }

    async fn initialize(
        &self,
        request: InitializeRequestParams,
        context: RequestContext<RoleServer>,
    ) -> Result<InitializeResult, McpError> {
        if context.peer.peer_info().is_none() {
            context.peer.set_peer_info(request);
        }
        Ok(self.get_info())
    }

    async fn list_tools(
        &self,
        _request: Option<PaginatedRequestParams>,
        _context: RequestContext<RoleServer>,
    ) -> Result<ListToolsResult, McpError> {
        Ok(ListToolsResult::with_all_items(cockpit_tool_definitions()))
    }

    async fn call_tool(
        &self,
        request: CallToolRequestParams,
        _context: RequestContext<RoleServer>,
    ) -> Result<CallToolResult, McpError> {
        self.dispatch_call(request)
            .await
            .map(CallToolResult::structured)
    }

    fn get_tool(&self, name: &str) -> Option<Tool> {
        cockpit_tool_definitions()
            .into_iter()
            .find(|t| t.name == name)
    }

    async fn on_initialized(&self, _context: NotificationContext<RoleServer>) {
        tracing::info!("praxec-cockpit MCP client initialized");
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use rmcp::model::CallToolRequestParams;

    fn call(name: &str, args: Value) -> CallToolRequestParams {
        let map = args.as_object().cloned().unwrap_or_default();
        CallToolRequestParams::new(name.to_string()).with_arguments(map)
    }

    #[tokio::test]
    async fn lists_the_four_navigation_tools() {
        let names: Vec<_> = cockpit_tool_definitions()
            .into_iter()
            .map(|t| t.name.to_string())
            .collect();
        assert_eq!(names, vec!["zoom_into", "zoom_out", "pan", "quit"]);
    }

    #[tokio::test]
    async fn zoom_into_resolves_a_mission_name_to_an_index() {
        let server = CockpitServer::demo();
        let out = server
            .dispatch_call(call("zoom_into", json!({ "mission": "postgres" })))
            .await
            .unwrap();
        assert_eq!(out["op"], "zoom_into");
        assert_eq!(out["mission_index"], 2);
    }

    #[tokio::test]
    async fn pan_round_trips_the_delta() {
        let server = CockpitServer::demo();
        let out = server
            .dispatch_call(call("pan", json!({ "delta": -3 })))
            .await
            .unwrap();
        assert_eq!(out, json!({ "op": "pan", "delta": -3 }));
    }

    #[tokio::test]
    async fn an_unknown_tool_is_invalid_params() {
        let server = CockpitServer::demo();
        let err = server
            .dispatch_call(call("frobnicate", json!({})))
            .await
            .unwrap_err();
        // invalid_params is the MCP code for a bad/unknown call.
        assert!(err.to_string().to_lowercase().contains("unknown"));
    }

    #[tokio::test]
    async fn zoom_out_at_the_fleet_is_rejected() {
        let server = CockpitServer::new(Fleet::demo(), Level::Fleet);
        let err = server
            .dispatch_call(call("zoom_out", json!({})))
            .await
            .unwrap_err();
        assert!(err.to_string().contains("Fleet"));
    }
}
