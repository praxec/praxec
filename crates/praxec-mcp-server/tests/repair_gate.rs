//! #14 — repair-only gate enforcement. When the loaded pack is contract-dirty,
//! only the declared repair surface may be STARTED; every other start is refused
//! with the precise diagnostics + the callable repair links.

use std::collections::HashSet;
use std::sync::Arc;

use async_trait::async_trait;
use praxec_core::WorkflowRuntime;
use praxec_core::audit::{AuditSink, MemoryAuditSink};
use praxec_core::error::ExecutorError;
use praxec_core::guards::DefaultGuardEvaluator;
use praxec_core::model::{ExecuteRequest, ExecuteResult, Principal};
use praxec_core::ports::{Executor, ExecutorRegistry};
use praxec_core::store::{ConfigDefinitionStore, InMemoryWorkflowStore};
use praxec_mcp_server::{PraxecServer, RepairGate};
use rmcp::model::CallToolRequestParams;
use serde_json::json;

struct NoopRegistry;
struct Noop;
#[async_trait]
impl Executor for Noop {
    async fn execute(&self, _r: ExecuteRequest) -> Result<ExecuteResult, ExecutorError> {
        Ok(ExecuteResult::default())
    }
}
impl ExecutorRegistry for NoopRegistry {
    fn get(&self, _kind: &str) -> Option<Arc<dyn Executor>> {
        Some(Arc::new(Noop))
    }
}

/// Server with a `flow.repair` (the repair surface) and a `flow.normal`
/// (functional), gated so only `flow.repair` may start.
async fn gated_server() -> PraxecServer {
    let cfg = json!({
        "version": "1.0.0",
        "workflows": {
            "flow.repair": {
                "initialState": "s",
                "states": { "s": { "transitions": {
                    "go": { "target": "done", "actor": "deterministic" }
                }}, "done": { "terminal": true } }
            },
            "flow.normal": {
                "initialState": "s",
                "states": { "s": { "transitions": {
                    "go": { "target": "done", "actor": "deterministic" }
                }}, "done": { "terminal": true } }
            }
        }
    });
    let resolved = praxec_core::config::resolve(cfg).expect("resolve");
    let defs = Arc::new(ConfigDefinitionStore::from_config(&resolved));
    let store = Arc::new(InMemoryWorkflowStore::new());
    let audit = Arc::new(MemoryAuditSink::new());
    let guards = Arc::new(DefaultGuardEvaluator::new());
    let runtime = WorkflowRuntime::new(
        defs,
        store,
        Arc::new(NoopRegistry),
        guards,
        audit as Arc<dyn AuditSink>,
    )
    .with_writable_repo_roots(vec![praxec_core::RepoRoot::for_test()]);
    let gate = RepairGate {
        diagnostics: vec![
            "USE_BINDING_CONTRACT_DRIFT: workflow 'flow.normal' … maps `use.inputs.filter` …"
                .into(),
        ],
        repair_ids: HashSet::from(["flow.repair".to_string()]),
    };
    PraxecServer::new(runtime).with_repair_gate(Some(gate))
}

fn start(def_id: &str) -> CallToolRequestParams {
    let mut req = CallToolRequestParams::new("praxec.command");
    let mut args = serde_json::Map::new();
    args.insert("definitionId".into(), json!(def_id));
    args.insert("input".into(), json!({}));
    req.arguments = Some(args);
    req
}

#[tokio::test]
async fn dirty_gateway_refuses_starting_a_non_repair_workflow() {
    let server = gated_server().await;
    let resp = server
        .dispatch_call_with_principal(start("flow.normal"), Principal::anonymous())
        .await
        .expect("dispatch returns a structured response");
    assert_eq!(
        resp["error"]["code"], "CONTRACT_DIRTY_REPAIR_ONLY",
        "resp: {resp:#}"
    );
    // Precise diagnostics + the callable repair surface are surfaced.
    assert!(
        resp["diagnostics"].as_array().unwrap()[0]
            .as_str()
            .unwrap()
            .contains("USE_BINDING_CONTRACT_DRIFT")
    );
    assert_eq!(resp["repairSurface"][0], "flow.repair");
    assert_eq!(resp["links"][0]["args"]["definitionId"], "flow.repair");
}

#[tokio::test]
async fn dirty_gateway_allows_starting_the_repair_surface() {
    let server = gated_server().await;
    let resp = server
        .dispatch_call_with_principal(start("flow.repair"), Principal::anonymous())
        .await
        .expect("dispatch returns a structured response");
    // NOT the repair-gate refusal — the repair workflow was allowed to start.
    assert_ne!(
        resp["error"]["code"], "CONTRACT_DIRTY_REPAIR_ONLY",
        "the repair surface must remain startable while dirty: {resp:#}"
    );
    assert_eq!(
        resp["workflow"]["definitionId"], "flow.repair",
        "resp: {resp:#}"
    );
}

#[tokio::test]
async fn a_clean_gateway_starts_anything() {
    // Same config, no gate → both workflows start.
    let server = {
        let s = gated_server().await;
        s.with_repair_gate(None)
    };
    let resp = server
        .dispatch_call_with_principal(start("flow.normal"), Principal::anonymous())
        .await
        .expect("dispatch");
    assert_ne!(
        resp["error"]["code"], "CONTRACT_DIRTY_REPAIR_ONLY",
        "resp: {resp:#}"
    );
    assert_eq!(
        resp["workflow"]["definitionId"], "flow.normal",
        "resp: {resp:#}"
    );
}
