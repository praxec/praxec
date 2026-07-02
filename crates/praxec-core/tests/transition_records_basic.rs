//! Guarantee tests for transition record emission.
//!
//! Every applied workflow transition must emit exactly one `workflow.transition`
//! audit event (a "transition record"), and it must be emitted *record-first*:
//! the record is written before the authoritative state snapshot is committed.
//! If the record write fails, the transition fails fast and the snapshot is NOT
//! committed.
//!
//! This file holds the record-emission + blackboard-delta tests. The executor
//! descriptor tests live in `transition_records_executor.rs`.

use std::sync::Arc;

use praxec_core::audit::{AuditEvent, AuditSink, MemoryAuditSink};
use praxec_core::model::{GetWorkflow, Principal, StartWorkflow, SubmitTransition};
use praxec_core::ports::WorkflowStore;
use praxec_core::store::{ConfigDefinitionStore, InMemoryWorkflowStore};
use praxec_core::WorkflowRuntime;
use serde_json::{json, Value};

mod common;
use common::transition_records::*;

// =============================================================================
// Tests
// =============================================================================

/// A workflow that applies N transitions (here N=3, via a deterministic chain
/// out of one `start`) must emit exactly N `workflow.transition` records, whose
/// `seq` values are 1..=N.
#[tokio::test]
async fn record_emitted_per_applied_transition() {
    let audit = Arc::new(MemoryAuditSink::new());
    let (runtime, _store) = build_runtime(three_step_chain(), audit.clone());

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
        .expect("start should succeed");

    let records: Vec<AuditEvent> = audit
        .snapshot()
        .into_iter()
        .filter(|e| e.event_type == "workflow.transition")
        .collect();

    assert_eq!(
        records.len(),
        3,
        "exactly one workflow.transition record per applied transition"
    );

    let seqs: Vec<u64> = records
        .iter()
        .map(|e| {
            e.payload
                .get("seq")
                .and_then(Value::as_u64)
                .expect("record must carry a numeric seq")
        })
        .collect();
    assert_eq!(seqs, vec![1, 2, 3], "seq must run 1..=N");
}

/// If the transition record write fails, the `submit` must fail with an error
/// identifiable as `RECORD_WRITE_FAILED`.
#[tokio::test]
async fn record_write_failure_aborts_transition() {
    let audit = Arc::new(FailingAuditSink::fail_all_transition_records());
    let (runtime, _store) = build_runtime(single_agent_transition(), audit.clone());

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
        .expect("start should succeed (no transition applied at start)");
    let wf_id = started["workflow"]["id"].as_str().unwrap().to_string();
    let version = started["workflow"]["version"].as_u64().unwrap();

    let result = runtime
        .submit(SubmitTransition {
            workflow_id: wf_id.clone(),
            expected_version: version,
            transition: "go".into(),
            arguments: json!({}),
            principal: Principal::anonymous(),
            summary: None,
            trace_id: None,
            run_id: None,
        })
        .await;

    let err = result.expect_err("submit must fail when the transition record write fails");
    let msg = format!("{err:#}");
    assert!(
        msg.contains("RECORD_WRITE_FAILED"),
        "error must be identifiable as RECORD_WRITE_FAILED, got: {msg}"
    );
}

/// After a record-write failure aborts a `submit`, the persisted workflow
/// version must be unchanged — proof the snapshot did not commit.
#[tokio::test]
async fn version_unchanged_when_record_write_fails() {
    let audit = Arc::new(FailingAuditSink::fail_all_transition_records());
    let (runtime, store) = build_runtime(single_agent_transition(), audit.clone());

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
        .expect("start should succeed");
    let wf_id = started["workflow"]["id"].as_str().unwrap().to_string();
    let version_before = started["workflow"]["version"].as_u64().unwrap();

    let result = runtime
        .submit(SubmitTransition {
            workflow_id: wf_id.clone(),
            expected_version: version_before,
            transition: "go".into(),
            arguments: json!({}),
            principal: Principal::anonymous(),
            summary: None,
            trace_id: None,
            run_id: None,
        })
        .await;
    assert!(result.is_err(), "submit must fail");

    let loaded = store.load(&wf_id).await.expect("workflow must still load");
    assert_eq!(
        loaded.version, version_before,
        "version must be unchanged: the snapshot must not have committed"
    );
    assert_eq!(
        loaded.state, "a",
        "state must be unchanged: the snapshot must not have committed"
    );
}

