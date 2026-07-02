//! Tests for deterministic chaining — audit, versioning, depth, and recovery.
//!
//! Covers chain depth limiting, partial failure + recovery links, version
//! monotonicity across chained steps, audit-event emission, explain output
//! for chained transitions, and manual recovery of deterministic transitions.

mod common;
use common::chain::*;

use std::sync::Arc;

use praxec_core::model::{Principal, StartWorkflow, SubmitTransition};
use praxec_core::ports::Executor;
use serde_json::{json, Value};

// ---- 5. Depth limit stops chain early ---------------------------------------

#[tokio::test]
async fn depth_limit_stops_chain_early() {
    let (runtime, exec, _) = build_runtime(depth_limited_config());
    let resp = runtime
        .start(StartWorkflow {
            definition_id: "pipeline".into(),
            input: json!({}),
            principal: Principal::anonymous(),
            trace_id: None,
            run_id: None,
            depth: 0,
            parent: None,
        })
        .await
        .unwrap();

    // maxChainDepth=2, so chain should stop after 2 steps (at state "c")
    assert_eq!(resp["workflow"]["state"], "c");
    let chain = resp["chain"].as_array().expect("chain array");
    assert_eq!(chain.len(), 2);
    assert_eq!(exec.count(), 2);
}

// ---- 6. Chain failure returns partial steps and recovery link ---------------

#[tokio::test]
async fn chain_failure_returns_partial_and_recovery_link() {
    let executor = Arc::new(FailAfterN::new(1)); // succeed once, fail on second
    let (runtime, audit) = build_runtime_with_executor(
        fully_deterministic_to_terminal(),
        executor as Arc<dyn Executor>,
    );

    let resp = runtime
        .start(StartWorkflow {
            definition_id: "pipeline".into(),
            input: json!({}),
            principal: Principal::anonymous(),
            trace_id: None,
            run_id: None,
            depth: 0,
            parent: None,
        })
        .await
        .unwrap();

    // First step (a→b) succeeds, second step (b→c) fails
    assert_eq!(resp["workflow"]["state"], "b");
    assert_eq!(resp["result"]["status"], "failed");
    assert_eq!(resp["error"]["code"], "CHAIN_FAILED");

    let chain = resp["chain"].as_array().expect("chain array");
    assert_eq!(chain.len(), 1, "only the successful step recorded");
    assert_eq!(chain[0]["fromState"], "a");
    assert_eq!(chain[0]["toState"], "b");

    // Recovery link should include the failed deterministic transition
    let rels: Vec<&str> = resp["links"]
        .as_array()
        .unwrap()
        .iter()
        .map(|l| l["rel"].as_str().unwrap())
        .collect();
    assert!(
        rels.contains(&"step2"),
        "failed deterministic transition should appear in links for recovery"
    );

    // Audit should include chain.failed
    let types = audit.event_types();
    assert!(types.iter().any(|t| t == "chain.failed"));
}

// ---- 7. Chain after submit auto-executes from new state ---------------------

