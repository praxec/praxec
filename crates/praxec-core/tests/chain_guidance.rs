//! Tests for phase guidance — `goal`/`guidance` body on workflow states.
//!
//! Guidance surfaces contextual instructions in every workflow response.
//! These tests verify it appears when configured and is omitted otherwise.

mod common;
use common::chain::*;

use praxec_core::model::{Principal, StartWorkflow};
use serde_json::json;

// ---- 8. Phase guidance in responses -----------------------------------------

#[tokio::test]
async fn phase_guidance_appears_in_response() {
    let (runtime, _, _) = build_runtime(linear_chain_stops_at_agent());
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

    // After chain, we're at state "b" which has goal and guidance
    assert_eq!(resp["workflow"]["state"], "b");
    assert_eq!(resp["guidance"]["goal"], "Review validation results");
    assert_eq!(
        resp["guidance"]["instructions"],
        "Check the context for validation output before proceeding"
    );
}

// ---- 9. Phase guidance absent when state has none ---------------------------

#[tokio::test]
async fn phase_guidance_absent_when_not_configured() {
    let cfg = json!({
        "version": "1.0.0",
        "workflows": {
            "plain": {
                "initialState": "a",
                "states": {
                    "a": {
                        "transitions": {
                            "go": { "target": "b", "actor": "agent", "executor": { "kind": "noop" } }
                        }
                    },
                    "b": { "terminal": true }
                }
            }
        }
    });

    let (runtime, _, _) = build_runtime(cfg);
    let resp = runtime
        .start(StartWorkflow {
            definition_id: "plain".into(),
            input: json!({}),
            principal: Principal::anonymous(),
            trace_id: None,
            run_id: None,
            depth: 0,
            parent: None,
        })
        .await
        .unwrap();

    assert!(
        resp.get("guidance").is_none(),
        "guidance should not appear when state has no goal/guidance"
    );
}
