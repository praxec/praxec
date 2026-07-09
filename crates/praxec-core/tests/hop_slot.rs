//! Slice-2 enforcement tests for the `hop_slot:` primitive (Spec A / A.1 §3).
//!
//! A transition declaring `hop_slot: <name>` has, at config load, its canonical
//! `In` contract injected as `inputSchema` and its `Out` contract injected as
//! the `$.context.<name>` typed blackboard slot — both by `$ref` into the
//! shipped HOP vocabulary (`praxec://hop`). Enforcement is then pure reuse:
//! the existing `validate_schema` (input) and `validate_blackboard_writes`
//! (output) seams. These tests prove the contract is UNBYPASSABLE end-to-end:
//! a malformed `verifyOut` written to `$.context.verify` is rejected before the
//! transition advances, exactly as a hand-declared typed slot would be.

use std::sync::Arc;

use async_trait::async_trait;
use praxec_core::WorkflowRuntime;
use praxec_core::audit::{AuditSink, MemoryAuditSink};
use praxec_core::guards::DefaultGuardEvaluator;
use praxec_core::model::{Principal, StartWorkflow, SubmitTransition};
use praxec_core::ports::{Executor, ExecutorRegistry};
use praxec_core::store::{ConfigDefinitionStore, InMemoryWorkflowStore};
use serde_json::{Value, json};

// ── harness (mirrors tests/blackboard_typing.rs) ──────────────────────────────

/// Executor that returns a controlled `output` value, so the test can drive the
/// post-write blackboard value into any shape.
struct FixedOutputExecutor {
    output: Value,
}

#[async_trait]
impl Executor for FixedOutputExecutor {
    async fn execute(
        &self,
        _: praxec_core::model::ExecuteRequest,
    ) -> Result<praxec_core::model::ExecuteResult, praxec_core::error::ExecutorError> {
        Ok(praxec_core::model::ExecuteResult {
            output: self.output.clone(),
            evidence: vec![],
            child_workflow_id: None,
            next_transition: None,
            suspend: None,
            telemetry: None,
        })
    }
}

struct SingleExecRegistry {
    inner: Arc<dyn Executor>,
}

impl ExecutorRegistry for SingleExecRegistry {
    fn get(&self, _kind: &str) -> Option<Arc<dyn Executor>> {
        Some(self.inner.clone())
    }
}

/// A workflow whose single `gate.run` transition is `hop_slot: verify`, writing
/// the executor output into `$.context.verify`. Raw (pre-resolve) so the load
/// pass performs the injection.
fn hop_slot_verify_config() -> Value {
    json!({
        "version": "1.0.0",
        "workflows": {
            "sdlc": {
                "initialState": "gate",
                "states": {
                    "gate": {
                        "transitions": {
                            "run": {
                                "target": "done",
                                "actor": "agent",
                                "hop_slot": "verify",
                                "executor": { "kind": "noop" },
                                "output": { "verify": "$.output" }
                            }
                        }
                    },
                    "done": { "terminal": true }
                }
            }
        }
    })
}

fn build_runtime(config: Value, executor_output: Value) -> WorkflowRuntime {
    let definitions = Arc::new(ConfigDefinitionStore::from_config(&config));
    let store = Arc::new(InMemoryWorkflowStore::new());
    let executors = Arc::new(SingleExecRegistry {
        inner: Arc::new(FixedOutputExecutor {
            output: executor_output,
        }),
    });
    let guards = Arc::new(DefaultGuardEvaluator::new());
    let audit = Arc::new(MemoryAuditSink::new());
    WorkflowRuntime::new(
        definitions,
        store,
        executors,
        guards,
        audit as Arc<dyn AuditSink>,
    )
}

/// A well-formed `verifyOut` instance.
fn valid_verify_out() -> Value {
    json!({
        "status": "pass",
        "summary": "criteria met",
        "criteria": [{ "id": "c1", "met": true, "evidence": "green" }],
        "findings": [],
        "provenance": { "stack": "language:rust", "source": "pack" }
    })
}

