//! Invariants 4-8 + 10: executor failure, invalid transitions, stale
//! version, version increment, terminal no-links, and unknown-transition
//! never-invokes-executor.
//!
//! Split from `tests/invariants.rs` (SPLIT-002). Shared fixtures live in
//! `tests/common/invariants.rs`.

mod common;

use std::sync::atomic::Ordering;

use common::invariants::*;

use praxec_core::model::{Principal, StartWorkflow, SubmitTransition};
use serde_json::json;

// ---- 4. Executors never decide workflow legality ---------------------------

#[tokio::test]
async fn invariant_4_executor_failure_yields_failed_not_advanced_state() {
    let (runtime, exec, _) = build_runtime(governed_config(), json!({}));
    // configure the executor to fail enough times to exhaust default retries
    exec.failures_left.store(10, Ordering::SeqCst);

    let started = runtime
        .start(StartWorkflow {
            definition_id: "demo".into(),
            input: json!({}),
            principal: Principal::anonymous(),
            run_env: praxec_core::RunEnv::for_test(),
            depth: 0,
            parent: None,
        })
        .await
        .unwrap();
    let workflow_id = started["workflow"]["id"].as_str().unwrap().to_string();
    let version = started["workflow"]["version"].as_u64().unwrap();

    let resp = runtime
        .submit(SubmitTransition {
            workflow_id: workflow_id.clone(),
            expected_version: version,
            transition: "approve".into(),
            arguments: json!({}),
            principal: principal_with(&["demo.approve"]),
            summary: None,
            trace_id: None,
            run_id: None,
        })
        .await
        .unwrap();

    assert_eq!(resp["result"]["status"], "failed");
    // state must remain `open`, not `done`
    assert_eq!(resp["workflow"]["state"], "open");
}

// ---- 5. Invalid transitions return current legal links ---------------------

#[tokio::test]
async fn invariant_5_invalid_transitions_return_current_links() {
    let (runtime, _, _) = build_runtime(governed_config(), json!({}));
    let started = runtime
        .start(StartWorkflow {
            definition_id: "demo".into(),
            input: json!({}),
            principal: Principal::anonymous(),
            run_env: praxec_core::RunEnv::for_test(),
            depth: 0,
            parent: None,
        })
        .await
        .unwrap();
    let workflow_id = started["workflow"]["id"].as_str().unwrap().to_string();
    let version = started["workflow"]["version"].as_u64().unwrap();

    let resp = runtime
        .submit(SubmitTransition {
            workflow_id,
            expected_version: version,
            transition: "definitely_not_a_thing".into(),
            arguments: json!({}),
            principal: Principal::anonymous(),
            summary: None,
            trace_id: None,
            run_id: None,
        })
        .await
        .unwrap();
    assert_eq!(resp["error"]["code"], "INVALID_TRANSITION");
    let rels: Vec<&str> = resp["links"]
        .as_array()
        .unwrap()
        .iter()
        .map(|l| l["rel"].as_str().unwrap())
        .collect();
    assert!(
        rels.contains(&"approve"),
        "rejected response must list legal links"
    );
}

// ---- 6. Every submit requires expectedVersion ------------------------------

#[tokio::test]
async fn invariant_6_stale_expected_version_is_rejected() {
    let (runtime, _, _) = build_runtime(governed_config(), json!({}));
    let started = runtime
        .start(StartWorkflow {
            definition_id: "demo".into(),
            input: json!({}),
            principal: Principal::anonymous(),
            run_env: praxec_core::RunEnv::for_test(),
            depth: 0,
            parent: None,
        })
        .await
        .unwrap();
    let workflow_id = started["workflow"]["id"].as_str().unwrap().to_string();
    let actual_version = started["workflow"]["version"].as_u64().unwrap();

    let resp = runtime
        .submit(SubmitTransition {
            workflow_id,
            expected_version: actual_version + 99,
            transition: "approve".into(),
            arguments: json!({}),
            principal: principal_with(&["demo.approve"]),
            summary: None,
            trace_id: None,
            run_id: None,
        })
        .await
        .unwrap();
    assert_eq!(resp["error"]["code"], "STALE_WORKFLOW_VERSION");
}

