//! Invariants 1-3: proxy compile + input schema + guards-before-executor.
//!
//! Split from `tests/invariants.rs` (SPLIT-002). Shared fixtures live in
//! `tests/common/invariants.rs`.

mod common;

use common::invariants::*;

use praxec_core::model::{Principal, StartWorkflow, SubmitTransition};
use praxec_core::proxy_workflow::{compile_proxy_workflow, DEFAULT_PROXY_WORKFLOW_ID};
use serde_json::json;

// ---- 1. Proxy exposure compiles to a null-op workflow transition -----------

#[test]
fn invariant_1_proxy_compiles_to_null_op_workflow() {
    let cfg = proxy_config();
    let workflow = compile_proxy_workflow(&cfg).expect("proxy workflow");
    assert_eq!(workflow.pointer("/initialState").unwrap(), "ready");
    let transition = workflow
        .pointer("/states/ready/transitions/echo")
        .expect("echo transition");
    assert_eq!(transition.get("target").unwrap(), "ready");
}

// ---- 2. All transitions validate inputSchema before execution --------------

#[tokio::test]
async fn invariant_2_input_schema_is_validated_before_executor() {
    let (runtime, exec, _) = build_runtime(proxy_config(), json!({}));

    let started = runtime
        .start(StartWorkflow {
            definition_id: DEFAULT_PROXY_WORKFLOW_ID.into(),
            input: json!({}),
            principal: Principal::anonymous(),
            trace_id: None,
            run_id: None,
            depth: 0,
            parent: None,
        })
        .await
        .unwrap();
    let workflow_id = started["workflow"]["id"].as_str().unwrap().to_string();
    let version = started["workflow"]["version"].as_u64().unwrap();

    // Bad input: msg is required.
    let resp = runtime
        .submit(SubmitTransition {
            workflow_id: workflow_id.clone(),
            expected_version: version,
            transition: "echo".into(),
            arguments: json!({}),
            principal: Principal::anonymous(),
            summary: None,
            trace_id: None,
            run_id: None,
        })
        .await
        .unwrap();
    assert_eq!(resp["result"]["status"], "running");
    assert_eq!(resp["error"]["code"], "INPUT_SCHEMA_VIOLATION");
    assert_eq!(exec.count(), 0, "executor must not run on schema violation");
}

// ---- 3. Guards run before executor dispatch --------------------------------

#[tokio::test]
async fn invariant_3_guards_run_before_executor() {
    let (runtime, exec, _) = build_runtime(governed_config(), json!({}));
    let started = runtime
        .start(StartWorkflow {
            definition_id: "demo".into(),
            input: json!({}),
            principal: Principal::anonymous(),
            trace_id: None,
            run_id: None,
            depth: 0,
            parent: None,
        })
        .await
        .unwrap();
    let workflow_id = started["workflow"]["id"].as_str().unwrap().to_string();
    let version = started["workflow"]["version"].as_u64().unwrap();

    // No permission → guard rejects.
    let denied = runtime
        .submit(SubmitTransition {
            workflow_id: workflow_id.clone(),
            expected_version: version,
            transition: "approve".into(),
            arguments: json!({}),
            principal: Principal::anonymous(),
            summary: None,
            trace_id: None,
            run_id: None,
        })
        .await
        .unwrap();
    assert_eq!(denied["error"]["code"], "GUARD_REJECTED");
    assert_eq!(exec.count(), 0, "executor must not run when guard rejects");
}
