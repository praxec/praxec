//! The cockpit's read surface. Mission Control is a *client* of the same gateway
//! the model uses (SPEC §32). Two implementations:
//! - [`FakeGateway`] — fixture-backed, for development and tests.
//! - [`StdioGateway`] — live: a thin rmcp **stdio client** to a running
//!   `praxec` server, reading a mission via `praxec.query { workflowId }`.

use crate::model::{DefinitionDetail, GatewayResponse, LibraryEntry, LibraryListing};
use anyhow::{Context, Result};
use rmcp::model::CallToolRequestParams;
use rmcp::service::{RoleClient, RunningService};
use rmcp::transport::TokioChildProcess;
use rmcp::ServiceExt;
use serde_json::{json, Value};

/// The cockpit's gateway surface: read a mission, and submit a transition (the
/// governed write — exactly the two stable tools the model uses, SPEC §32).
pub trait Gateway {
    /// Fetch the current response for a workflow instance by id.
    fn get(&self, workflow_id: &str) -> Result<GatewayResponse>;
    /// Submit a transition (`praxec.command`); returns the post-transition
    /// gateway response. `expected_version` guards against a stale read (optimistic
    /// concurrency); the gateway rejects the write if the instance moved.
    fn command(
        &self,
        workflow_id: &str,
        expected_version: u64,
        transition: &str,
    ) -> Result<GatewayResponse>;
    /// Launch a new mission: start a workflow/agent instance via `praxec.command
    /// { definitionId, input }` (the §32 start dispatch). Returns the new mission's
    /// response — its fresh `workflow.id` is how the cockpit tracks it.
    fn launch(&self, definition_id: &str, input: Value) -> Result<GatewayResponse>;
    /// Browse the layered library — every discoverable definition the gateway
    /// serves (Build mode). Reads via `praxec.query` discovery, the same
    /// surface the model searches.
    fn library(&self) -> Result<Vec<LibraryEntry>>;
    /// Read one definition's current body + content hash (`praxec.query
    /// { definitionId }`) — the basis for an edit (Build mode, ⏎ on a row).
    fn read_definition(&self, definition_id: &str) -> Result<DefinitionDetail>;
}

/// The praxec MCP-server binary the live gateway spawns. Overridable via
/// `$PRAXEC_BIN`; defaults to `px` on `PATH`.
fn praxec_binary() -> String {
    std::env::var("PRAXEC_BIN").unwrap_or_else(|_| "px".to_string())
}

/// Live gateway: a thin MCP-stdio client. The cockpit is a *client* of the same
/// gateway the model uses, so it spawns the praxec binary (pointed at the
/// shared config/store) and reads a mission via `praxec.query`. The blocking
/// `Gateway::get` bridges to the async rmcp call on the cockpit's tokio handle.
pub struct StdioGateway {
    service: RunningService<RoleClient, ()>,
    handle: tokio::runtime::Handle,
}

impl StdioGateway {
    /// Connect: spawn `praxec` over stdio and run the MCP init handshake.
    /// `config_path` becomes the child's `PRAXEC_CONFIG`, so it reads the same
    /// store the running praxec writes (that's how the cockpit sees real
    /// missions). Call from the cockpit's tokio runtime.
    pub async fn connect(
        handle: tokio::runtime::Handle,
        config_path: Option<&str>,
    ) -> Result<Self> {
        let binary = praxec_binary();
        let mut cmd = tokio::process::Command::new(&binary);
        if let Some(p) = config_path {
            cmd.env("PRAXEC_CONFIG", p);
        }
        let transport = TokioChildProcess::new(cmd)
            .with_context(|| format!("spawning praxec binary '{binary}'"))?;
        let service = ServiceExt::serve((), transport)
            .await
            .context("rmcp client init against the praxec child process")?;
        Ok(Self { service, handle })
    }