// ---- 7. Every successful transition increments workflow.version -----------

#[tokio::test]
async fn invariant_7_successful_transition_increments_version() {
    let (runtime, _, _) = build_runtime(governed_config(), json!({}));
    let started = runtime
        .start(StartWorkflow {
            definition_id: "demo".into(),
            input: json!({}),
            principal: Principal::anonymous(),
            run_env: praxec_core::RunEnv::for_test(),
            depth: 0,
            parent: None,
        })
        .await
        .unwrap();
    let workflow_id = started["workflow"]["id"].as_str().unwrap().to_string();
    let version_before = started["workflow"]["version"].as_u64().unwrap();

    let after = runtime
        .submit(SubmitTransition {
            workflow_id,
            expected_version: version_before,
            transition: "approve".into(),
            arguments: json!({}),
            principal: principal_with(&["demo.approve"]),
            summary: None,
            trace_id: None,
            run_id: None,
        })
        .await
        .unwrap();
    let version_after = after["workflow"]["version"].as_u64().unwrap();
    assert!(
        version_after > version_before,
        "version must increase on successful transition (was {version_before}, now {version_after})"
    );
}

// ---- 8. Terminal states return no links ------------------------------------

#[tokio::test]
async fn invariant_8_terminal_state_has_no_links() {
    let (runtime, _, _) = build_runtime(governed_config(), json!({}));
    let started = runtime
        .start(StartWorkflow {
            definition_id: "demo".into(),
            input: json!({}),
            principal: Principal::anonymous(),
            run_env: praxec_core::RunEnv::for_test(),
            depth: 0,
            parent: None,
        })
        .await
        .unwrap();
    let workflow_id = started["workflow"]["id"].as_str().unwrap().to_string();
    let version = started["workflow"]["version"].as_u64().unwrap();

    let after = runtime
        .submit(SubmitTransition {
            workflow_id,
            expected_version: version,
            transition: "approve".into(),
            arguments: json!({}),
            principal: principal_with(&["demo.approve"]),
            summary: None,
            trace_id: None,
            run_id: None,
        })
        .await
        .unwrap();

    assert_eq!(after["workflow"]["state"], "done");
    assert_eq!(after["result"]["status"], "succeeded");
    let links = after["links"].as_array().unwrap();
    assert!(links.is_empty(), "terminal state must return no links");
}

// ---- 10. Downstream tools only reachable through configured transitions ----
//
// This invariant is structural: the runtime never invokes an executor outside
// of a transition or onEnter action. We assert it by checking that no executor
// calls happened when only `start` ran (no onEnter), and that an unknown
// transition does not call the executor.

#[tokio::test]
async fn invariant_10_unknown_transition_does_not_invoke_executor() {
    let (runtime, exec, _) = build_runtime(governed_config(), json!({}));
    let started = runtime
        .start(StartWorkflow {
            definition_id: "demo".into(),
            input: json!({}),
            principal: Principal::anonymous(),
            run_env: praxec_core::RunEnv::for_test(),
            depth: 0,
            parent: None,
        })
        .await
        .unwrap();
    assert_eq!(
        exec.count(),
        0,
        "start without onEnter must not call executor"
    );

    let workflow_id = started["workflow"]["id"].as_str().unwrap().to_string();
    let version = started["workflow"]["version"].as_u64().unwrap();
    let _ = runtime
        .submit(SubmitTransition {
            workflow_id,
            expected_version: version,
            transition: "ghost".into(),
            arguments: json!({}),
            principal: Principal::anonymous(),
            summary: None,
            trace_id: None,
            run_id: None,
        })
        .await
        .unwrap();
    assert_eq!(exec.count(), 0, "ghost transition must not call executor");
}
