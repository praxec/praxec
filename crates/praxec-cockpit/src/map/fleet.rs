//! The fleet (L0) model — every mission as stable terrain. The demo fleet is
//! fixture-backed; the live fleet is built from the cockpit's launch roster
//! (ADR-0008 d1) via [`Fleet::from_roster`].

use crate::model::GatewayResponse;
use crate::view::MissionView;

/// Terrain colour for a mission tile — its overall health at a glance.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Health {
    Running,
    NeedsYou,
    Blocked,
    Failed,
    Done,
}

/// Preattentive attention counts, aggregated from the mission's nodes.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Pins {
    pub needs_you: usize,
    pub blocked: usize,
    pub failed: usize,
}

/// One mission as it appears on the map: terrain + pins at L0, the full
/// task-spine [`MissionView`] when zoomed to L1.
pub struct Mission {
    pub name: String,
    pub orchestrator: String,
    pub health: Health,
    pub pins: Pins,
    pub view: MissionView,
    /// ADR-0008 d1 — the live instance id for a launched mission (`Some`); zoom
    /// fetches the real HATEOAS surface for it. `None` for demo/fixture tiles.
    pub workflow_id: Option<String>,
}

impl Mission {
    fn from_view(health: Health, view: MissionView) -> Self {
        let c = view.counts();
        Mission {
            name: view.name.clone(),
            orchestrator: view.orchestrator.clone(),
            health,
            pins: Pins {
                needs_you: c.needs_you,
                blocked: c.blocked,
                failed: c.failed,
            },
            view,
            workflow_id: None,
        }
    }

    /// A live tile from a launched mission's gateway response — health from its
    /// ADR-0008 resolution status; the real HATEOAS surface is fetched on zoom.
    fn real(response: &GatewayResponse) -> Self {
        let title = response
            .workflow
            .definition_id
            .rsplit('/')
            .next()
            .unwrap_or(&response.workflow.definition_id)
            .to_string();
        Mission {
            name: title.clone(),
            orchestrator: response.orchestrator.clone().unwrap_or_default(),
            health: Health::from_status(&response.result.status),
            pins: Pins::default(),
            view: MissionView::stub(&title, &response.workflow.definition_id),
            workflow_id: Some(response.workflow.id.clone()),
        }
    }
}

impl Health {
    /// Map an ADR-0008 mission resolution status to a terrain colour.
    pub fn from_status(status: &str) -> Self {
        match status {
            "running" => Health::Running,
            "waiting" => Health::NeedsYou,
            "succeeded" => Health::Done,
            "failed" => Health::Failed,
            _ => Health::Running,
        }
    }
}

pub struct Fleet {
    pub missions: Vec<Mission>,
}

impl Fleet {
    /// A fixture fleet for the demo / snapshots. One mission is the real CPM
    /// plan we've been dogfooding (it carries the needs-you ask); the rest are
    /// plausible siblings so the Fleet view has terrain to show.
    pub fn demo() -> Self {
        let m1 = Mission::from_view(Health::NeedsYou, MissionView::demo());
        let m2 = Mission::from_view(
            Health::Running,
            MissionView::stub("Provider catalog unification", "cognitive/flow.refactor"),
        );
        let m3 = Mission::from_view(
            Health::Blocked,
            MissionView::stub("Postgres store migration", "cognitive/flow.migrate"),
        );
        let m4 = Mission::from_view(
            Health::Done,
            MissionView::stub("Help-surface cleanup", "cognitive/flow.tidy"),
        );
        Fleet {
            missions: vec![m1, m2, m3, m4],
        }
    }

    /// The live fleet — one tile per launched mission, built from each instance's
    /// current gateway response (ADR-0008 d1). Order follows the roster (launch
    /// order). Empty when nothing has been launched.
    pub fn from_roster(responses: &[GatewayResponse]) -> Self {
        Fleet {
            missions: responses.iter().map(Mission::real).collect(),
        }
    }

    /// Whether this fleet has any tiles to show.
    pub fn is_empty(&self) -> bool {
        self.missions.is_empty()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn demo_fleet_has_several_missions() {
        assert!(Fleet::demo().missions.len() >= 4);
    }

    #[test]
    fn a_mission_carries_its_task_spine_view() {
        let f = Fleet::demo();
        assert!(!f.missions[0].view.nodes.is_empty());
    }

    #[test]
    fn at_least_one_mission_needs_you() {
        let f = Fleet::demo();
        assert!(f.missions.iter().any(|m| m.pins.needs_you > 0));
    }

    #[test]
    fn status_maps_to_terrain_health() {
        assert_eq!(Health::from_status("running"), Health::Running);
        assert_eq!(Health::from_status("waiting"), Health::NeedsYou);
        assert_eq!(Health::from_status("succeeded"), Health::Done);
        assert_eq!(Health::from_status("failed"), Health::Failed);
    }

    #[test]
    fn from_roster_builds_live_tiles_carrying_instance_ids() {
        let resp: GatewayResponse = serde_json::from_value(serde_json::json!({
            "workflow": { "id": "wf_42", "definitionId": "cognitive/flow.migrate", "state": "verifying", "version": 2 },
            "result": { "status": "failed", "reason": "guard_unmet" },
            "links": []
        }))
        .unwrap();
        let fleet = Fleet::from_roster(&[resp]);
        assert_eq!(fleet.missions.len(), 1);
        assert_eq!(fleet.missions[0].health, Health::Failed);
        assert_eq!(fleet.missions[0].workflow_id.as_deref(), Some("wf_42"));
        assert_eq!(fleet.missions[0].name, "flow.migrate"); // short title
    }
}
