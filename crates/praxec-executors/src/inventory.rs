//! `inventory` executor — deterministic gateway self-survey.
//!
//! `cap.research.tool-inventory` (praxec-meta) once asked an LLM **agent** to
//! "survey the live gateway for reachable tools/scripts/skills/caps." But the
//! agent had no tool with which to enumerate the gateway, so it burned the
//! whole step budget producing nothing (`AGENT_STEP_BUDGET_EXHAUSTED` across two
//! models), leaving the *entire* meta authoring pack unusable — you could not
//! mint or improve any workflow through praxec.
//!
//! Surveying the gateway's OWN registry is inherently deterministic: the
//! [`DiscoveryIndex`] already holds every reachable item, and it is the same
//! surface `praxec.query` reads. This executor reads it directly and emits the
//! typed inventory in one instant, governed step — no model, no budget, no
//! hallucinated tools. Wired as a `deterministic` transition, it completes on
//! `start` via the runtime's deterministic chain: the survey is free.

use std::sync::Arc;

use async_trait::async_trait;
use praxec_core::discovery::{DiscoveryIndex, DiscoveryItem, DiscoveryKind};
use praxec_core::error::ExecutorError;
use praxec_core::model::{ExecuteRequest, ExecuteResult};
use praxec_core::ports::Executor;
use serde_json::{Value, json};

/// Reads the live discovery index and emits `{ inventory: {...} }`. Holds the
/// SAME `Arc<dyn DiscoveryIndex>` the gateway swaps on hot-reload (the binary
/// hands it a [`crate::SwappableDiscoveryIndex`]-backed handle), so a reload
/// that re-indexes is reflected in the next survey with nothing to re-wire.
pub struct InventoryExecutor {
    discovery: Arc<dyn DiscoveryIndex>,
}

impl InventoryExecutor {
    pub fn new(discovery: Arc<dyn DiscoveryIndex>) -> Self {
        Self { discovery }
    }
}

/// Project one discovery item into an inventory entry. Carries exactly the
/// fields the downstream compose step reasons over — id/title/description and
/// the verb — plus provenance (`source`), so a reviewer can audit WHERE each
/// entry came from (the skill's "inventory without provenance" anti-pattern).
fn entry(item: &DiscoveryItem) -> Value {
    json!({
        "id": item.id,
        "title": item.title,
        "description": item.description,
        "verb": item.verb,
        "source": item.source,
        "tags": item.tags,
    })
}

