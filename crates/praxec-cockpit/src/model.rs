//! Typed mirror of `schemas/workflow-response.schema.json` — only the fields
//! the cockpit renders. Unknown fields are ignored (forward-compatible).

use serde::Deserialize;
use serde_json::Value;

#[derive(Debug, Clone, Deserialize)]
pub struct GatewayResponse {
    pub workflow: WorkflowSnapshot,
    pub result: ResultBlock,
    #[serde(default)]
    pub context: Value,
    #[serde(default)]
    pub summary: Option<String>,
    #[serde(default)]
    pub delegate: Option<String>,
    /// ADR-0009 — the workflow's orchestrator binding (the actor driving the
    /// mission), surfaced by the runtime. Present only when declared.
    #[serde(default)]
    pub orchestrator: Option<String>,
    #[serde(default)]
    pub guidance: Option<Guidance>,
    #[serde(default)]
    pub links: Vec<Link>,
    /// ADR-0008 — the mission's outcomes (its measurable definition of done),
    /// with a live `met` flag each. Present only when the workflow declares them.
    #[serde(default)]
    pub outcomes: Vec<Outcome>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct WorkflowSnapshot {
    pub id: String,
    #[serde(rename = "definitionId")]
    pub definition_id: String,
    pub state: String,
    pub version: u64,
}

#[derive(Debug, Clone, Deserialize)]
pub struct ResultBlock {
    /// ADR-0008 — `running | waiting | succeeded | failed`.
    pub status: String,
    /// Present when `status == "failed"`: `cancelled | timed_out | guard_unmet | error`.
    #[serde(default)]
    pub reason: Option<String>,
}

/// ADR-0008 — one outcome on a mission's definition of done, evaluated live.
#[derive(Debug, Clone, Deserialize)]
pub struct Outcome {
    #[serde(default)]
    pub id: String,
    #[serde(default)]
    pub statement: String,
    #[serde(default)]
    pub met: bool,
}

#[derive(Debug, Clone, Deserialize)]
pub struct Guidance {
    #[serde(default)]
    pub goal: Option<String>,
    #[serde(default)]
    pub instructions: Option<String>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct Link {
    pub rel: String,
    #[serde(default)]
    pub title: Option<String>,
    #[serde(default)]
    pub actor: Option<String>,
}

impl GatewayResponse {
    /// Legal next actions a human/agent may take from the current state.
    pub fn legal_actions(&self) -> &[Link] {
        &self.links
    }
}

/// One discoverable definition in the layered library (Build mode). Decoded
/// from a `praxec.query` search hit's `item` — only the fields Build renders.
#[derive(Debug, Clone, Deserialize)]
pub struct LibraryEntry {
    pub id: String,
    #[serde(default)]
    pub kind: String,
    #[serde(default)]
    pub title: String,
    #[serde(default)]
    pub description: String,
}

impl LibraryEntry {
    /// The owning repo namespace — the `<ns>/` prefix of a layered id
    /// (`cognitive/flow.x` → `cognitive`). `None` for an unprefixed local id.
    pub fn namespace(&self) -> Option<&str> {
        self.id.split_once('/').map(|(ns, _)| ns)
    }
}

/// A definition's current body + content hash, from
/// `praxec.query { definitionId }` — the basis an author reads before editing.
#[derive(Debug, Clone, Deserialize)]
pub struct DefinitionDetail {
    #[serde(rename = "definitionId")]
    pub definition_id: String,
    #[serde(default)]
    pub definition: Value,
    #[serde(default)]
    pub hash: String,
}

/// The shape of a `praxec.query { query: "" }` (search) response — the
/// library listing. Each hit wraps the scored `item` we care about.
#[derive(Debug, Clone, Deserialize)]
pub struct LibraryListing {
    #[serde(default)]
    pub items: Vec<LibraryHit>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct LibraryHit {
    pub item: LibraryEntry,
}

impl LibraryListing {
    /// Flatten the hits into entries, sorted by id for a stable browse order.
    pub fn into_entries(self) -> Vec<LibraryEntry> {
        let mut entries: Vec<LibraryEntry> = self.items.into_iter().map(|h| h.item).collect();
        entries.sort_by(|a, b| a.id.cmp(&b.id));
        entries
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const FIXTURE: &str = include_str!("../fixtures/run_editing.json");

    #[test]
    fn parses_fixture_into_typed_response() {
        let r: GatewayResponse = serde_json::from_str(FIXTURE).unwrap();
        assert_eq!(r.workflow.id, "wf_safe_refactor_01");
        assert_eq!(r.workflow.definition_id, "cognitive/flow.safe-refactor");
        assert_eq!(r.workflow.state, "editing");
        assert_eq!(r.workflow.version, 4);
        assert_eq!(r.result.status, "running");
        assert_eq!(r.delegate.as_deref(), Some("reasoning"));
        assert_eq!(r.legal_actions().len(), 2);
        assert_eq!(r.legal_actions()[0].rel, "edits_produced");
    }
}
