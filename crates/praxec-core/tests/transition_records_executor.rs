//! SPEC §7.2 — executor descriptor on `workflow.transition` records.
//!
//! When a transition's executor actually runs, the record's `executor` field
//! must carry `{ kind, ok, durationMs }`. When no executor is declared, the
//! field must be absent entirely (no partial descriptor).
//!
//! Split sibling of `transition_records_basic.rs`; shared fixtures live in
//! `tests/common/transition_records.rs`.

use std::sync::Arc;

use praxec_core::audit::{AuditSink, MemoryAuditSink};
use praxec_core::model::{Principal, StartWorkflow, SubmitTransition};
use serde_json::{json, Value};

mod common;
use common::transition_records::*;

#[tokio::test]
async fn executor_descriptor_carries_kind_ok_and_duration_ms() {
    // SPEC §7.2: the record's `executor` carries `{ kind, ok, durationMs }`
    // when the transition's executor actually ran.
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
                                "executor": { "kind": "noop" }
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
    let executor = record
        .payload
        .get("executor")
        .and_then(Value::as_object)
        .expect("executor descriptor must be present");
    assert_eq!(executor.get("kind").and_then(Value::as_str), Some("noop"));
    assert_eq!(executor.get("ok").and_then(Value::as_bool), Some(true));
    assert!(
        executor.get("durationMs").and_then(Value::as_u64).is_some(),
        "durationMs must be present as a non-negative integer; got: {executor:?}"
    );
}

#[tokio::test]
async fn executor_descriptor_omitted_when_no_executor_runs() {
    // A transition without an executor declared must NOT emit a partial
    // descriptor — schema says the field is optional and absent is the
    // honest signal that nothing ran.
    let cfg = json!({
        "version": "1.0.0",
        "workflows": {
            "wf": {
                "initialState": "draft",
                "states": {
                    "draft": {
                        "transitions": { "submit": { "target": "done", "actor": "agent" } }
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
    assert!(
        record.payload.get("executor").is_none(),
        "no executor declared → no executor descriptor on the record; got: {}",
        record.payload
    );
}