/// A workflow that fires a lazy timeout (via `get`) must emit a
/// `workflow.transition` record with `actor` = `"system"` and
/// `transition` = `"onTimeout"`. The record must appear before the
/// `workflow.timed_out` event (record-first ordering).
#[tokio::test]
async fn timeout_emits_workflow_transition_record_with_system_actor() {
    let config = json!({
        "version": "1.0.0",
        "workflows": {
            "short_lived": {
                "initialState": "open",
                "timeoutMs": 1,
                "onTimeout": { "target": "timed_out_state" },
                "states": {
                    "open": {
                        "transitions": {
                            "approve": { "target": "done", "executor": { "kind": "noop" } }
                        }
                    },
                    "timed_out_state": { "terminal": true },
                    "done": { "terminal": true }
                }
            }
        }
    });
    let audit = Arc::new(MemoryAuditSink::new());
    let (runtime, _store) = build_runtime(config, audit.clone());

    let started = runtime
        .start(StartWorkflow {
            definition_id: "short_lived".into(),
            input: json!({}),
            principal: Principal::anonymous(),
            trace_id: None,
            run_id: None,
            depth: 0,
            parent: None,
        })
        .await
        .expect("start should succeed");
    let wf_id = started["workflow"]["id"].as_str().unwrap().to_string();

    // Sleep past the 1ms timeout deadline.
    tokio::time::sleep(std::time::Duration::from_millis(20)).await;

    // Trigger the lazy timeout via a `get`.
    runtime
        .get(GetWorkflow {
            workflow_id: wf_id.clone(),
            principal: Principal::anonymous(),
            trace_id: None,
            run_id: None,
        })
        .await
        .expect("get should succeed");

    let snapshot = audit.snapshot();
    let event_types: Vec<&str> = snapshot.iter().map(|e| e.event_type.as_str()).collect();

    // A `workflow.transition` record must be present.
    let transition_records: Vec<&AuditEvent> = snapshot
        .iter()
        .filter(|e| e.event_type == "workflow.transition")
        .collect();
    assert!(
        !transition_records.is_empty(),
        "timeout must emit a workflow.transition record; got event types: {event_types:?}"
    );

    // The transition record must name `actor` = `"system"` in its payload.
    let record = transition_records[0];
    assert_eq!(
        record.payload.get("actor").and_then(Value::as_str),
        Some("system"),
        "timeout transition record must carry actor = \"system\""
    );

    // The transition name must be `"onTimeout"`.
    assert_eq!(
        record.payload.get("transition").and_then(Value::as_str),
        Some("onTimeout"),
        "timeout transition record must carry transition = \"onTimeout\""
    );

    // Record-first: the `workflow.transition` record must appear before the
    // `workflow.timed_out` event in the audit stream.
    let tr_pos = snapshot
        .iter()
        .position(|e| e.event_type == "workflow.transition")
        .unwrap();
    let timed_out_pos = snapshot
        .iter()
        .position(|e| e.event_type == "workflow.timed_out")
        .unwrap();
    assert!(
        tr_pos < timed_out_pos,
        "workflow.transition record (pos {tr_pos}) must precede workflow.timed_out (pos {timed_out_pos})"
    );
}

// ─────────────────────────────────────────────────────────────────────────────
// SPEC §7.2 / §7.5 — blackboardDelta carries the per-transition diff so a
// cumulative replay reconstructs the blackboard at any past `seq`.
// ─────────────────────────────────────────────────────────────────────────────