    /// Call `praxec.query { workflowId }` and decode the gateway response.
    /// The async fetch — call directly from an async context (e.g. startup);
    /// `Gateway::get` is the blocking wrapper for the sync event loop.
    pub async fn fetch(&self, workflow_id: &str) -> Result<GatewayResponse> {
        let args = json!({ "workflowId": workflow_id });
        let params = CallToolRequestParams::new("praxec.query".to_string())
            .with_arguments(args.as_object().cloned().unwrap_or_default());
        let result = self
            .service
            .peer()
            .call_tool(params)
            .await
            .map_err(|e| anyhow::anyhow!("praxec.query failed for '{workflow_id}': {e}"))?;
        if result.is_error.unwrap_or(false) {
            return Err(anyhow::anyhow!(
                "praxec.query returned is_error for '{workflow_id}'"
            ));
        }
        // Success bodies arrive as structured content (the gateway response JSON).
        decode(result, workflow_id)
    }

    /// Submit a transition via `praxec.command`. The async write — `command`
    /// is the blocking wrapper. Returns the post-transition gateway response.
    pub async fn submit(
        &self,
        workflow_id: &str,
        expected_version: u64,
        transition: &str,
    ) -> Result<GatewayResponse> {
        let args = json!({
            "workflowId": workflow_id,
            "expectedVersion": expected_version,
            "transition": transition,
        });
        let params = CallToolRequestParams::new("praxec.command".to_string())
            .with_arguments(args.as_object().cloned().unwrap_or_default());
        let result = self
            .service
            .peer()
            .call_tool(params)
            .await
            .map_err(|e| anyhow::anyhow!("praxec.command '{transition}' failed: {e}"))?;
        if result.is_error.unwrap_or(false) {
            return Err(anyhow::anyhow!(
                "praxec.command '{transition}' was rejected (stale version or not your move)"
            ));
        }
        decode(result, workflow_id)
    }

    /// Start a workflow/agent via `praxec.command { definitionId, input }` (the
    /// §32 start dispatch). The async launch — `Gateway::launch` is the blocking
    /// wrapper. Returns the new mission's response (carrying its fresh id).
    pub async fn start(&self, definition_id: &str, input: Value) -> Result<GatewayResponse> {
        let args = json!({ "definitionId": definition_id, "input": input });
        let params = CallToolRequestParams::new("praxec.command".to_string())
            .with_arguments(args.as_object().cloned().unwrap_or_default());
        let result = self
            .service
            .peer()
            .call_tool(params)
            .await
            .map_err(|e| anyhow::anyhow!("launching '{definition_id}' failed: {e}"))?;
        if result.is_error.unwrap_or(false) {
            return Err(anyhow::anyhow!(
                "launching '{definition_id}' was rejected (check its required input)"
            ));
        }
        decode(result, definition_id)
    }

    /// Call `praxec.query { query: "" }` (unfiltered search) and decode the
    /// library listing. The async read — `Gateway::library` is the blocking
    /// wrapper.
    pub async fn fetch_library(&self) -> Result<Vec<LibraryEntry>> {
        let args = json!({ "query": "" });
        let params = CallToolRequestParams::new("praxec.query".to_string())
            .with_arguments(args.as_object().cloned().unwrap_or_default());
        let result = self
            .service
            .peer()
            .call_tool(params)
            .await
            .map_err(|e| anyhow::anyhow!("praxec.query (library) failed: {e}"))?;
        if result.is_error.unwrap_or(false) {
            return Err(anyhow::anyhow!("praxec.query (library) returned is_error"));
        }
        let value = result.structured_content.unwrap_or(Value::Null);
        let listing: LibraryListing =
            serde_json::from_value(value).context("decoding the library listing")?;
        Ok(listing.into_entries())
    }

    /// Call `praxec.query { definitionId }` and decode the definition body +
    /// hash. The async read — `Gateway::read_definition` is the blocking wrapper.
    pub async fn fetch_definition(&self, definition_id: &str) -> Result<DefinitionDetail> {
        let args = json!({ "definitionId": definition_id });
        let params = CallToolRequestParams::new("praxec.query".to_string())
            .with_arguments(args.as_object().cloned().unwrap_or_default());
        let result = self.service.peer().call_tool(params).await.map_err(|e| {
            anyhow::anyhow!("praxec.query (definition '{definition_id}') failed: {e}")
        })?;
        if result.is_error.unwrap_or(false) {
            return Err(anyhow::anyhow!("definition '{definition_id}' not found"));
        }
        let value = result.structured_content.unwrap_or(Value::Null);
        serde_json::from_value(value)
            .with_context(|| format!("decoding definition '{definition_id}'"))
    }
}

