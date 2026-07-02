//! ADR-0009 — the **mediator's cross-mission inbox**.
//!
//! The cockpit is a separate process from the gateway, so it can't consume the
//! in-process [`Bus`](praxec_core::bus) — that's for headless single-process
//! runs. The cross-process equivalent of a *parked* HITL request is a **`waiting`
//! mission whose legal moves belong to a human**: the §32 surface IS the bus here.
//!
//! The mediator scans the roster's missions and gathers those into **one themed
//! inbox** — so the human decides related things in one context instead of
//! bouncing between missions (the cognitive-load point) — and *answers* by
//! submitting the chosen human transition via `praxec.command`.

use crate::model::GatewayResponse;

/// One thing waiting on the human: a parked mission and the moves it offers.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InboxItem {
    pub mission_id: String,
    pub definition_id: String,
    /// The instance version — the optimistic-concurrency guard when answering.
    pub version: u64,
    /// What the human is being asked — the mission's current goal/guidance.
    pub prompt: String,
    /// The human-actor transitions the human may submit to answer.
    pub choices: Vec<String>,
}

/// The cross-mission Needs-You inbox: every `waiting` mission with at least one
/// human-actor legal action, in roster order. A mission that is running, resolved,
/// or only agent-actionable is not the human's concern and is excluded.
pub fn inbox(missions: &[GatewayResponse]) -> Vec<InboxItem> {
    missions.iter().filter_map(item_for).collect()
}

fn item_for(mission: &GatewayResponse) -> Option<InboxItem> {
    if mission.result.status != "waiting" {
        return None;
    }
    let choices: Vec<String> = mission
        .legal_actions()
        .iter()
        .filter(|l| l.actor.as_deref() == Some("human"))
        .map(|l| l.rel.clone())
        .collect();
    if choices.is_empty() {
        return None;
    }
    Some(InboxItem {
        mission_id: mission.workflow.id.clone(),
        definition_id: mission.workflow.definition_id.clone(),
        version: mission.workflow.version,
        prompt: mission
            .guidance
            .as_ref()
            .and_then(|g| g.goal.clone())
            .unwrap_or_default(),
        choices,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn resp(json: serde_json::Value) -> GatewayResponse {
        serde_json::from_value(json).unwrap()
    }

    fn waiting_human(id: &str) -> GatewayResponse {
        resp(serde_json::json!({
            "workflow": { "id": id, "definitionId": "cognitive/flow.review", "state": "review", "version": 2 },
            "result": { "status": "waiting" },
            "guidance": { "goal": "approve the change?" },
            "links": [
                { "rel": "approve", "actor": "human" },
                { "rel": "reject",  "actor": "human" }
            ]
        }))
    }

    #[test]
    fn a_waiting_mission_with_a_human_action_appears_in_the_inbox() {
        assert_eq!(inbox(&[waiting_human("m1")]).len(), 1);
    }

    #[test]
    fn a_running_mission_does_not_appear_in_the_inbox() {
        let running = resp(serde_json::json!({
            "workflow": { "id": "m1", "definitionId": "f", "state": "x", "version": 1 },
            "result": { "status": "running" },
            "links": [ { "rel": "go", "actor": "agent" } ]
        }));
        assert!(inbox(&[running]).is_empty());
    }

    #[test]
    fn a_succeeded_mission_does_not_appear_in_the_inbox() {
        let done = resp(serde_json::json!({
            "workflow": { "id": "m1", "definitionId": "f", "state": "done", "version": 3 },
            "result": { "status": "succeeded" },
            "links": []
        }));
        assert!(inbox(&[done]).is_empty());
    }

    #[test]
    fn a_waiting_mission_with_only_agent_actions_does_not_appear() {
        let agent_only = resp(serde_json::json!({
            "workflow": { "id": "m1", "definitionId": "f", "state": "x", "version": 1 },
            "result": { "status": "waiting" },
            "links": [ { "rel": "go", "actor": "agent" } ]
        }));
        assert!(inbox(&[agent_only]).is_empty());
    }

    #[test]
    fn the_item_carries_the_mission_id() {
        let items = inbox(&[waiting_human("wf_42")]);
        assert_eq!(items.first().map(|i| i.mission_id.as_str()), Some("wf_42"));
    }

    #[test]
    fn the_item_carries_the_definition_id() {
        let items = inbox(&[waiting_human("m1")]);
        assert_eq!(
            items.first().map(|i| i.definition_id.as_str()),
            Some("cognitive/flow.review")
        );
    }

    #[test]
    fn the_item_carries_the_human_choices() {
        let items = inbox(&[waiting_human("m1")]);
        assert_eq!(
            items.first().map(|i| i.choices.clone()),
            Some(vec!["approve".to_string(), "reject".to_string()])
        );
    }

    #[test]
    fn the_item_carries_the_prompt_from_guidance_goal() {
        let items = inbox(&[waiting_human("m1")]);
        assert_eq!(
            items.first().map(|i| i.prompt.as_str()),
            Some("approve the change?")
        );
    }

    #[test]
    fn the_inbox_aggregates_across_multiple_missions() {
        assert_eq!(inbox(&[waiting_human("m1"), waiting_human("m2")]).len(), 2);
    }
}