#[tokio::test]
async fn blackboard_delta_populated_with_output_writes() {
    let cfg = json!({
        "version": "1.0.0",
        "workflows": {
            "wf": {
                "initialState": "draft",
                "blackboard": ["lintPassed", "testCount"],
                "states": {
                    "draft": {
                        "transitions": {
                            "ship": {
                                "target": "done",
                                "actor": "agent",
                                "executor": { "kind": "noop" },
                                "output": {
                                    "lintPassed": "$.output.lint",
                                    "testCount": "$.output.tests"
                                }
                            }
                        }
                    },
                    "done": { "terminal": true }
                }
            }
        }
    });

    let audit = Arc::new(MemoryAuditSink::new()) as Arc<dyn AuditSink>;
    let definitions = Arc::new(ConfigDefinitionStore::from_config(&cfg));
    let store = Arc::new(InMemoryWorkflowStore::new());
    let executors = Arc::new(SingleExecRegistry {
        inner: Arc::new(FixedOutputExecutor {
            output: json!({ "lint": true, "tests": 12 }),
        }),
    });
    let guards = Arc::new(praxec_core::guards::DefaultGuardEvaluator::new());
    let runtime =
        WorkflowRuntime::new(definitions, store.clone(), executors, guards, audit.clone());

    let start = runtime
        .start(StartWorkflow {
            definition_id: "wf".into(),
            input: json!({}),
            principal: Principal::anonymous(),
            trace_id: None,
            run_id: None,
            depth: 0,
            parent: None,
        })
        .await
        .unwrap();
    let workflow_id = start["workflow"]["id"].as_str().unwrap().to_string();
    let version = start["workflow"]["version"].as_u64().unwrap();

    runtime
        .submit(SubmitTransition {
            workflow_id: workflow_id.clone(),
            expected_version: version,
            transition: "ship".into(),
            arguments: json!({}),
            principal: Principal::anonymous(),
            summary: None,
            trace_id: None,
            run_id: None,
        })
        .await
        .unwrap();

    let events = audit.list_events().await.expect("memory sink lists");
    let record = events
        .iter()
        .find(|e| e.event_type == "workflow.transition")
        .expect("a transition record must be emitted");
    let delta = record
        .payload
        .get("blackboardDelta")
        .expect("blackboardDelta must be present");
    assert_eq!(delta["lintPassed"], json!(true));
    assert_eq!(delta["testCount"], json!(12));
}

#[tokio::test]
async fn blackboard_delta_empty_when_no_context_change() {
    let cfg = json!({
        "version": "1.0.0",
        "workflows": {
            "wf": {
                "initialState": "draft",
                "states": {
                    "draft": {
                        "transitions": {
                            "submit": { "target": "done", "actor": "agent" }
                        }
                    },
                    "done": { "terminal": true }
                }
            }
        }
    });

    let audit = Arc::new(MemoryAuditSink::new()) as Arc<dyn AuditSink>;
    let (runtime, _) = build_runtime(cfg, audit.clone());
    let start = runtime
        .start(StartWorkflow {
            definition_id: "wf".into(),
            input: json!({}),
            principal: Principal::anonymous(),
            trace_id: None,
            run_id: None,
            depth: 0,
            parent: None,
        })
        .await
        .unwrap();
    let workflow_id = start["workflow"]["id"].as_str().unwrap().to_string();
    let version = start["workflow"]["version"].as_u64().unwrap();
    runtime
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

    let events = audit.list_events().await.expect("memory sink lists");
    let record = events
        .iter()
        .find(|e| e.event_type == "workflow.transition")
        .expect("a transition record must be emitted");
    let delta = record
        .payload
        .get("blackboardDelta")
        .and_then(Value::as_object)
        .expect("blackboardDelta must be an object");
    assert!(
        delta.is_empty(),
        "no output: writes → blackboardDelta should be empty; got: {delta:?}"
    );
}