/// Extract the gateway-response JSON from a tool result and decode it.
fn decode(result: rmcp::model::CallToolResult, workflow_id: &str) -> Result<GatewayResponse> {
    let value = result
        .structured_content
        .or_else(|| (!result.content.is_empty()).then(|| json!({ "content": result.content })))
        .unwrap_or(Value::Null);
    serde_json::from_value(value)
        .with_context(|| format!("decoding the gateway response for '{workflow_id}'"))
}

impl Gateway for StdioGateway {
    fn get(&self, workflow_id: &str) -> Result<GatewayResponse> {
        // Bridge the sync read surface to the async rmcp call. Called from the
        // cockpit's sync event loop (not inside an async task), so block_on on
        // the runtime handle is safe.
        self.handle.block_on(self.fetch(workflow_id))
    }

    fn command(
        &self,
        workflow_id: &str,
        expected_version: u64,
        transition: &str,
    ) -> Result<GatewayResponse> {
        self.handle
            .block_on(self.submit(workflow_id, expected_version, transition))
    }

    fn launch(&self, definition_id: &str, input: Value) -> Result<GatewayResponse> {
        self.handle.block_on(self.start(definition_id, input))
    }

    fn library(&self) -> Result<Vec<LibraryEntry>> {
        self.handle.block_on(self.fetch_library())
    }

    fn read_definition(&self, definition_id: &str) -> Result<DefinitionDetail> {
        self.handle.block_on(self.fetch_definition(definition_id))
    }
}

/// Fixture-backed gateway for development and tests.
pub struct FakeGateway {
    response: GatewayResponse,
    library: Vec<LibraryEntry>,
}

impl FakeGateway {
    /// Build from a raw JSON string (e.g. a fixture).
    pub fn from_json(json: &str) -> Result<Self> {
        Ok(Self {
            response: serde_json::from_str(json)?,
            library: Self::demo_library(),
        })
    }

    /// The bundled `run_editing` fixture — a safe-refactor mission mid-edit.
    pub fn editing_demo() -> Self {
        Self::from_json(include_str!("../fixtures/run_editing.json"))
            .expect("bundled fixture is valid")
    }

    /// A small layered library spanning a couple of source repos, for Build-mode
    /// development and render tests.
    fn demo_library() -> Vec<LibraryEntry> {
        let listing: LibraryListing =
            serde_json::from_str(include_str!("../fixtures/library.json"))
                .expect("bundled library fixture is valid");
        listing.into_entries()
    }
}

impl Gateway for FakeGateway {
    fn get(&self, _workflow_id: &str) -> Result<GatewayResponse> {
        Ok(self.response.clone())
    }

    /// The fixture is static, so a command is a no-op echo (development only).
    fn command(&self, _id: &str, _v: u64, _transition: &str) -> Result<GatewayResponse> {
        Ok(self.response.clone())
    }

    /// Synthetic launch for development/tests: a freshly-started running mission
    /// keyed by the definition id. Real instances come from the live gateway.
    fn launch(&self, definition_id: &str, _input: Value) -> Result<GatewayResponse> {
        if !self.library.iter().any(|e| e.id == definition_id) {
            return Err(anyhow::anyhow!("definition '{definition_id}' not found"));
        }
        let id = format!("wf_launched_{}", definition_id.replace('/', "_"));
        let resp = json!({
            "workflow": { "id": id, "definitionId": definition_id, "state": "started", "version": 0 },
            "result": { "status": "running" },
            "links": [],
        });
        Ok(serde_json::from_value(resp)?)
    }

    fn library(&self) -> Result<Vec<LibraryEntry>> {
        Ok(self.library.clone())
    }