async fn start_and_submit(runtime: &WorkflowRuntime, output_valid_input: bool) -> Value {
    let start = runtime
        .start(StartWorkflow {
            definition_id: "sdlc".into(),
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
    // `arguments` must satisfy the injected `verifyIn` (requires `cwd`) — proves
    // the input contract was injected too.
    let arguments = if output_valid_input {
        json!({ "cwd": "." })
    } else {
        json!({ "not_a_verify_in_field": true })
    };
    runtime
        .submit(SubmitTransition {
            workflow_id,
            expected_version: version,
            transition: "run".into(),
            arguments,
            principal: Principal::anonymous(),
            summary: None,
            trace_id: None,
            run_id: None,
        })
        .await
        .unwrap()
}

// ── load-time injection ───────────────────────────────────────────────────────

#[test]
fn hop_slot_injects_input_and_output_contracts() {
    let resolved = praxec_core::config::resolve(hop_slot_verify_config()).expect("resolves");
    let t = resolved
        .pointer("/workflows/sdlc/states/gate/transitions/run")
        .expect("transition present");

    // (a) inputSchema injected as the verifyIn $ref.
    assert_eq!(
        t.pointer("/inputSchema"),
        Some(&json!({ "$ref": "praxec://hop#/$defs/verifyIn" })),
        "verifyIn must be injected as the transition inputSchema"
    );

    // (b) typed blackboard slot `verify` injected as the verifyOut $ref.
    assert_eq!(
        resolved.pointer("/workflows/sdlc/blackboard/verify"),
        Some(&json!({ "$ref": "praxec://hop#/$defs/verifyOut" })),
        "verifyOut must be injected as the $.context.verify blackboard slot"
    );
}

#[test]
fn hop_slot_does_not_clobber_an_explicit_input_schema() {
    let mut cfg = hop_slot_verify_config();
    // Author supplies an explicit inputSchema — the engine must not overwrite it.
    *cfg.pointer_mut("/workflows/sdlc/states/gate/transitions/run")
        .unwrap()
        .as_object_mut()
        .unwrap() = {
        let mut m = cfg
            .pointer("/workflows/sdlc/states/gate/transitions/run")
            .unwrap()
            .as_object()
            .unwrap()
            .clone();
        m.insert("inputSchema".into(), json!({ "type": "object" }));
        m
    };
    let resolved = praxec_core::config::resolve(cfg).expect("resolves");
    assert_eq!(
        resolved.pointer("/workflows/sdlc/states/gate/transitions/run/inputSchema"),
        Some(&json!({ "type": "object" })),
        "an explicit inputSchema must be preserved (author may narrow)"
    );
    // But the Out slot is still engine-owned/injected.
    assert_eq!(
        resolved.pointer("/workflows/sdlc/blackboard/verify"),
        Some(&json!({ "$ref": "praxec://hop#/$defs/verifyOut" }))
    );
}

#[test]
fn unknown_hop_slot_name_is_a_load_error() {
    let mut cfg = hop_slot_verify_config();
    cfg.pointer_mut("/workflows/sdlc/states/gate/transitions/run")
        .unwrap()
        .as_object_mut()
        .unwrap()
        .insert("hop_slot".into(), json!("frobnicate"));
    let err = praxec_core::config::resolve(cfg).expect_err("unknown slot must fail load");
    let msg = err.to_string();
    assert!(
        msg.contains("HOP_SLOT_UNKNOWN") && msg.contains("frobnicate"),
        "error must name the offending slot: {msg}"
    );
    assert!(
        msg.contains("verify") && msg.contains("lint_format"),
        "error must list the valid slot names: {msg}"
    );
}

// ── runtime enforcement (the unbypassable guarantee) ──────────────────────────

#[tokio::test]
async fn valid_verify_out_advances_the_transition() {
    let resolved = praxec_core::config::resolve(hop_slot_verify_config()).unwrap();
    let runtime = build_runtime(resolved, valid_verify_out());
    let resp = start_and_submit(&runtime, true).await;
    assert!(
        resp["error"].is_null(),
        "a conforming verifyOut must pass; got: {resp}"
    );
    assert_eq!(resp["workflow"]["state"], "done");
    assert_eq!(resp["context"]["verify"]["status"], "pass");
}

#[tokio::test]
async fn verify_out_with_bad_status_enum_is_rejected() {
    let mut bad = valid_verify_out();
    bad["status"] = json!("green"); // not a member of gateStatus
    let resolved = praxec_core::config::resolve(hop_slot_verify_config()).unwrap();
    let runtime = build_runtime(resolved, bad);

    let start = runtime
        .start(StartWorkflow {
            definition_id: "sdlc".into(),
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
    let pre_version = start["workflow"]["version"].as_u64().unwrap();

    let resp = runtime
        .submit(SubmitTransition {
            workflow_id,
            expected_version: pre_version,
            transition: "run".into(),
            arguments: json!({ "cwd": "." }),
            principal: Principal::anonymous(),
            summary: None,
            trace_id: None,
            run_id: None,
        })
        .await
        .unwrap();

    assert_eq!(
        resp["error"]["code"].as_str(),
        Some("BLACKBOARD_TYPE_ERROR"),
        "a bad status enum must be rejected via the injected verifyOut slot; got: {}",
        resp["error"]
    );
    assert!(
        resp["error"]["message"]
            .as_str()
            .unwrap_or_default()
            .contains("verify")
    );
    // Snapshot version unchanged — the transition was aborted, not committed.
    assert_eq!(
        resp["workflow"]["version"].as_u64(),
        Some(pre_version),
        "version must not advance on a rejected slot write"
    );
}

#[tokio::test]
async fn verify_out_finding_fix_missing_schema_ref_is_rejected() {
    // A finding.fix (SchemaBound) missing its required `schema_ref` — proves the
    // nested $ref chain (verifyOut -> finding -> schemaBound) resolves through the
    // registry at the blackboard seam.
    let mut bad = valid_verify_out();
    bad["status"] = json!("fail");
    bad["findings"] = json!([{
        "file": "src/lib.rs",
        "line": 10,
        "rule_id": "r1",
        "severity": "error",
        "message": "bad",
        "fix": { "value": { "kind": "manual" } }
    }]);
    let resolved = praxec_core::config::resolve(hop_slot_verify_config()).unwrap();
    let runtime = build_runtime(resolved, bad);
    let resp = start_and_submit(&runtime, true).await;
    assert_eq!(
        resp["error"]["code"].as_str(),
        Some("BLACKBOARD_TYPE_ERROR"),
        "a finding.fix missing schema_ref must be rejected; got: {}",
        resp["error"]
    );
}

#[tokio::test]
async fn injected_verify_in_rejects_nonconforming_arguments() {
    // The input contract is enforced too: arguments that violate verifyIn
    // (additionalProperties:false, missing required `cwd`) are rejected at submit.
    let resolved = praxec_core::config::resolve(hop_slot_verify_config()).unwrap();
    let runtime = build_runtime(resolved, valid_verify_out());
    let resp = start_and_submit(&runtime, false).await;
    assert_eq!(
        resp["error"]["code"].as_str(),
        Some("INPUT_SCHEMA_VIOLATION"),
        "arguments violating the injected verifyIn must be rejected; got: {}",
        resp["error"]
    );
}
