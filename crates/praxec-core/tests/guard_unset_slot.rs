//! Guarantee tests for SPEC §9 — the runtime fails fast on a guard
//! evaluating against an unset blackboard slot.
//!
//! "The runtime remains the backstop — a guard hitting an unset slot fails
//! fast with rich context, never a silent `false`."
//!
//! A slot explicitly set to JSON `null` is *not* "unset"; that's a
//! deliberate write of the null value and must evaluate normally.

use std::sync::Arc;

use praxec_core::WorkflowRuntime;
use praxec_core::audit::{AuditSink, MemoryAuditSink};
use praxec_core::guards::DefaultGuardEvaluator;
use praxec_core::model::{Principal, StartWorkflow, SubmitTransition};
use praxec_core::store::{ConfigDefinitionStore, InMemoryWorkflowStore};
use serde_json::{Value, json};

struct NoopRegistry;
impl praxec_core::ExecutorRegistry for NoopRegistry {
    fn get(&self, _kind: &str) -> Option<Arc<dyn praxec_core::Executor>> {
        None
    }
}

fn build_runtime(config: Value) -> WorkflowRuntime {
    let definitions = Arc::new(ConfigDefinitionStore::from_config(&config));
    let store = Arc::new(InMemoryWorkflowStore::new());
    let executors = Arc::new(NoopRegistry);
    let guards = Arc::new(DefaultGuardEvaluator::new());
    let audit: Arc<dyn AuditSink> = Arc::new(MemoryAuditSink::new());
    WorkflowRuntime::new(definitions, store, executors, guards, audit)
        .with_writable_repo_roots(vec![praxec_core::RepoRoot::for_test()])
}

#[tokio::test]
async fn guard_reading_unset_slot_returns_guard_unset_slot_rejection() {
    // No initialContext; the guard reads $.context.flag which is never set.
    // SPEC §9: must fail fast, not silently coalesce to null/false.
    let cfg = json!({
        "version": "1.0.0",
        "workflows": {
            "wf": {
                "initialState": "draft",
                "states": {
                    "draft": {
                        "transitions": {
                            "submit": {
                                "target": "done",
                                "actor": "agent",
                                "guards": [
                                    { "kind": "expr", "expr": "$.context.flag == true" }
                                ]
                            }
                        }
                    },
                    "done": { "terminal": true }
                }
            }
        }
    });
    let runtime = build_runtime(cfg);
    let start = runtime
        .start(StartWorkflow {
            definition_id: "wf".into(),
            input: json!({}),
            principal: Principal::anonymous(),
            run_env: praxec_core::RunEnv::for_test(),
            depth: 0,
            parent: None,
        })
        .await
        .unwrap();
    let workflow_id = start["workflow"]["id"].as_str().unwrap().to_string();
    let version = start["workflow"]["version"].as_u64().unwrap();

    let resp = runtime
        .submit(SubmitTransition {
            workflow_id: workflow_id.clone(),
            expected_version: version,
            transition: "submit".into(),
            arguments: json!({}),
            principal: Principal::anonymous(),
            summary: None,
            trace_id: None,
            run_id: None,
        })
        .await
        .unwrap();

    assert_eq!(
        resp["result"]["status"].as_str(),
        Some("running"),
        "a rejected move leaves the mission in process; got: {resp}"
    );
    assert_eq!(
        resp["error"]["code"].as_str(),
        Some("GUARD_UNSET_SLOT"),
        "error code must be GUARD_UNSET_SLOT; got: {}",
        resp["error"]
    );
    let message = resp["error"]["message"].as_str().unwrap_or_default();
    assert!(
        message.contains("$.context.flag"),
        "error message must name the unset slot path; got: {message}"
    );

    // Snapshot version unchanged — the transition was aborted at the guard.
    assert_eq!(
        resp["workflow"]["version"].as_u64(),
        Some(version),
        "version must not advance when guard fails on unset slot"
    );
}

#[tokio::test]
async fn explicitly_null_slot_is_not_unset() {
    // `output: { x: null }` writes the slot to null. A subsequent guard
    // `$.context.x == null` must evaluate normally (true) — this is NOT
    // an unset-slot scenario.
    let cfg = json!({
        "version": "1.0.0",
        "workflows": {
            "wf": {
                "initialState": "draft",
                "initialContext": { "x": null },
                "states": {
                    "draft": {
                        "transitions": {
                            "submit": {
                                "target": "done",
                                "actor": "agent",
                                "guards": [
                                    { "kind": "expr", "expr": "$.context.x == null" }
                                ]
                            }
                        }
                    },
                    "done": { "terminal": true }
                }
            }
        }
    });
    let runtime = build_runtime(cfg);
    let start = runtime
        .start(StartWorkflow {
            definition_id: "wf".into(),
            input: json!({}),
            principal: Principal::anonymous(),
            run_env: praxec_core::RunEnv::for_test(),
            depth: 0,
            parent: None,
        })
        .await
        .unwrap();
    let workflow_id = start["workflow"]["id"].as_str().unwrap().to_string();
    let version = start["workflow"]["version"].as_u64().unwrap();

    let resp = runtime
        .submit(SubmitTransition {
            workflow_id,
            expected_version: version,
            transition: "submit".into(),
            arguments: json!({}),
            principal: Principal::anonymous(),
            summary: None,
            trace_id: None,
            run_id: None,
        })
        .await
        .unwrap();

    assert_eq!(
        resp["result"]["status"].as_str(),
        Some("succeeded"),
        "explicit null is a write, not an unset slot; the guard evaluates normally \
         and the mission advances to its terminal. got: {resp}"
    );
    assert_eq!(resp["workflow"]["state"], "done");
}