    /// A synthetic body for development: a two-state stub keyed by the id, with
    /// a deterministic pseudo-hash. Real bodies come from the live gateway.
    fn read_definition(&self, definition_id: &str) -> Result<DefinitionDetail> {
        if !self.library.iter().any(|e| e.id == definition_id) {
            return Err(anyhow::anyhow!("definition '{definition_id}' not found"));
        }
        let definition = json!({
            "initialState": "start",
            "states": { "start": { "transitions": { "go": { "target": "done" } } },
                        "done": { "terminal": true } }
        });
        Ok(DefinitionDetail {
            definition_id: definition_id.to_string(),
            definition,
            hash: format!("sha256:fake-{}", definition_id.len()),
        })
    }
}

/// A shared, cloneable record of what a [`ScriptedGateway`] received — held by the
/// test even after the gateway is boxed into `App.conn` (so the §32 calls the
/// cockpit makes are assertable).
#[derive(Clone, Default)]
pub struct GatewayLog {
    pub commands: std::sync::Arc<std::sync::Mutex<Vec<(String, u64, String)>>>,
    pub launches: std::sync::Arc<std::sync::Mutex<Vec<(String, Value)>>>,
}

impl GatewayLog {
    /// `(workflow_id, expected_version, transition)` of every command.
    pub fn commands(&self) -> Vec<(String, u64, String)> {
        self.commands
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .clone()
    }
    /// `(definition_id, input)` of every launch.
    pub fn launches(&self) -> Vec<(String, Value)> {
        self.launches
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .clone()
    }
}

/// A **scriptable** test gateway (testing-strategy S1). Unlike [`FakeGateway`]
/// (static), it hands back a *queue* of responses for `get`/`command`/`launch`
/// (in order) and **records** every command + launch into a shared [`GatewayLog`]
/// — so a cockpit flow (launch → submit → status flips → resolve) can be driven
/// deterministically and the §32 calls asserted. An empty queue is an error (the
/// script ran short).
pub struct ScriptedGateway {
    responses: std::sync::Mutex<std::collections::VecDeque<GatewayResponse>>,
    library: Vec<LibraryEntry>,
    log: GatewayLog,
}

impl ScriptedGateway {
    /// Build from the response sequence `get`/`command`/`launch` will return.
    pub fn new(responses: Vec<GatewayResponse>) -> Self {
        Self {
            responses: std::sync::Mutex::new(responses.into()),
            library: Vec::new(),
            log: GatewayLog::default(),
        }
    }

    /// Set the Build-mode library this gateway serves.
    pub fn with_library(mut self, library: Vec<LibraryEntry>) -> Self {
        self.library = library;
        self
    }

    /// A shared handle to the call log — clone it before boxing the gateway.
    pub fn log(&self) -> GatewayLog {
        self.log.clone()
    }

    fn pop(&self) -> Result<GatewayResponse> {
        self.responses
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .pop_front()
            .ok_or_else(|| anyhow::anyhow!("ScriptedGateway: response queue exhausted"))
    }
}

impl Gateway for ScriptedGateway {
    fn get(&self, _workflow_id: &str) -> Result<GatewayResponse> {
        self.pop()
    }

    fn command(
        &self,
        workflow_id: &str,
        expected_version: u64,
        transition: &str,
    ) -> Result<GatewayResponse> {
        self.log
            .commands
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .push((
                workflow_id.to_string(),
                expected_version,
                transition.to_string(),
            ));
        self.pop()
    }

    fn launch(&self, definition_id: &str, input: Value) -> Result<GatewayResponse> {
        self.log
            .launches
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .push((definition_id.to_string(), input));
        self.pop()
    }

    fn library(&self) -> Result<Vec<LibraryEntry>> {
        Ok(self.library.clone())
    }

    fn read_definition(&self, definition_id: &str) -> Result<DefinitionDetail> {
        Ok(DefinitionDetail {
            definition_id: definition_id.to_string(),
            definition: json!({}),
            hash: format!("sha256:scripted-{}", definition_id.len()),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fake_gateway_returns_fixture_response() {
        let gw = FakeGateway::editing_demo();
        let r = gw.get("wf_safe_refactor_01").unwrap();
        assert_eq!(r.workflow.state, "editing");
        assert_eq!(r.legal_actions().len(), 2);
    }
}
