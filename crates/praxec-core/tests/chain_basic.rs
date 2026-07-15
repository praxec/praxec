//! Tests for deterministic chaining — linear + fully deterministic flows.
//!
//! These cover the happy path: chain auto-execution stops at agent decision
//! points, traverses to terminals when fully deterministic, hides
//! deterministic transitions from links, and short-circuits cleanly when
//! there's nothing to chain.

mod common;
use common::chain::*;

use praxec_core::model::{Principal, StartWorkflow};
use serde_json::json;

// ---- 1. Linear chain stops at agent decision point --------------------------

#[tokio::test]
async fn chain_auto_executes_deterministic_and_stops_at_agent() {
    let (runtime, exec, _) = build_runtime(linear_chain_stops_at_agent());
    let resp = runtime
        .start(StartWorkflow {
            definition_id: "pipeline".into(),
            input: json!({}),
            principal: Principal::anonymous(),
            run_env: praxec_core::RunEnv::for_test(),
            depth: 0,
            parent: None,
        })
        .await
        .unwrap();

    // Should have chained from a→b, stopping at b (agent transition)
    assert_eq!(resp["workflow"]["state"], "b");
    assert_eq!(resp["result"]["status"], "running");

    let chain = resp["chain"].as_array().expect("chain array");
    assert_eq!(chain.len(), 1);
    assert_eq!(chain[0]["fromState"], "a");
    assert_eq!(chain[0]["transition"], "validate");
    assert_eq!(chain[0]["toState"], "b");

    // Executor ran once for the deterministic step
    assert_eq!(exec.count(), 1);
}

// ---- 2. Fully deterministic chain reaches terminal --------------------------

#[tokio::test]
async fn fully_deterministic_chain_reaches_terminal() {
    let (runtime, exec, _) = build_runtime(fully_deterministic_to_terminal());
    let resp = runtime
        .start(StartWorkflow {
            definition_id: "pipeline".into(),
            input: json!({}),
            principal: Principal::anonymous(),
            run_env: praxec_core::RunEnv::for_test(),
            depth: 0,
            parent: None,
        })
        .await
        .unwrap();

    assert_eq!(resp["workflow"]["state"], "d");
    assert_eq!(resp["result"]["status"], "succeeded");

    let chain = resp["chain"].as_array().expect("chain array");
    assert_eq!(chain.len(), 3);
    assert_eq!(chain[0]["fromState"], "a");
    assert_eq!(chain[0]["toState"], "b");
    assert_eq!(chain[1]["fromState"], "b");
    assert_eq!(chain[1]["toState"], "c");
    assert_eq!(chain[2]["fromState"], "c");
    assert_eq!(chain[2]["toState"], "d");

    assert_eq!(exec.count(), 3);
    assert!(resp["links"].as_array().unwrap().is_empty());
}

// ---- 3. Mixed state stops the chain (no auto-execute) -----------------------

#[tokio::test]
async fn mixed_state_stops_chain() {
    let (runtime, exec, _) = build_runtime(mixed_state_config());
    let resp = runtime
        .start(StartWorkflow {
            definition_id: "pipeline".into(),
            input: json!({}),
            principal: Principal::anonymous(),
            run_env: praxec_core::RunEnv::for_test(),
            depth: 0,
            parent: None,
        })
        .await
        .unwrap();

    // Chain should NOT execute; stays at initial state "a"
    assert_eq!(resp["workflow"]["state"], "a");
    assert!(resp.get("chain").is_none() || resp["chain"].as_array().unwrap().is_empty());
    assert_eq!(exec.count(), 0);

    // Only the agent transition should appear in links (deterministic hidden)
    let rels: Vec<&str> = resp["links"]
        .as_array()
        .unwrap()
        .iter()
        .map(|l| l["rel"].as_str().unwrap())
        .collect();
    assert!(rels.contains(&"manual_override"));
    assert!(
        !rels.contains(&"auto_check"),
        "deterministic transitions must be hidden from links"
    );
}

// ---- 4. Deterministic transitions are hidden from links ---------------------