#[tokio::test]
async fn any_of_with_unset_sibling_passes_if_another_clause_satisfies() {
    // `any_of` over an unset slot AND a passing clause must succeed via
    // the passing clause — the author opted into "any of these works".
    let cfg = json!({
        "version": "1.0.0",
        "workflows": {
            "wf": {
                "initialState": "draft",
                "initialContext": { "ok": true },
                "states": {
                    "draft": {
                        "transitions": {
                            "submit": {
                                "target": "done",
                                "actor": "agent",
                                "guards": [
                                    {
                                        "kind": "any_of",
                                        "guards": [
                                            { "kind": "expr", "expr": "$.context.missing == true" },
                                            { "kind": "expr", "expr": "$.context.ok == true" }
                                        ]
                                    }
                                ]
                            }
                        }
                    },
                    "done": { "terminal": true }
                }
            }
        }
    });
    let runtime = build_runtime(cfg);
    let start = runtime
        .start(StartWorkflow {
            definition_id: "wf".into(),
            input: json!({}),
            principal: Principal::anonymous(),
            run_env: praxec_core::RunEnv::for_test(),
            depth: 0,
            parent: None,
        })
        .await
        .unwrap();
    let workflow_id = start["workflow"]["id"].as_str().unwrap().to_string();
    let version = start["workflow"]["version"].as_u64().unwrap();

    let resp = runtime
        .submit(SubmitTransition {
            workflow_id,
            expected_version: version,
            transition: "submit".into(),
            arguments: json!({}),
            principal: Principal::anonymous(),
            summary: None,
            trace_id: None,
            run_id: None,
        })
        .await
        .unwrap();

    assert!(
        resp["error"].is_null(),
        "any_of must accept a passing sibling even when another clause hits an unset slot; got: {resp}"
    );
}

#[tokio::test]
async fn guard_can_read_workflow_state_id_and_version() {
    // SPEC §5.2: templates and guards share the same `$.`-rooted paths,
    // including `$.workflow.*`. A guard comparing the instance's state must
    // resolve to the live state name and evaluate without an unset-slot
    // error.
    let cfg = json!({
        "version": "1.0.0",
        "workflows": {
            "wf": {
                "version": "2026-05-22",
                "initialState": "draft",
                "states": {
                    "draft": {
                        "transitions": {
                            "submit": {
                                "target": "done",
                                "actor": "agent",
                                "guards": [
                                    { "kind": "expr", "expr": "$.workflow.state == 'draft'" }
                                ]
                            }
                        }
                    },
                    "done": { "terminal": true }
                }
            }
        }
    });
    let runtime = build_runtime(cfg);
    let start = runtime
        .start(StartWorkflow {
            definition_id: "wf".into(),
            input: json!({}),
            principal: Principal::anonymous(),
            run_env: praxec_core::RunEnv::for_test(),
            depth: 0,
            parent: None,
        })
        .await
        .unwrap();
    let workflow_id = start["workflow"]["id"].as_str().unwrap().to_string();
    let version = start["workflow"]["version"].as_u64().unwrap();

    let resp = runtime
        .submit(SubmitTransition {
            workflow_id,
            expected_version: version,
            transition: "submit".into(),
            arguments: json!({}),
            principal: Principal::anonymous(),
            summary: None,
            trace_id: None,
            run_id: None,
        })
        .await
        .unwrap();

    assert!(
        resp["error"].is_null(),
        "guard reading $.workflow.state must resolve without rejection; got: {resp}"
    );
    assert_eq!(resp["workflow"]["state"], "done");
}

#[tokio::test]
async fn any_of_with_only_unset_clauses_surfaces_unset_error() {
    // If `any_of` has no passing clause AND at least one errored on an
    // unset slot, the runtime must surface GUARD_UNSET_SLOT — silent
    // false would hide a real authoring bug.
    let cfg = json!({
        "version": "1.0.0",
        "workflows": {
            "wf": {
                "initialState": "draft",
                "states": {
                    "draft": {
                        "transitions": {
                            "submit": {
                                "target": "done",
                                "actor": "agent",
                                "guards": [
                                    {
                                        "kind": "any_of",
                                        "guards": [
                                            { "kind": "expr", "expr": "$.context.missing_a == true" },
                                            { "kind": "expr", "expr": "$.context.missing_b == true" }
                                        ]
                                    }
                                ]
                            }
                        }
                    },
                    "done": { "terminal": true }
                }
            }
        }
    });
    let runtime = build_runtime(cfg);
    let start = runtime
        .start(StartWorkflow {
            definition_id: "wf".into(),
            input: json!({}),
            principal: Principal::anonymous(),
            run_env: praxec_core::RunEnv::for_test(),
            depth: 0,
            parent: None,
        })
        .await
        .unwrap();
    let workflow_id = start["workflow"]["id"].as_str().unwrap().to_string();
    let version = start["workflow"]["version"].as_u64().unwrap();

    let resp = runtime
        .submit(SubmitTransition {
            workflow_id,
            expected_version: version,
            transition: "submit".into(),
            arguments: json!({}),
            principal: Principal::anonymous(),
            summary: None,
            trace_id: None,
            run_id: None,
        })
        .await
        .unwrap();

    assert_eq!(resp["result"]["status"].as_str(), Some("running"));
    assert_eq!(resp["error"]["code"].as_str(), Some("GUARD_UNSET_SLOT"));
}
