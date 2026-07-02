//! A TransitionChooser that picks a random legal *agent* action under a
//! deterministic seed, recording which transitions it has exercised.

use std::collections::HashSet;
use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use praxec_agents::orchestrator::{MissionState, TransitionChooser};

use crate::prng::Prng;

const MAX_STALL: u32 = 6;

pub struct FuzzChooser {
    rng: Mutex<Prng>,
    coverage: Arc<Mutex<HashSet<String>>>,
    progress: Mutex<(u64, u32)>,
}

impl FuzzChooser {
    pub fn new(seed: u64, coverage: Arc<Mutex<HashSet<String>>>) -> Self {
        Self {
            rng: Mutex::new(Prng::new(seed)),
            coverage,
            progress: Mutex::new((u64::MAX, 0)),
        }
    }
}

#[async_trait]
impl TransitionChooser for FuzzChooser {
    async fn choose(&self, state: &MissionState) -> Option<String> {
        // Detect stalls: if version hasn't changed across MAX_STALL calls, give up.
        {
            let mut p = self.progress.lock().expect("progress lock");
            if state.version == p.0 {
                p.1 += 1;
            } else {
                p.0 = state.version;
                p.1 = 0;
            }
            if p.1 >= MAX_STALL {
                return None; // no progress for MAX_STALL choices → give up (livelock)
            }
        }

        let actions = state.agent_actions();
        if actions.is_empty() {
            return None;
        }
        let idx = self.rng.lock().expect("rng lock").below(actions.len());
        let chosen = actions[idx].transition.clone();
        self.coverage
            .lock()
            .expect("coverage lock")
            .insert(chosen.clone());
        Some(chosen)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use praxec_agents::orchestrator::LegalAction;

    fn state_with(actions: Vec<(&str, &str)>) -> MissionState {
        MissionState {
            mission_id: "wf_test".to_string(),
            status: "running".to_string(),
            reason: None,
            version: 1,
            goal: None,
            outcomes: vec![],
            legal_actions: actions
                .into_iter()
                .map(|(t, a)| LegalAction {
                    transition: t.to_string(),
                    actor: a.to_string(),
                })
                .collect(),
        }
    }

    #[tokio::test]
    async fn never_chooses_human_actions() {
        let cov = Arc::new(Mutex::new(HashSet::new()));
        let chooser = FuzzChooser::new(1, cov);
        let st = state_with(vec![("approve", "human"), ("request_changes", "human")]);
        assert_eq!(chooser.choose(&st).await, None);
    }

    #[tokio::test]
    async fn picks_an_agent_action_and_records_coverage() {
        let cov = Arc::new(Mutex::new(HashSet::new()));
        let chooser = FuzzChooser::new(1, cov.clone());
        let st = state_with(vec![("a", "agent"), ("b", "agent")]);
        let chosen = chooser.choose(&st).await.expect("an agent action");
        assert!(chosen == "a" || chosen == "b");
        assert!(cov.lock().unwrap().contains(&chosen));
    }

    #[tokio::test]
    async fn deterministic_for_a_seed() {
        let st = state_with(vec![("a", "agent"), ("b", "agent"), ("c", "agent")]);
        let c1 = FuzzChooser::new(99, Arc::new(Mutex::new(HashSet::new())));
        let c2 = FuzzChooser::new(99, Arc::new(Mutex::new(HashSet::new())));
        assert_eq!(c1.choose(&st).await, c2.choose(&st).await);
    }

    #[tokio::test]
    async fn gives_up_after_stall() {
        let cov = Arc::new(Mutex::new(HashSet::new()));
        let chooser = FuzzChooser::new(1, cov);
        // Same version every call (no progress). Build a state with agent actions.
        let st = state_with(vec![("a", "agent"), ("b", "agent")]);
        // First MAX_STALL calls return Some; then it gives up with None.
        let mut results = Vec::new();
        for _ in 0..8 {
            results.push(chooser.choose(&st).await);
        }
        assert!(
            results.iter().take(6).all(|r| r.is_some()),
            "first 6 should pick: {results:?}"
        );
        assert!(
            results[6].is_none() || results[7].is_none(),
            "should give up by stall: {results:?}"
        );
    }
}
