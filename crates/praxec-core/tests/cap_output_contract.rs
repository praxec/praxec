//! SPEC §5.3 — a definition owes its declared outputs at its OWN terminal.
//!
//! The compose path (`WorkflowExecutor`) has always validated a capability's
//! `snippet.outputs` against the host's `use.outputs` projection. Nothing
//! validated a DIRECT run. That asymmetry is what these tests pin shut: an
//! author could run a cap on its own, see green, and only learn the output
//! contract was violated once someone wrapped it in a `use:` block.
//!
//! The guarantee under test is the useful one:
//!
//! > a green direct run implies a green composed run.
//!
//! which holds because the terminal check IS the compose check evaluated
//! under a synthesized full identity binding — the strictest host any
//! composer could be.

mod common;
use common::chain::{FixedExecutor, build_runtime_with_executor};

use praxec_core::model::{Principal, StartWorkflow};
use serde_json::{Value, json};
use std::sync::Arc;

/// A capability whose single deterministic transition maps the executor's
/// output into `$.context.verdict`, then goes terminal. `snippet.outputs`
/// declares `verdict` as a HOP-style closed object so a stray key is a
/// contract violation — the exact shape of the dogfooding report's
/// `provenance.mode` defect.
fn verify_cap() -> Value {
    json!({
        "version": "1.0.0",
        "workflows": {
            "cap.verify.thing": {
                "initialState": "ready",
                "verb": "verify",
                "snippet": {
                    "inputs": {},
                    "outputs": {
                        "verdict": {
                            "type": "object",
                            "properties": {
                                "passed":     { "type": "boolean" },
                                "provenance": {
                                    "type": "object",
                                    "properties": { "stack": { "type": "string" } },
                                    "additionalProperties": false
                                }
                            },
                            "required": ["passed"],
                            "additionalProperties": false
                        }
                    }
                },
                "states": {
                    "ready": {
                        "transitions": {
                            "run": {
                                "target": "done",
                                "actor": "deterministic",
                                "executor": { "kind": "noop" },
                                "output": { "verdict": "$.output.verdict" }
                            }
                        }
                    },
                    "done": { "terminal": true }
                }
            }
        }
    })
}

async fn run_cap_directly(executor_output: Value) -> Value {
    let executor = Arc::new(FixedExecutor::new(executor_output));
    let (runtime, _audit) = build_runtime_with_executor(verify_cap(), executor);
    runtime
        .start(StartWorkflow {
            definition_id: "cap.verify.thing".into(),
            input: json!({}),
            principal: Principal::anonymous(),
            trace_id: None,
            run_id: None,
            depth: 0,
            parent: None,
        })
        .await
        .expect("start returns a response, not a transport error")
}

#[tokio::test]
async fn a_direct_cap_run_honoring_its_snippet_contract_still_succeeds() {
    let resp = run_cap_directly(json!({
        "verdict": { "passed": true, "provenance": { "stack": "language:dotnet" } }
    }))
    .await;

    assert_eq!(resp["workflow"]["state"], "done");
    assert_eq!(
        resp["result"]["status"], "succeeded",
        "a conforming cap must not be touched by the new terminal check: {resp}"
    );
}

#[tokio::test]
async fn a_direct_cap_run_producing_a_stray_key_fails_instead_of_reporting_green() {
    // `provenance.mode` is the stray key. `stackProvenance` is CLOSED
    // (`additionalProperties: false`), so this verdict cannot satisfy the
    // declared contract — and before this check, a direct run reported it as a
    // clean success and only blew up once a flow composed the cap.
    let resp = run_cap_directly(json!({
        "verdict": {
            "passed": true,
            "provenance": { "stack": "language:dotnet", "mode": "full" }
        }
    }))
    .await;

    assert_eq!(
        resp["result"]["status"], "failed",
        "a cap violating its own declared output contract must fail its direct \
         run, not hand back a green verdict: {resp}"
    );
    assert_eq!(resp["error"]["errorClass"], "cap_output_schema_violation");
    let message = resp["error"]["message"].as_str().unwrap_or_default();
    assert!(
        message.contains("verdict"),
        "the failure must name the offending output slot: {message}"
    );
}