#[tokio::test]
async fn guards_array_populated_with_kind_and_result() {
    // A transition with one passing `expr` guard must emit a record whose
    // `guards` array contains exactly that guard's {kind, result: true}.
    let cfg = json!({
        "version": "1.0.0",
        "workflows": {
            "wf": {
                "initialState": "draft",
                "initialContext": { "ready": true },
                "states": {
                    "draft": {
                        "transitions": {
                            "submit": {
                                "target": "done",
                                "actor": "agent",
                                "guards": [
                                    { "kind": "expr", "expr": "$.context.ready == true" }
                                ]
                            }
                        }
                    },
                    "done": { "terminal": true }
                }
            }
        }
    });
    let audit = Arc::new(MemoryAuditSink::new()) as Arc<dyn AuditSink>;
    let (runtime, _) = build_runtime(cfg, audit.clone());
    let start = runtime
        .start(StartWorkflow {
            definition_id: "wf".into(),
            input: json!({}),
            principal: Principal::anonymous(),
            trace_id: None,
            run_id: None,
            depth: 0,
            parent: None,
        })
        .await
        .unwrap();
    let workflow_id = start["workflow"]["id"].as_str().unwrap().to_string();
    let version = start["workflow"]["version"].as_u64().unwrap();
    runtime
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

    let events = audit.list_events().await.expect("memory sink lists");
    let record = events
        .iter()
        .find(|e| e.event_type == "workflow.transition")
        .expect("a transition record must be emitted");
    let guards = record
        .payload
        .get("guards")
        .and_then(Value::as_array)
        .expect("guards must be present and an array");
    assert_eq!(guards.len(), 1, "expected one guard entry; got {guards:?}");
    assert_eq!(guards[0]["kind"].as_str(), Some("expr"));
    assert_eq!(guards[0]["result"], json!(true));
}

#[tokio::test]
async fn blackboard_delta_chain_hops_have_distinct_deltas() {
    // Two-hop deterministic chain; each hop writes its own slot. Each emitted
    // record's delta must reflect only that hop's mutation.
    let cfg = json!({
        "version": "1.0.0",
        "workflows": {
            "pipeline": {
                "initialState": "a",
                "states": {
                    "a": {
                        "transitions": {
                            "s1": {
                                "target": "b",
                                "actor": "deterministic",
                                "executor": { "kind": "noop" },
                                "output": { "first": "$.output.v1" }
                            }
                        }
                    },
                    "b": {
                        "transitions": {
                            "s2": {
                                "target": "c",
                                "actor": "deterministic",
                                "executor": { "kind": "noop" },
                                "output": { "second": "$.output.v2" }
                            }
                        }
                    },
                    "c": { "terminal": true }
                }
            }
        }
    });

    let audit = Arc::new(MemoryAuditSink::new()) as Arc<dyn AuditSink>;
    let definitions = Arc::new(ConfigDefinitionStore::from_config(&cfg));
    let store = Arc::new(InMemoryWorkflowStore::new());
    let executors = Arc::new(SingleExecRegistry {
        inner: Arc::new(FixedOutputExecutor {
            output: json!({ "v1": "alpha", "v2": "beta" }),
        }),
    });
    let guards = Arc::new(praxec_core::guards::DefaultGuardEvaluator::new());
    let runtime = WorkflowRuntime::new(definitions, store, executors, guards, audit.clone());

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

    let events = audit.list_events().await.expect("memory sink lists");
    let records: Vec<_> = events
        .iter()
        .filter(|e| e.event_type == "workflow.transition")
        .collect();
    assert_eq!(
        records.len(),
        2,
        "expected 2 chain-hop records; got {}",
        records.len()
    );

    // Hop 1 writes only `first`; the executor's $.output.v1 == "alpha".
    let d1 = records[0].payload.get("blackboardDelta").unwrap();
    assert_eq!(d1["first"], json!("alpha"));
    assert!(
        d1.get("second").is_none(),
        "delta for hop 1 must not include slots written by hop 2: {d1}"
    );

    // Hop 2 writes only `second`.
    let d2 = records[1].payload.get("blackboardDelta").unwrap();
    assert_eq!(d2["second"], json!("beta"));
    assert!(
        d2.get("first").is_none(),
        "delta for hop 2 must not re-include hop 1's slot: {d2}"
    );
}
