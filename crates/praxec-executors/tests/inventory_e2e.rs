//! End-to-end: `cap.research.tool-inventory`'s deterministic `inventory`
//! executor completes on `start` and maps its survey into the cap's declared
//! `inventory` output.
//!
//! This pins the fix for the "whole meta authoring pack unusable" defect: the
//! step must complete instantly via the deterministic chain (no agent, no step
//! budget) AND its output must flow through the transition's `$.output.*`
//! mapping into the workflow's terminal output — the exact seam a parent
//! authoring flow reads when it composes against the inventory.

use std::sync::Arc;

use async_trait::async_trait;
use praxec_core::audit::{AuditSink, MemoryAuditSink};
use praxec_core::discovery::{DiscoveryIndex, DiscoveryItem, DiscoveryKind, SearchHit, SearchRequest};
use praxec_core::guards::DefaultGuardEvaluator;
use praxec_core::model::{Principal, StartWorkflow};
use praxec_core::ports::{Executor, ExecutorRegistry, WorkflowStore};
use praxec_core::runtime::WorkflowRuntime;
use praxec_core::store::{ConfigDefinitionStore, InMemoryEvidenceStore, InMemoryWorkflowStore};
use praxec_executors::InventoryExecutor;
use serde_json::{Value, json};

/// A discovery index with a fixed, mixed-kind item set standing in for a live
/// gateway catalog.
struct FixedIndex {
    items: Vec<DiscoveryItem>,
}

fn item(id: &str, kind: DiscoveryKind) -> DiscoveryItem {
    DiscoveryItem {
        id: id.to_string(),
        kind,
        title: format!("title of {id}"),
        description: format!("what {id} does"),
        tags: vec![],
        examples: vec![],
        aliases: vec![],
        text: String::new(),
        links: vec![],
        verb: Some("research".to_string()),
        body: None,
        source: Some("config".to_string()),
        structural_fingerprint: None,
    }
}

#[async_trait]
impl DiscoveryIndex for FixedIndex {
    async fn search(&self, _request: SearchRequest) -> anyhow::Result<Vec<SearchHit>> {
        Ok(vec![])
    }
    async fn describe(&self, _id: &str) -> anyhow::Result<Option<DiscoveryItem>> {
        Ok(None)
    }
    async fn list(&self, kind: Option<DiscoveryKind>) -> anyhow::Result<Vec<DiscoveryItem>> {
        Ok(match kind {
            None => self.items.clone(),
            Some(k) => self.items.iter().filter(|i| i.kind == k).cloned().collect(),
        })
    }
}

/// Registry mapping the `inventory` kind to the real executor; everything else
/// is unused in this single-transition cap.
struct InventoryOnlyRegistry {
    inventory: Arc<dyn Executor>,
}
impl ExecutorRegistry for InventoryOnlyRegistry {
    fn get(&self, kind: &str) -> Option<Arc<dyn Executor>> {
        match kind {
            "inventory" => Some(self.inventory.clone()),
            _ => None,
        }
    }
}

/// The cap definition, matching the shipped praxec-meta shape: a single
/// deterministic `survey` transition that runs `kind: inventory` and maps
/// `$.output.inventory` into the declared `inventory` output.
fn cap_config() -> Value {
    json!({
        "version": "1.0.0",
        "workflows": {
            "cap.research.tool-inventory": {
                "initialState": "ready",
                "verb": "research",
                "snippet": { "outputs": { "inventory": { "type": "object" } } },
                "states": {
                    "ready": {
                        "transitions": {
                            "survey": {
                                "target": "done",
                                "actor": "deterministic",
                                "executor": { "kind": "inventory" },
                                "output": { "inventory": "$.output.inventory" }
                            }
                        }
                    },
                    "done": { "terminal": true }
                }
            }
        }
    })
}

fn build_runtime(discovery: Arc<dyn DiscoveryIndex>) -> WorkflowRuntime {
    let config = cap_config();
    let audit = Arc::new(MemoryAuditSink::new());
    let definitions = Arc::new(ConfigDefinitionStore::from_config(&config));
    let store: Arc<dyn WorkflowStore> = Arc::new(InMemoryWorkflowStore::new());
    let evidence = Arc::new(InMemoryEvidenceStore::new());
    let guards = Arc::new(DefaultGuardEvaluator::with_evidence(evidence.clone()));
    let registry = Arc::new(InventoryOnlyRegistry {
        inventory: Arc::new(InventoryExecutor::new(discovery)),
    }) as Arc<dyn ExecutorRegistry>;
    WorkflowRuntime::new(definitions, store, registry, guards, audit as Arc<dyn AuditSink>)
        .with_evidence(evidence)
}

#[tokio::test]
async fn tool_inventory_cap_surveys_on_start_and_maps_output() {
    let discovery = Arc::new(FixedIndex {
        items: vec![
            item("praxec.query", DiscoveryKind::Tool),
            item("build.cargo", DiscoveryKind::Script),
            item("fmeca.apply", DiscoveryKind::Guidance),
            item("cap.research.tool-inventory", DiscoveryKind::Capability),
            item("github", DiscoveryKind::Connection),
        ],
    });
    let runtime = build_runtime(discovery);

    let resp = runtime
        .start(StartWorkflow {
            definition_id: "cap.research.tool-inventory".into(),
            input: json!({}),
            principal: Principal::anonymous(),
            trace_id: None,
            run_id: None,
            depth: 0,
            parent: None,
        })
        .await
        .expect("start returns a response");

    // Completed instantly via the deterministic chain — no agent, no budget.
    assert_eq!(resp["workflow"]["state"], "done", "resp: {resp:#}");
    assert_eq!(resp["result"]["status"], "succeeded", "resp: {resp:#}");

    // The survey flowed through `$.output.inventory` into the workflow output
    // (surfaced on the response's top-level `context`).
    let inventory = resp
        .pointer("/context/inventory")
        .unwrap_or_else(|| panic!("inventory output must surface; resp: {resp:#}"));
    assert_eq!(inventory["counts"]["total"], 5, "resp: {resp:#}");
    assert_eq!(inventory["mcp_tools"].as_array().unwrap().len(), 1);
    assert_eq!(inventory["capabilities"][0]["id"], "cap.research.tool-inventory");
}