#[tokio::test]
async fn a_direct_cap_run_omitting_a_declared_output_fails() {
    // The other half of the asymmetry: a cap that simply never writes a
    // declared output. `project_use_outputs` renders the absent key as `Null`,
    // which cannot satisfy `{"type": "object", ...}` — so composing this cap
    // would already fail today. The terminal check just refuses to let the
    // direct run call it green first.
    let resp = run_cap_directly(json!({ "something_else": true })).await;

    assert_eq!(
        resp["result"]["status"], "failed",
        "an unwritten declared output must fail the direct run: {resp}"
    );
    assert_eq!(resp["error"]["errorClass"], "cap_output_schema_violation");
}

#[tokio::test]
async fn the_violation_is_recorded_as_a_cap_output_schema_violation_event() {
    let executor = Arc::new(FixedExecutor::new(json!({
        "verdict": { "passed": true, "provenance": { "mode": "full" } }
    })));
    let (runtime, audit) = build_runtime_with_executor(verify_cap(), executor);
    let _ = runtime
        .start(StartWorkflow {
            definition_id: "cap.verify.thing".into(),
            input: json!({}),
            principal: Principal::anonymous(),
            trace_id: None,
            run_id: None,
            depth: 0,
            parent: None,
        })
        .await
        .expect("start returns a response");

    let events = audit.snapshot();
    let violation = events
        .iter()
        .find(|e| e.event_type == "cap.output.schema_violation")
        .expect("the terminal check must leave an audit trail of WHY it failed");
    assert_eq!(violation.payload["caughtAt"], "terminal");
    assert_eq!(violation.payload["definitionId"], "cap.verify.thing");
    assert_eq!(
        violation.payload["violations"][0]["slot"], "verdict",
        "the event must carry the offending slot: {:?}",
        violation.payload
    );
}

/// The deterministic-repair rung (P12 R3.1) runs on the compose path before
/// validation. The terminal check must not be HARSHER than the compose check it
/// mirrors, or a cap that composes cleanly would fail when run directly — the
/// asymmetry, merely inverted.
#[tokio::test]
async fn a_deterministically_repairable_null_output_does_not_fail_the_terminal_check() {
    let config = json!({
        "version": "1.0.0",
        "workflows": {
            "cap.review.thing": {
                "initialState": "ready",
                "verb": "review",
                "snippet": {
                    "inputs":  {},
                    "outputs": { "findings": { "type": "array" } }
                },
                "states": {
                    "ready": {
                        "transitions": {
                            "run": {
                                "target": "done",
                                "actor": "deterministic",
                                "executor": { "kind": "noop" },
                                "output": { "findings": "$.output.findings" }
                            }
                        }
                    },
                    "done": { "terminal": true }
                }
            }
        }
    });
    // A commodity model emitting an explicit `null` for an array field is the
    // canonical repairable miss: `null` → `[]`, zero model calls.
    let executor = Arc::new(FixedExecutor::new(json!({ "findings": Value::Null })));
    let (runtime, _audit) = build_runtime_with_executor(config, executor);
    let resp = runtime
        .start(StartWorkflow {
            definition_id: "cap.review.thing".into(),
            input: json!({}),
            principal: Principal::anonymous(),
            trace_id: None,
            run_id: None,
            depth: 0,
            parent: None,
        })
        .await
        .expect("start returns a response");

    assert_eq!(
        resp["result"]["status"], "succeeded",
        "the repair rung must run before the terminal check, exactly as it does \
         under `use:`: {resp}"
    );
}

/// A definition declaring NO outputs (`snippet.outputs: {}`) has no contract to
/// break. The check must be inert for it — most caps in the wild are this shape,
/// and a check that fires on them would be a brownout, not a guard.
#[tokio::test]
async fn a_cap_declaring_no_outputs_is_untouched() {
    let config = json!({
        "version": "1.0.0",
        "workflows": {
            "cap.record.thing": {
                "initialState": "ready",
                "verb": "record",
                "snippet": { "inputs": {}, "outputs": {} },
                "states": {
                    "ready": {
                        "transitions": {
                            "run": {
                                "target": "done",
                                "actor": "deterministic",
                                "executor": { "kind": "noop" }
                            }
                        }
                    },
                    "done": { "terminal": true }
                }
            }
        }
    });
    let executor = Arc::new(FixedExecutor::new(json!({})));
    let (runtime, _audit) = build_runtime_with_executor(config, executor);
    let resp = runtime
        .start(StartWorkflow {
            definition_id: "cap.record.thing".into(),
            input: json!({}),
            principal: Principal::anonymous(),
            trace_id: None,
            run_id: None,
            depth: 0,
            parent: None,
        })
        .await
        .expect("start returns a response");

    assert_eq!(resp["result"]["status"], "succeeded");
}
