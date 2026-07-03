//! Contract test C2 (testing-strategy) — the cockpit's typed `GatewayResponse`
//! mirror is **consumer-driven-consistent with `workflow-response.schema.json`**.
//!
//! `GatewayResponse` is a `Deserialize`-only *subset* of the full §32 response —
//! it models only the fields the cockpit reads. The risk is silent drift: if the
//! runtime renames a field the cockpit depends on (e.g. `met` → `satisfied`), the
//! `#[serde(default)]` mirror would quietly read a default and the UI would lie.
//!
//! The mechanism: build one *maximal* response, **validate it against the schema**
//! (so the fixture itself is pinned to the contract — rename a field in the schema
//! and this fixture stops conforming), then deserialize it and assert every field
//! the cockpit actually reads survived. One read per test (atomic).

use praxec_cockpit::model::GatewayResponse;
use serde_json::{Value, json};

const SCHEMA: &str = include_str!("../../../schemas/workflow-response.schema.json");

/// A maximal §32 response exercising every slot the cockpit reads: the typed
/// status + fail reason, the workflow snapshot (incl. the optimistic-lock
/// version), a human-actor link (the mediator's keystone), the outcomes
/// checklist, the orchestrator binding, and guidance.
fn response() -> Value {
    json!({
        "workflow": { "id": "wf_1", "definitionId": "flow.x", "state": "review", "version": 4 },
        "result": { "status": "failed", "reason": "guard_unmet" },
        "context": { "approved": false },
        "orchestrator": "anthropic:claude-sonnet-4-6",
        "guidance": { "goal": "Review and approve the change." },
        "outcomes": [
            { "id": "approved", "statement": "A human approved the change.", "met": false }
        ],
        "links": [
            {
                "rel": "approve",
                "title": "approve",
                "actor": "human",
                "method": "praxec.command",
                "args": { "workflowId": "wf_1", "expectedVersion": 4, "transition": "approve" }
            }
        ]
    })
}

fn decoded() -> GatewayResponse {
    serde_json::from_value(response())
        .expect("a schema-valid response deserializes into the mirror")
}

#[test]
fn the_fixture_conforms_to_the_response_schema() {
    // The contract gate: if this fails, the fixture (and thus the cockpit's
    // assumptions) drifted from the §32 schema — fix the mirror, not the schema.
    let schema: Value = serde_json::from_str(SCHEMA).expect("the response schema is valid JSON");
    let validator = jsonschema::validator_for(&schema).expect("the response schema compiles");
    let errors: Vec<String> = validator
        .iter_errors(&response())
        .map(|e| e.to_string())
        .collect();
    assert!(
        errors.is_empty(),
        "the C2 fixture violates the response schema:\n{errors:#?}"
    );
}

#[test]
fn the_cockpit_reads_the_mission_status() {
    assert_eq!(decoded().result.status, "failed");
}

#[test]
fn the_cockpit_reads_the_fail_reason() {
    assert_eq!(decoded().result.reason.as_deref(), Some("guard_unmet"));
}

#[test]
fn the_cockpit_reads_the_workflow_version() {
    // The optimistic-lock keystone — a wrong read here corrupts every submit.
    assert_eq!(decoded().workflow.version, 4);
}

#[test]
fn the_cockpit_reads_the_workflow_definition_id() {
    assert_eq!(decoded().workflow.definition_id, "flow.x");
}

#[test]
fn the_cockpit_reads_the_human_actor_on_a_link() {
    // The mediator's keystone: inbox membership is `link.actor == "human"`.
    let links = decoded().links;
    assert_eq!(
        links.first().and_then(|l| l.actor.clone()).as_deref(),
        Some("human")
    );
}

#[test]
fn the_cockpit_reads_the_link_rel() {
    assert_eq!(
        decoded().links.first().map(|l| l.rel.clone()),
        Some("approve".to_string())
    );
}

#[test]
fn the_cockpit_reads_the_outcome_met_flag() {
    assert_eq!(decoded().outcomes.first().map(|o| o.met), Some(false));
}

#[test]
fn the_cockpit_reads_the_outcome_statement() {
    assert_eq!(
        decoded().outcomes.first().map(|o| o.statement.clone()),
        Some("A human approved the change.".to_string())
    );
}

#[test]
fn the_cockpit_reads_the_orchestrator_binding() {
    assert_eq!(
        decoded().orchestrator.as_deref(),
        Some("anthropic:claude-sonnet-4-6")
    );
}

#[test]
fn the_cockpit_reads_the_guidance_goal() {
    assert_eq!(
        decoded().guidance.and_then(|g| g.goal).as_deref(),
        Some("Review and approve the change.")
    );
}