#[tokio::test]
async fn deterministic_transitions_hidden_from_links() {
    let cfg = json!({
        "version": "1.0.0",
        "workflows": {
            "pipeline": {
                "initialState": "a",
                "states": {
                    "a": {
                        "transitions": {
                            "auto_lint": {
                                "target": "b",
                                "actor": "deterministic",
                                "executor": { "kind": "noop" }
                            },
                            "manual_review": {
                                "target": "b",
                                "actor": "agent",
                                "executor": { "kind": "noop" }
                            },
                            "human_approve": {
                                "target": "c",
                                "actor": "human",
                                "executor": { "kind": "noop" }
                            }
                        }
                    },
                    "b": { "terminal": true },
                    "c": { "terminal": true }
                }
            }
        }
    });

    let (runtime, _, _) = build_runtime(cfg);
    let resp = runtime
        .start(StartWorkflow {
            definition_id: "pipeline".into(),
            input: json!({}),
            principal: Principal::anonymous(),
            run_env: praxec_core::RunEnv::for_test(),
            depth: 0,
            parent: None,
        })
        .await
        .unwrap();

    let rels: Vec<&str> = resp["links"]
        .as_array()
        .unwrap()
        .iter()
        .map(|l| l["rel"].as_str().unwrap())
        .collect();
    assert!(rels.contains(&"manual_review"));
    assert!(rels.contains(&"human_approve"));
    assert!(!rels.contains(&"auto_lint"));
}

// ---- 12. No chain when initial state is terminal ----------------------------

#[tokio::test]
async fn no_chain_when_already_terminal() {
    let cfg = json!({
        "version": "1.0.0",
        "workflows": {
            "instant": {
                "initialState": "done",
                "states": {
                    "done": { "terminal": true }
                }
            }
        }
    });

    let (runtime, exec, _) = build_runtime(cfg);
    let resp = runtime
        .start(StartWorkflow {
            definition_id: "instant".into(),
            input: json!({}),
            principal: Principal::anonymous(),
            run_env: praxec_core::RunEnv::for_test(),
            depth: 0,
            parent: None,
        })
        .await
        .unwrap();

    assert_eq!(resp["workflow"]["state"], "done");
    assert_eq!(resp["result"]["status"], "succeeded");
    assert!(resp.get("chain").is_none() || resp["chain"].as_array().unwrap().is_empty());
    assert_eq!(exec.count(), 0);
}

// ---- 13. No chain when no transitions exist ---------------------------------

#[tokio::test]
async fn no_chain_when_state_has_no_transitions() {
    let cfg = json!({
        "version": "1.0.0",
        "workflows": {
            "stuck": {
                "initialState": "a",
                "states": {
                    "a": {}
                }
            }
        }
    });

    let (runtime, exec, _) = build_runtime(cfg);
    let resp = runtime
        .start(StartWorkflow {
            definition_id: "stuck".into(),
            input: json!({}),
            principal: Principal::anonymous(),
            run_env: praxec_core::RunEnv::for_test(),
            depth: 0,
            parent: None,
        })
        .await
        .unwrap();

    assert_eq!(resp["workflow"]["state"], "a");
    assert!(resp.get("chain").is_none() || resp["chain"].as_array().unwrap().is_empty());
    assert_eq!(exec.count(), 0);
}

// ---- 16. Chain without executor (pure routing) ------------------------------

#[tokio::test]
async fn chain_works_without_executor() {
    let cfg = json!({
        "version": "1.0.0",
        "workflows": {
            "pipeline": {
                "initialState": "a",
                "states": {
                    "a": {
                        "transitions": {
                            "route": {
                                "target": "b",
                                "actor": "deterministic"
                            }
                        }
                    },
                    "b": {
                        "transitions": {
                            "next": {
                                "target": "c",
                                "actor": "agent",
                                "executor": { "kind": "noop" }
                            }
                        }
                    },
                    "c": { "terminal": true }
                }
            }
        }
    });

    let (runtime, exec, _) = build_runtime(cfg);
    let resp = runtime
        .start(StartWorkflow {
            definition_id: "pipeline".into(),
            input: json!({}),
            principal: Principal::anonymous(),
            run_env: praxec_core::RunEnv::for_test(),
            depth: 0,
            parent: None,
        })
        .await
        .unwrap();

    assert_eq!(resp["workflow"]["state"], "b");
    let chain = resp["chain"].as_array().unwrap();
    assert_eq!(chain.len(), 1);
    assert_eq!(exec.count(), 0, "no executor should run for pure routing");
}