#[async_trait]
impl Executor for InventoryExecutor {
    async fn execute(&self, _request: ExecuteRequest) -> Result<ExecuteResult, ExecutorError> {
        let items = self.discovery.list(None).await.map_err(|e| {
            ExecutorError::Permanent(format!("inventory: discovery list failed: {e}"))
        })?;

        // Group by the discovery kind into the five categories the
        // `research.tool-inventory.assemble` skill defines, plus `workflows`
        // and `agents` (also reachable, also composable). The closed
        // `DiscoveryKind` match means a new kind can't be silently dropped.
        let mut mcp_tools = Vec::new();
        let mut cli_scripts = Vec::new();
        let mut skills = Vec::new();
        let mut capabilities = Vec::new();
        let mut connections = Vec::new();
        let mut workflows = Vec::new();
        let mut agents = Vec::new();

        for item in &items {
            let projected = entry(item);
            match item.kind {
                DiscoveryKind::Tool => mcp_tools.push(projected),
                DiscoveryKind::Script => cli_scripts.push(projected),
                DiscoveryKind::Guidance => skills.push(projected),
                DiscoveryKind::Capability => capabilities.push(projected),
                DiscoveryKind::Connection => connections.push(projected),
                DiscoveryKind::Workflow => workflows.push(projected),
                DiscoveryKind::Agent => agents.push(projected),
            }
        }

        // Counts computed before the arrays are moved into the inventory value,
        // so the planner (and a human reading `observe`) sees the shape at a
        // glance without walking every array.
        let counts = json!({
            "mcp_tools": mcp_tools.len(),
            "cli_scripts": cli_scripts.len(),
            "skills": skills.len(),
            "capabilities": capabilities.len(),
            "connections": connections.len(),
            "workflows": workflows.len(),
            "agents": agents.len(),
            "total": items.len(),
        });

        let inventory = json!({
            "mcp_tools": mcp_tools,
            "cli_scripts": cli_scripts,
            "skills": skills,
            "capabilities": capabilities,
            "connections": connections,
            "workflows": workflows,
            "agents": agents,
            "counts": counts,
        });

        Ok(ExecuteResult {
            output: json!({ "inventory": inventory }),
            evidence: vec![],
            child_workflow_id: None,
            next_transition: None,
            suspend: None,
            telemetry: None,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use async_trait::async_trait;
    use chrono::Utc;
    use praxec_core::discovery::{DiscoveryItem, DiscoveryKind, SearchHit, SearchRequest};
    use praxec_core::model::WorkflowInstance;
    use serde_json::{Value, json};

    /// A discovery index whose `list` returns a fixed, mixed-kind item set.
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

    fn request() -> ExecuteRequest {
        ExecuteRequest {
            workflow: WorkflowInstance {
                id: "wf_inv".into(),
                definition_id: "cap.research.tool-inventory".into(),
                definition_version: "0".into(),
                definition: Value::Null,
                state: "ready".into(),
                version: 0,
                input: json!({}),
                context: json!({}),
                started_at: Utc::now(),
                run_env: praxec_core::RunEnv::for_test(),
                cancelled_at: None,
                cancelled_reason: None,
                depth: 0,
                parent: None,
            },
            transition: Some("submit_inventory".to_string()),
            arguments: json!({}),
            executor_config: json!({ "kind": "inventory" }),
            idempotency_key: None,
            correlation_id: None,
        }
    }

    #[tokio::test]
    async fn surveys_and_groups_by_kind() {
        let index = Arc::new(FixedIndex {
            items: vec![
                item("praxec.query", DiscoveryKind::Tool),
                item("build.cargo", DiscoveryKind::Script),
                item("fmeca.apply", DiscoveryKind::Guidance),
                item("cap.research.tool-inventory", DiscoveryKind::Capability),
                item("github", DiscoveryKind::Connection),
                item("flow.author", DiscoveryKind::Workflow),
                item("coder", DiscoveryKind::Agent),
            ],
        });
        let exec = InventoryExecutor::new(index);
        let result = exec.execute(request()).await.expect("inventory runs");
        let inv = &result.output["inventory"];

        assert_eq!(inv["mcp_tools"].as_array().unwrap().len(), 1);
        assert_eq!(inv["cli_scripts"].as_array().unwrap().len(), 1);
        assert_eq!(inv["skills"].as_array().unwrap().len(), 1);
        assert_eq!(inv["capabilities"].as_array().unwrap().len(), 1);
        assert_eq!(inv["connections"].as_array().unwrap().len(), 1);
        assert_eq!(inv["workflows"].as_array().unwrap().len(), 1);
        assert_eq!(inv["agents"].as_array().unwrap().len(), 1);
        assert_eq!(inv["counts"]["total"], 7);
        // Provenance survives into every entry.
        assert_eq!(inv["mcp_tools"][0]["source"], "config");
        assert_eq!(inv["capabilities"][0]["id"], "cap.research.tool-inventory");
    }

    #[tokio::test]
    async fn empty_gateway_yields_empty_typed_inventory() {
        let exec = InventoryExecutor::new(Arc::new(FixedIndex { items: vec![] }));
        let result = exec.execute(request()).await.expect("inventory runs");
        let inv = &result.output["inventory"];
        // Every category present and empty — a typed shape the planner can
        // compose against, never a missing key.
        assert_eq!(inv["mcp_tools"].as_array().unwrap().len(), 0);
        assert_eq!(inv["counts"]["total"], 0);
    }
}