#[tokio::test]
async fn chain_runs_after_submit() {
    // a→b is agent, b→c is deterministic, c is terminal
    let cfg = json!({
        "version": "1.0.0",
        "workflows": {
            "pipeline": {
                "initialState": "a",
                "states": {
                    "a": {
                        "transitions": {
                            "begin": {
                                "target": "b",
                                "actor": "agent",
                                "executor": { "kind": "noop" }
                            }
                        }
                    },
                    "b": {
                        "transitions": {
                            "finalize": {
                                "target": "c",
                                "actor": "deterministic",
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
    let started = runtime
        .start(StartWorkflow {
            definition_id: "pipeline".into(),
            input: json!({}),
            principal: Principal::anonymous(),
            trace_id: None,
            run_id: None,
            depth: 0,
            parent: None,
        })
        .await
        .unwrap();

    // Start should stay at "a" (agent transition, no chain)
    assert_eq!(started["workflow"]["state"], "a");

    let wf_id = started["workflow"]["id"].as_str().unwrap().to_string();
    let version = started["workflow"]["version"].as_u64().unwrap();

    let resp = runtime
        .submit(SubmitTransition {
            workflow_id: wf_id,
            expected_version: version,
            transition: "begin".into(),
            arguments: json!({}),
            principal: Principal::anonymous(),
            summary: None,
            trace_id: None,
            run_id: None,
        })
        .await
        .unwrap();

    // Submit should execute begin (a→b) then chain finalize (b→c)
    assert_eq!(resp["workflow"]["state"], "c");
    assert_eq!(resp["result"]["status"], "succeeded");

    let chain = resp["chain"].as_array().expect("chain array");
    assert_eq!(chain.len(), 1);
    assert_eq!(chain[0]["fromState"], "b");
    assert_eq!(chain[0]["transition"], "finalize");
    assert_eq!(chain[0]["toState"], "c");

    // 2 executor calls: 1 for submit's "begin" + 1 for chain's "finalize"
    assert_eq!(exec.count(), 2);
}

// ---- 10. Chain steps record correct versions --------------------------------

#[tokio::test]
async fn chain_steps_have_incrementing_versions() {
    let (runtime, _, _) = build_runtime(fully_deterministic_to_terminal());
    let resp = runtime
        .start(StartWorkflow {
            definition_id: "pipeline".into(),
            input: json!({}),
            principal: Principal::anonymous(),
            trace_id: None,
            run_id: None,
            depth: 0,
            parent: None,
        })
        .await
        .unwrap();

    let chain = resp["chain"].as_array().unwrap();
    assert_eq!(chain.len(), 3);

    // Each step's version should be strictly increasing
    let versions: Vec<u64> = chain
        .iter()
        .map(|s| s["version"].as_u64().unwrap())
        .collect();
    for i in 1..versions.len() {
        assert!(
            versions[i] > versions[i - 1],
            "versions must increase: {:?}",
            versions
        );
    }
}

// ---- 11. Audit trail for deterministic chain --------------------------------

#[tokio::test]
async fn chain_emits_audit_events() {
    let (runtime, _, audit) = build_runtime(fully_deterministic_to_terminal());
    runtime
        .start(StartWorkflow {
            definition_id: "pipeline".into(),
            input: json!({}),
            principal: Principal::anonymous(),
            trace_id: None,
            run_id: None,
            depth: 0,
            parent: None,
        })
        .await
        .unwrap();

    let types = audit.event_types();

    // chain.step for each deterministic step
    let chain_steps: Vec<_> = audit
        .snapshot()
        .into_iter()
        .filter(|e| e.event_type == "chain.step")
        .collect();
    assert_eq!(chain_steps.len(), 3);

    // chain.completed at the end
    assert!(types.iter().any(|t| t == "chain.completed"));

    // workflow.transitioned for each step (with deterministic: true)
    let transitions: Vec<_> = audit
        .snapshot()
        .into_iter()
        .filter(|e| {
            e.event_type == "workflow.transitioned"
                && e.payload.get("deterministic").and_then(Value::as_bool) == Some(true)
        })
        .collect();
    assert_eq!(transitions.len(), 3);
}

// ---- 14. Explain includes actor and deterministic flag ----------------------

#[tokio::test]
async fn explain_shows_actor_and_deterministic_flag() {
    let (runtime, _, _) = build_runtime(mixed_state_config());
    let started = runtime
        .start(StartWorkflow {
            definition_id: "pipeline".into(),
            input: json!({}),
            principal: Principal::anonymous(),
            trace_id: None,
            run_id: None,
            depth: 0,
            parent: None,
        })
        .await
        .unwrap();
    let wf_id = started["workflow"]["id"].as_str().unwrap();

    let explain_det = runtime.explain(wf_id, "auto_check").await.unwrap();
    assert_eq!(explain_det["actor"], "deterministic");
    assert_eq!(explain_det["deterministic"], true);

    let explain_agent = runtime.explain(wf_id, "manual_override").await.unwrap();
    assert_eq!(explain_agent["actor"], "agent");
    assert_eq!(explain_agent["deterministic"], false);
}

// ---- 15. Deterministic transition can still be submitted manually -----------
// (No actor gate — FMECA finding: gate creates stuck workflows)

#[tokio::test]
async fn deterministic_transition_submittable_for_recovery() {
    let cfg = json!({
        "version": "1.0.0",
        "workflows": {
            "pipeline": {
                "initialState": "a",
                "states": {
                    "a": {
                        "transitions": {
                            "auto_step": {
                                "target": "b",
                                "actor": "deterministic",
                                "executor": { "kind": "noop" }
                            },
                            "manual_alt": {
                                "target": "b",
                                "actor": "agent",
                                "executor": { "kind": "noop" }
                            }
                        }
                    },
                    "b": { "terminal": true }
                }
            }
        }
    });

    let (runtime, _, _) = build_runtime(cfg);
    let started = runtime
        .start(StartWorkflow {
            definition_id: "pipeline".into(),
            input: json!({}),
            principal: Principal::anonymous(),
            trace_id: None,
            run_id: None,
            depth: 0,
            parent: None,
        })
        .await
        .unwrap();
    let wf_id = started["workflow"]["id"].as_str().unwrap().to_string();
    let version = started["workflow"]["version"].as_u64().unwrap();

    // Manually submitting a deterministic transition should work
    let resp = runtime
        .submit(SubmitTransition {
            workflow_id: wf_id,
            expected_version: version,
            transition: "auto_step".into(),
            arguments: json!({}),
            principal: Principal::anonymous(),
            summary: None,
            trace_id: None,
            run_id: None,
        })
        .await
        .unwrap();

    assert_eq!(resp["workflow"]["state"], "b");
    assert_eq!(resp["result"]["status"], "succeeded");
}

// ---- 16. Unguarded deterministic transition is a lowest-precedence default ---
// A switch state whose guarded arms all fail (e.g. an out-of-domain discriminant)
// falls through to the single unguarded default instead of dead-stalling.

#[tokio::test]
async fn switch_falls_through_to_the_unguarded_default() {
    let cfg = json!({
        "version": "1.0.0",
        "workflows": {
            "pipeline": {
                "initialState": "gate",
                // verdict is present but out-of-domain (the real failure mode: a
                // producer emitted a value outside the enumerated arms, e.g. "").
                // An *absent* slot would fail-loud as GUARD_UNSET_SLOT instead.
                "initialContext": { "verdict": "unexpected" },
                "states": {
                    "gate": {
                        "transitions": {
                            "to_a": {
                                "target": "a", "actor": "deterministic",
                                "executor": { "kind": "noop" },
                                "guards": [ { "kind": "expr", "expr": "$.context.verdict == 'x'" } ]
                            },
                            "to_b": {
                                "target": "b", "actor": "deterministic",
                                "executor": { "kind": "noop" },
                                "guards": [ { "kind": "expr", "expr": "$.context.verdict == 'y'" } ]
                            },
                            "fallthrough": {
                                "target": "defaulted", "actor": "deterministic",
                                "executor": { "kind": "noop" }
                            }
                        }
                    },
                    "a": { "terminal": true },
                    "b": { "terminal": true },
                    "defaulted": { "terminal": true }
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
            trace_id: None,
            run_id: None,
            depth: 0,
            parent: None,
        })
        .await
        .unwrap();

    // verdict is out-of-domain → neither guarded arm matches → the unguarded
    // default is taken (no selection_error dead-stall).
    assert_eq!(resp["workflow"]["state"], "defaulted");
}
