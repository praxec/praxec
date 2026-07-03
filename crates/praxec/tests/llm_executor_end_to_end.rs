//! SPEC §33 D9 — end-to-end coverage for the in-runtime LLM executor.
//!
//! Each test wires a full submit pipeline (runtime + executors registry +
//! audit + transition resolver) against the SPEC §33 D9 adversarial
//! mock provider — no network, no real model. The assertions name
//! specific audit fields (event_type, error_code, tool_call_emitted)
//! and runtime outcomes (workflow state, ExecutorError class) so a
//! refactor that drops a wire-shape contract fails here loudly.

// The mock provider module lives under the llm-executor crate's
// integration tests dir. We import the module here as well so this
// e2e file can share scenarios without duplicating canned events.
//
// Rationale: cargo treats each `tests/<name>.rs` as a separate crate
// integration target, so we re-declare the module against an
// adjacent-but-shared path.
#[path = "../../praxec-llm-executor/tests/common/mod.rs"]
mod common;

use std::sync::Arc;

use praxec_core::WorkflowRuntime;
use praxec_core::audit::{AuditSink, MemoryAuditSink};
use praxec_core::error::{ExecutorError, LlmErrorCode};
use praxec_core::guards::DefaultGuardEvaluator;
use praxec_core::model::{Principal, StartWorkflow, SubmitTransition};
use praxec_core::ports::{Executor, ExecutorRegistry, TransitionResolver};
use praxec_core::runtime_transition_resolver::RuntimeTransitionResolver;
use praxec_core::store::{ConfigDefinitionStore, InMemoryWorkflowStore};
use praxec_core::validate::Diagnostic;
use praxec_executors::NoopExecutor;
use praxec_llm_executor::{LlmExecutor, ProviderFactory, cost::doctor_check};
use serde_json::{Value, json};

use common::mock_provider::{MockProviderFactory, MockProviderScenarios, MockScenario};

// ============================================================================
// Test wiring helpers.
// ============================================================================

/// A registry that routes `kind: "llm"` to the LlmExecutor and every
/// other kind to a `NoopExecutor`. Sufficient for the e2e workflows
/// which use `noop` for non-LLM transitions.
struct LlmOrNoopRegistry {
    llm: Arc<dyn Executor>,
    noop: Arc<dyn Executor>,
}

impl ExecutorRegistry for LlmOrNoopRegistry {
    fn get(&self, kind: &str) -> Option<Arc<dyn Executor>> {
        match kind {
            "llm" => Some(self.llm.clone()),
            "noop" => Some(self.noop.clone()),
            _ => None,
        }
    }
}

/// Build a runtime wired against the mock provider factory. Returns
/// the runtime, the audit sink (for snapshot assertions), and the
/// factory so tests can inspect what model strings were requested.
fn build_runtime(
    config: Value,
    factory: Arc<MockProviderFactory>,
) -> (
    WorkflowRuntime,
    Arc<MemoryAuditSink>,
    Arc<MockProviderFactory>,
) {
    let definitions = Arc::new(ConfigDefinitionStore::from_config(&config));
    let store = Arc::new(InMemoryWorkflowStore::new());
    let guards = Arc::new(DefaultGuardEvaluator::new());
    let audit = Arc::new(MemoryAuditSink::new());

    // Two-step wiring matches the binary's `register_llm_executor`
    // pattern: build runtime with a temporary registry, then re-build
    // with the LLM-aware registry once the resolver has a live runtime
    // to read from.
    let placeholder: Arc<dyn ExecutorRegistry> = Arc::new(LlmOrNoopRegistry {
        llm: Arc::new(NoopExecutor),
        noop: Arc::new(NoopExecutor),
    });
    let prelim_runtime = WorkflowRuntime::new(
        definitions.clone(),
        store.clone(),
        placeholder,
        guards.clone(),
        audit.clone() as Arc<dyn AuditSink>,
    );

    let resolver: Arc<dyn TransitionResolver> =
        Arc::new(RuntimeTransitionResolver::new(prelim_runtime));
    let factory_dyn: Arc<dyn ProviderFactory> = factory.clone();
    let llm_executor: Arc<dyn Executor> = Arc::new(LlmExecutor::with_provider_factory(
        audit.clone() as Arc<dyn AuditSink>,
        resolver,
        factory_dyn,
    ));
    let registry: Arc<dyn ExecutorRegistry> = Arc::new(LlmOrNoopRegistry {
        llm: llm_executor,
        noop: Arc::new(NoopExecutor),
    });

    let runtime = WorkflowRuntime::new(definitions, store, registry, guards, audit.clone());
    (runtime, audit, factory)
}

/// Tiny three-transition workflow with a single `kind: llm` triage
/// state. Mirrors the issue triager example.
fn triage_workflow() -> Value {
    json!({
        "version": "1.0.0",
        "workflows": {
            "triager": {
                "version": "2026-05-29",
                "initialState": "triaging",
                "states": {
                    "triaging": {
                        "transitions": {
                            "advance": {
                                "target": "investigating",
                                "actor": "agent",
                                "executor": {
                                    "kind": "llm",
                                    "model": "anthropic:claude-sonnet-4-6",
                                    "prompt_template": "triage this: {{ blackboard.issue_body }}"
                                }
                            },
                            "reject": {
                                "target": "closed",
                                "actor": "agent",
                                "executor": {
                                    "kind": "llm",
                                    "model": "anthropic:claude-sonnet-4-6",
                                    "prompt_template": "triage this: {{ blackboard.issue_body }}"
                                }
                            }
                        }
                    },
                    "investigating": { "terminal": true },
                    "closed":        { "terminal": true }
                }
            }
        }
    })
}

/// Chain workflow with three `kind: llm` states plus a terminating
/// `kind: noop` transition.
///
/// SPEC §33 D3 chain semantics: the executor's `next_transition.transition`
/// becomes the next chain-loop submit's transition NAME, dispatched
/// against the NEW workflow state. For the LLM executor this name comes
/// from the model's tool selection at the CURRENT state — so the chain
/// only progresses if each state happens to declare a transition with
/// the same name as the one the model just picked. The canonical way
/// to drive multiple LLM turns is to name the outgoing transition
/// consistently across states (here, `"next"` on every hop), then have
/// the executor at each state pick `"next"`. The terminating state
/// declares `"next"` as `kind: noop` so the chain breaks cleanly:
/// NoopExecutor returns `next_transition: None`.
fn chain_workflow() -> Value {
    json!({
        "version": "1.0.0",
        "workflows": {
            "chained": {
                "version": "2026-05-29",
                "initialState": "a",
                "states": {
                    "a": {
                        "transitions": {
                            "next": {
                                "target": "b",
                                "actor": "agent",
                                "executor": {
                                    "kind": "llm",
                                    "model": "anthropic:claude-sonnet-4-6",
                                    "prompt_template": "x"
                                }
                            }
                        }
                    },
                    "b": {
                        "transitions": {
                            "next": {
                                "target": "c",
                                "actor": "agent",
                                "executor": {
                                    "kind": "llm",
                                    "model": "anthropic:claude-sonnet-4-6",
                                    "prompt_template": "x"
                                }
                            }
                        }
                    },
                    "c": {
                        "transitions": {
                            "next": {
                                "target": "d",
                                "actor": "agent",
                                "executor": {
                                    "kind": "llm",
                                    "model": "anthropic:claude-sonnet-4-6",
                                    "prompt_template": "x"
                                }
                            }
                        }
                    },
                    "d": {
                        "transitions": {
                            "next": {
                                "target": "end",
                                "actor": "agent",
                                "executor": { "kind": "noop" }
                            }
                        }
                    },
                    "end": { "terminal": true }
                }
            }
        }
    })
}

/// Self-loop workflow used to exercise the chain depth cap. Each turn
/// the mock provider yields the same `loop` tool name, so the runtime
/// chains indefinitely until `max_chained_llm_turns` fires.
fn self_loop_workflow() -> Value {
    json!({
        "version": "1.0.0",
        "workflows": {
            "looper": {
                "version": "2026-05-29",
                "initialState": "spin",
                "states": {
                    "spin": {
                        "transitions": {
                            "loop": {
                                "target": "spin",
                                "actor": "agent",
                                "executor": {
                                    "kind": "llm",
                                    "model": "anthropic:claude-sonnet-4-6",
                                    "prompt_template": "x"
                                }
                            }
                        }
                    }
                }
            }
        }
    })
}

/// Variant of the happy-path scenario that returns a different
/// transition name. Used by the chain test so each turn picks the
/// correct outgoing edge for the corresponding state.
fn happy_with_tool(name: &'static str) -> MockScenario {
    use praxec_llm_executor::stream_event::{StopReason, StreamEvent, TokenUsage, ToolCallRequest};
    MockScenario {
        name: "happy_with_tool",
        events: vec![
            Ok(StreamEvent::ToolCall(ToolCallRequest {
                id: format!("call_{name}"),
                name: name.to_string(),
                arguments: "{}".into(),
            })),
            Ok(StreamEvent::Usage(TokenUsage {
                input_tokens: 10,
                output_tokens: 5,
                reasoning_tokens: None,
            })),
            Ok(StreamEvent::Done {
                stop_reason: Some(StopReason::ToolCalls),
            }),
        ],
    }
}

async fn start_workflow(runtime: &WorkflowRuntime, definition_id: &str) -> (String, u64) {
    let resp = runtime
        .start(StartWorkflow {
            definition_id: definition_id.into(),
            input: json!({}),
            principal: Principal::anonymous(),
            trace_id: None,
            run_id: None,
            depth: 0,
            parent: None,
        })
        .await
        .expect("workflow.start must succeed");
    let id = resp["workflow"]["id"]
        .as_str()
        .expect("response carries workflow.id")
        .to_string();
    let version = resp["workflow"]["version"]
        .as_u64()
        .expect("response carries workflow.version");
    (id, version)
}

fn submit(workflow_id: &str, version: u64, transition: &str) -> SubmitTransition {
    SubmitTransition {
        workflow_id: workflow_id.to_string(),
        expected_version: version,
        transition: transition.into(),
        arguments: json!({}),
        principal: Principal::anonymous(),
        summary: None,
        trace_id: None,
        run_id: None,
    }
}

// ============================================================================
// Tests.
// ============================================================================

#[tokio::test]
async fn e2e_happy_triage_advances_workflow() {
    let factory = Arc::new(MockProviderFactory::single(happy_with_tool("advance")));
    let (runtime, audit, _factory) = build_runtime(triage_workflow(), factory);

    let (wf_id, version) = start_workflow(&runtime, "triager").await;
    audit.clear();

    let resp = runtime
        .submit(submit(&wf_id, version, "advance"))
        .await
        .expect("happy-path submit must succeed");
    assert_eq!(
        resp["workflow"]["state"], "investigating",
        "happy path must land in investigating"
    );

    // Audit assertions: llm.invocation fired with tool_call_emitted; a
    // workflow.transitioned event fired.
    let snapshot = audit.snapshot();
    let invocation = snapshot
        .iter()
        .find(|e| e.event_type == "llm.invocation")
        .expect("llm.invocation audit event must fire");
    assert_eq!(
        invocation.payload.get("tool_call_emitted"),
        Some(&Value::from("advance")),
        "llm.invocation must record the chosen tool"
    );
    assert_eq!(
        invocation.payload.get("error_code"),
        Some(&Value::Null),
        "happy path llm.invocation has null error_code"
    );

    let transitioned_count = snapshot
        .iter()
        .filter(|e| e.event_type == "workflow.transitioned")
        .count();
    assert_eq!(
        transitioned_count, 1,
        "exactly one workflow.transitioned event in a single-turn happy path"
    );
}

/// Extract the executor error message from the runtime's HATEOAS-shaped
/// "failed" response. The runtime catches executor failures, records
/// `result.status: "failed"` plus an `error: { code, message, errorClass }`
/// block, and returns the response as `Ok`. Tests assert on the message
/// payload because that's the operator-facing wire shape.
fn assert_failed_with_llm_code(resp: &Value, expected: LlmErrorCode) {
    assert_eq!(
        resp["result"]["status"],
        Value::from("failed"),
        "failed-path response must carry result.status: failed. Full: {resp:?}"
    );
    let code = resp["error"]["code"]
        .as_str()
        .expect("failed response must include error.code");
    assert_eq!(
        code, "EXECUTOR_FAILED",
        "runtime wraps executor failures under EXECUTOR_FAILED"
    );
    let msg = resp["error"]["message"]
        .as_str()
        .expect("failed response must include error.message");
    assert!(
        msg.contains(expected.as_wire_code()),
        "error message must mention wire code {}: got `{msg}`",
        expected.as_wire_code()
    );
}

#[tokio::test]
async fn e2e_no_tool_call_returns_typed_error() {
    let factory = Arc::new(MockProviderFactory::single(
        MockProviderScenarios::no_tool_call(),
    ));
    let (runtime, audit, _factory) = build_runtime(triage_workflow(), factory);

    let (wf_id, version) = start_workflow(&runtime, "triager").await;
    audit.clear();

    let resp = runtime
        .submit(submit(&wf_id, version, "advance"))
        .await
        .expect("runtime returns Ok(failed-response) on typed executor errors");
    assert_failed_with_llm_code(&resp, LlmErrorCode::NoToolCall);
    assert_eq!(
        resp["workflow"]["state"], "triaging",
        "failed path must leave the workflow at the originating state"
    );

    // Audit must record the failure with the wire code.
    let snapshot = audit.snapshot();
    let invocation = snapshot
        .iter()
        .find(|e| e.event_type == "llm.invocation")
        .expect("llm.invocation fires on the failure path");
    assert_eq!(
        invocation.payload.get("error_code"),
        Some(&Value::from("LLM_NO_TOOL_CALL")),
        "failure llm.invocation must carry wire code"
    );
    assert_eq!(
        invocation.payload.get("tool_call_emitted"),
        Some(&Value::Null),
        "no tool emitted on no-tool-call failure"
    );
}

#[tokio::test]
async fn e2e_multi_tool_call_rejected() {
    let factory = Arc::new(MockProviderFactory::single(
        MockProviderScenarios::multi_tool_call(),
    ));
    let (runtime, audit, _factory) = build_runtime(triage_workflow(), factory);

    let (wf_id, version) = start_workflow(&runtime, "triager").await;
    audit.clear();

    let resp = runtime
        .submit(submit(&wf_id, version, "advance"))
        .await
        .expect("runtime returns Ok(failed-response) on typed executor errors");
    assert_failed_with_llm_code(&resp, LlmErrorCode::MultiToolCall);

    let snapshot = audit.snapshot();
    let invocation = snapshot
        .iter()
        .find(|e| e.event_type == "llm.invocation")
        .expect("llm.invocation fires on multi-tool-call");
    assert_eq!(
        invocation.payload.get("error_code"),
        Some(&Value::from("LLM_MULTI_TOOL_CALL"))
    );
}

#[tokio::test]
async fn e2e_unknown_tool_returns_typed_error() {
    // The mock picks `bogus_transition` which isn't declared in the
    // workflow's triaging state.
    let factory = Arc::new(MockProviderFactory::single(happy_with_tool(
        "bogus_transition",
    )));
    let (runtime, audit, _factory) = build_runtime(triage_workflow(), factory);

    let (wf_id, version) = start_workflow(&runtime, "triager").await;
    audit.clear();

    let resp = runtime
        .submit(submit(&wf_id, version, "advance"))
        .await
        .expect("runtime returns Ok(failed-response) on typed executor errors");
    assert_failed_with_llm_code(&resp, LlmErrorCode::UnknownTool);

    let snapshot = audit.snapshot();
    let invocation = snapshot
        .iter()
        .find(|e| e.event_type == "llm.invocation")
        .expect("llm.invocation fires on unknown-tool failure");
    assert_eq!(
        invocation.payload.get("error_code"),
        Some(&Value::from("LLM_UNKNOWN_TOOL"))
    );
}

#[tokio::test]
async fn e2e_malformed_arguments_returns_typed_error() {
    let factory = Arc::new(MockProviderFactory::single(
        MockProviderScenarios::malformed_arguments(),
    ));
    let (runtime, audit, _factory) = build_runtime(triage_workflow(), factory);

    let (wf_id, version) = start_workflow(&runtime, "triager").await;
    audit.clear();

    let resp = runtime
        .submit(submit(&wf_id, version, "advance"))
        .await
        .expect("runtime returns Ok(failed-response) on typed executor errors");
    assert_failed_with_llm_code(&resp, LlmErrorCode::MalformedArguments);

    let snapshot = audit.snapshot();
    let invocation = snapshot
        .iter()
        .find(|e| e.event_type == "llm.invocation")
        .expect("llm.invocation fires on malformed-args failure");
    assert_eq!(
        invocation.payload.get("error_code"),
        Some(&Value::from("LLM_MALFORMED_ARGUMENTS"))
    );
}

#[tokio::test]
async fn e2e_chain_advances_through_multiple_llm_states() {
    // Each state declares a transition named `next`; each LLM turn
    // picks `next` (the only option). The chain loop submits `next`
    // at the new state, drives the next LLM turn, and so on. The last
    // state's `next` is `kind: noop` so the chain breaks cleanly.
    let factory = Arc::new(MockProviderFactory::scripted(vec![
        happy_with_tool("next"), // LLM at A picks "next" → fires next, a→b
        happy_with_tool("next"), // LLM at B picks "next" → fires next, b→c
        happy_with_tool("next"), // LLM at C picks "next" → fires next, c→d
    ]));
    let (runtime, audit, factory_ref) = build_runtime(chain_workflow(), factory);

    let (wf_id, version) = start_workflow(&runtime, "chained").await;
    audit.clear();

    let resp = runtime
        .submit(submit(&wf_id, version, "next"))
        .await
        .expect("3-turn chain must succeed end to end");
    assert_eq!(
        resp["workflow"]["state"], "end",
        "chain must drive the workflow through three LLM states then \
         the noop terminator to `end`. Full response: {resp:?}"
    );

    // Factory observed three model requests, one per chained turn.
    let seen = factory_ref.models_seen();
    assert_eq!(
        seen.len(),
        3,
        "factory must be invoked once per chained LLM turn (3 turns). seen: {seen:?}"
    );
    for model in &seen {
        assert_eq!(model, "anthropic:claude-sonnet-4-6");
    }

    // Audit: three llm.invocation events (one per LLM turn).
    let snapshot = audit.snapshot();
    let invocation_count = snapshot
        .iter()
        .filter(|e| e.event_type == "llm.invocation")
        .count();
    assert_eq!(
        invocation_count, 3,
        "one llm.invocation per chained LLM turn"
    );

    // Four workflow.transitioned events fired total: three LLM turns
    // plus the noop terminator that broke the chain.
    let transitioned_count = snapshot
        .iter()
        .filter(|e| e.event_type == "workflow.transitioned")
        .count();
    assert_eq!(
        transitioned_count, 4,
        "three LLM transitions + one terminating noop transition"
    );
}

#[tokio::test]
async fn e2e_chain_depth_cap_exceeded_returns_chain_depth_exceeded() {
    // The mock yields `loop` every turn. With max_chained_llm_turns = 2
    // the chain runs 1 initial + 2 chained turns then refuses the 4th.
    let factory = Arc::new(MockProviderFactory::single(happy_with_tool("loop")));
    let (runtime, audit, _factory) = build_runtime(self_loop_workflow(), factory);
    let runtime = runtime.with_max_chained_llm_turns(2);

    let (wf_id, version) = start_workflow(&runtime, "looper").await;
    audit.clear();

    let err = runtime
        .submit(submit(&wf_id, version, "loop"))
        .await
        .expect_err("chain depth cap must surface an error");
    let exec_err = err
        .downcast_ref::<ExecutorError>()
        .expect("error must downcast to ExecutorError");
    match exec_err {
        ExecutorError::Llm(LlmErrorCode::ChainDepthExceeded, _) => {}
        other => panic!("expected ChainDepthExceeded, got {other:?}"),
    }

    let snapshot = audit.snapshot();
    let cap_event = snapshot.iter().find(|e| {
        e.event_type == "transition.rejected"
            && e.payload.get("code").and_then(Value::as_str) == Some("LLM_CHAIN_DEPTH_EXCEEDED")
    });
    assert!(
        cap_event.is_some(),
        "audit must record LLM_CHAIN_DEPTH_EXCEEDED rejection. Saw: {:?}",
        snapshot
            .iter()
            .map(|e| e.event_type.as_str())
            .collect::<Vec<_>>()
    );
}

#[tokio::test]
async fn e2e_cost_catalog_unknown_model_with_cap_fails_doctor() {
    // Workflow declares an unknown model under `max_cost_usd: 10.0` —
    // doctor must surface a COST_CATALOG_MISSING_ENTRY error.
    let registry = json!({
        "version": "1.0.0",
        "workflows": {
            "triager_with_unknown_model": {
                "initialState": "triaging",
                "states": {
                    "triaging": {
                        "transitions": {
                            "advance": {
                                "target": "done",
                                "actor": "agent",
                                "executor": {
                                    "kind": "llm",
                                    "model": "unknown:future-model",
                                    "prompt_template": "x",
                                    "max_cost_usd": 10.0
                                }
                            }
                        }
                    },
                    "done": { "terminal": true }
                }
            }
        }
    });
    let today = chrono::NaiveDate::from_ymd_opt(2026, 5, 29).expect("static date");
    let diags = doctor_check(&registry, today, None);
    let errors: Vec<&Diagnostic> = diags.iter().filter(|d| d.is_error()).collect();
    assert_eq!(
        errors.len(),
        1,
        "expected exactly one cost-catalog error, got {diags:?}"
    );
    assert!(
        errors[0].message().contains("COST_CATALOG_MISSING_ENTRY"),
        "doctor error must carry wire code: {}",
        errors[0].message()
    );
    assert!(
        errors[0].message().contains("unknown:future-model"),
        "doctor error must name the offending model: {}",
        errors[0].message()
    );
}

/// Bonus coverage — the audit on a successful turn records latency_ms
/// and the model name. Pins those fields against accidental rename.
#[tokio::test]
async fn e2e_audit_carries_latency_and_model_on_success() {
    let factory = Arc::new(MockProviderFactory::single(happy_with_tool("advance")));
    let (runtime, audit, _factory) = build_runtime(triage_workflow(), factory);

    let (wf_id, version) = start_workflow(&runtime, "triager").await;
    audit.clear();

    runtime
        .submit(submit(&wf_id, version, "advance"))
        .await
        .expect("happy path must succeed");

    let snapshot = audit.snapshot();
    let invocation = snapshot
        .iter()
        .find(|e| e.event_type == "llm.invocation")
        .expect("llm.invocation must fire");
    assert_eq!(
        invocation.payload.get("model"),
        Some(&Value::from("anthropic:claude-sonnet-4-6")),
        "audit must record the resolved model string"
    );
    assert!(
        invocation
            .payload
            .get("latency_ms")
            .and_then(Value::as_u64)
            .is_some(),
        "audit must include numeric latency_ms"
    );
}

/// SPEC §33 audit fixup (F3 STUB-004) — FMECA F1 consecutive-failure
/// counter actually increments across failed turns.
///
/// Pre-fix: validate() catching `LLM_NO_TOOL_CALL` short-circuited
/// execute() BEFORE `build_post_turn_slot_updates` ran, so the
/// `_llm.consecutive_no_tool_call` counter never ticked up and
/// apply_caps's cap (`>= max_iterations`) could never fire.
///
/// Post-fix: NoToolCall surfaces as `ExecutorError::LlmWithUpdates`
/// carrying the slot updates; the runtime merges them into next.context
/// and persists via save_if_version BEFORE recording the rejection.
/// The counter survives across failed turns and the third consecutive
/// failure trips the cap → `LLM_EXECUTION_EXHAUSTED`.
#[tokio::test]
async fn e2e_consecutive_no_tool_call_trips_execution_exhausted() {
    let wf = json!({
        "version": "1.0.0",
        "workflows": {
            "counter": {
                "version": "2026-05-29",
                "initialState": "thinking",
                "states": {
                    "thinking": {
                        "transitions": {
                            "advance": {
                                "target": "done",
                                "actor": "agent",
                                "executor": {
                                    "kind": "llm",
                                    "model": "anthropic:claude-sonnet-4-6",
                                    "prompt_template": "go",
                                    // F1: 2 consecutive no-tool-call turns are
                                    // allowed; the 3rd must trip the cap.
                                    "max_iterations": 2
                                },
                                // Author opt-in for the F1 counter — without
                                // this `output:` mapping the executor's slot
                                // updates aren't merged into next.context.
                                // F3 fixup: executor output uses NESTED form
                                // (`_llm.consecutive_no_tool_call` -> nested
                                // path) because the path resolver treats
                                // dots as separators (no bracket escape).
                                "output": {
                                    "_llm.consecutive_no_tool_call": "$.output._llm.consecutive_no_tool_call"
                                }
                            }
                        }
                    },
                    "done": { "terminal": true }
                }
            }
        }
    });

    // Single scenario, cloned each call — always returns NoToolCall.
    let factory = Arc::new(MockProviderFactory::single(
        MockProviderScenarios::no_tool_call(),
    ));
    let (runtime, audit, factory_clone) = build_runtime(wf, factory);

    let (wf_id, mut version) = start_workflow(&runtime, "counter").await;
    audit.clear();

    // Turn 1: counter starts at 0 (absent), turn fails with NoToolCall,
    // counter persists at 1, version bumps.
    let resp1 = runtime
        .submit(submit(&wf_id, version, "advance"))
        .await
        .expect("runtime surfaces typed executor errors as a failed-response");
    assert_failed_with_llm_code(&resp1, LlmErrorCode::NoToolCall);
    let new_version = resp1["workflow"]["version"]
        .as_u64()
        .expect("F3: LlmWithUpdates must bump version on the failed turn");
    assert_eq!(
        new_version,
        version + 1,
        "version must bump because slot_updates persisted via save_if_version"
    );
    // Persisted counter is exposed in the failed response's `context`.
    assert_eq!(
        resp1["context"]["_llm.consecutive_no_tool_call"],
        Value::from(1),
        "F1 counter must persist as 1 after turn 1's NoToolCall"
    );
    version = new_version;

    // Turn 2: counter=1 < cap=2 still passes apply_caps. Same shape.
    let resp2 = runtime
        .submit(submit(&wf_id, version, "advance"))
        .await
        .expect("turn 2 surfaces typed error");
    assert_failed_with_llm_code(&resp2, LlmErrorCode::NoToolCall);
    version = resp2["workflow"]["version"]
        .as_u64()
        .expect("version present");

    // Turn 3: pre-snapshot counter=2 >= max_iterations=2 → apply_caps
    // FAILS with ExecutionExhausted. The provider is never invoked
    // (factory's seen-models list shows only 2 prior model lookups).
    let resp3 = runtime
        .submit(submit(&wf_id, version, "advance"))
        .await
        .expect("turn 3 surfaces typed error");
    assert_failed_with_llm_code(&resp3, LlmErrorCode::ExecutionExhausted);

    // The mock factory was asked for the model exactly TWICE (turns 1
    // and 2). Turn 3's cap fires BEFORE the provider call, so the
    // factory wasn't consulted.
    assert_eq!(
        factory_clone.models_seen().len(),
        2,
        "F1 cap must fire before the provider call on turn 3"
    );

    // Audit must show three llm.invocation events: NoToolCall x2 then
    // ExecutionExhausted. The wire codes pin the operator-visible
    // contract.
    let snapshot = audit.snapshot();
    let invocations: Vec<_> = snapshot
        .iter()
        .filter(|e| e.event_type == "llm.invocation")
        .collect();
    assert_eq!(
        invocations.len(),
        3,
        "exactly 3 llm.invocation events (1 per submitted turn)"
    );
    assert_eq!(
        invocations[0].payload.get("error_code"),
        Some(&Value::from("LLM_NO_TOOL_CALL"))
    );
    assert_eq!(
        invocations[1].payload.get("error_code"),
        Some(&Value::from("LLM_NO_TOOL_CALL"))
    );
    assert_eq!(
        invocations[2].payload.get("error_code"),
        Some(&Value::from("LLM_EXECUTION_EXHAUSTED"))
    );
}

/// SPEC §33 audit fixup (F2 STUB-003) — FMECA F8 runtime gate.
///
/// `cost::doctor_check` validates models against the catalog at LOAD time,
/// but it only triggers on `max_cost_usd` (not `max_tokens`). A workflow
/// with `max_tokens: N` + an unknown model passes the load-time gate,
/// then at runtime the cost lookup fails. Pre-fix the executor would
/// log a `tracing::warn` and pass `cost_usd: null` to the audit while
/// the budget cap was active — silently bypassing F8. Post-fix the
/// executor must surface `LLM_USAGE_MISSING` so the reliability layer
/// classifies it and the workflow fails fast.
#[tokio::test]
async fn e2e_cost_catalog_miss_with_max_tokens_returns_usage_missing() {
    use praxec_llm_executor::stream_event::{StopReason, StreamEvent, TokenUsage, ToolCallRequest};

    // A workflow declaring a model that genuinely isn't in the cost
    // catalog (no anthropic/openai/etc. prefix in the catalog table).
    // `max_tokens` is set, but `max_cost_usd` is NOT — so the load-time
    // gate (`praxec check`) passes and the runtime gate becomes
    // the only thing protecting F8.
    let wf = json!({
        "version": "1.0.0",
        "workflows": {
            "uncataloged_with_tokens_cap": {
                "version": "2026-05-29",
                "initialState": "thinking",
                "states": {
                    "thinking": {
                        "transitions": {
                            "advance": {
                                "target": "done",
                                "actor": "agent",
                                "executor": {
                                    "kind": "llm",
                                    "model": "ollama:no-such-model",
                                    "prompt_template": "go",
                                    "max_tokens": 1000
                                }
                            }
                        }
                    },
                    "done": { "terminal": true }
                }
            }
        }
    });

    let scenario = MockScenario {
        name: "happy_for_f2_gate",
        events: vec![
            Ok(StreamEvent::ToolCall(ToolCallRequest {
                id: "c1".into(),
                name: "advance".into(),
                arguments: "{}".into(),
            })),
            // Usage IS present — the executor reaches the cost lookup,
            // which is where the F2 gate fires.
            Ok(StreamEvent::Usage(TokenUsage {
                input_tokens: 100,
                output_tokens: 50,
                reasoning_tokens: None,
            })),
            Ok(StreamEvent::Done {
                stop_reason: Some(StopReason::ToolCalls),
            }),
        ],
    };
    let factory = Arc::new(MockProviderFactory::single(scenario));
    let (runtime, audit, _factory) = build_runtime(wf, factory);

    let (wf_id, version) = start_workflow(&runtime, "uncataloged_with_tokens_cap").await;
    audit.clear();

    let resp = runtime
        .submit(submit(&wf_id, version, "advance"))
        .await
        .expect("runtime surfaces typed executor errors as a failed-response");
    assert_failed_with_llm_code(&resp, LlmErrorCode::UsageMissing);

    let snapshot = audit.snapshot();
    let invocation = snapshot
        .iter()
        .find(|e| e.event_type == "llm.invocation")
        .expect("llm.invocation must fire on the F8 runtime gate");
    assert_eq!(
        invocation.payload.get("error_code"),
        Some(&Value::from("LLM_USAGE_MISSING")),
        "F2 audit must carry the typed wire code"
    );
    assert_eq!(
        invocation.payload.get("cost_usd"),
        Some(&Value::Null),
        "cost_usd must be null when the catalog missed (not 0 or guessed)"
    );
    assert_eq!(
        invocation.payload.get("tool_call_emitted"),
        Some(&Value::Null),
        "no tool call on F2 failure"
    );
}

// ============================================================================
// CMP-009 — audit-sink failure on a SUCCESSFUL (billed) turn must be
// classified as RETRYABLE, not a Permanent discard.
// ============================================================================

/// Audit sink that always fails `record`, simulating a transient
/// audit-sink blip (network/disk).
struct AlwaysFailingAuditSink;

#[async_trait::async_trait]
impl AuditSink for AlwaysFailingAuditSink {
    async fn record(&self, _event: praxec_core::audit::AuditEvent) -> anyhow::Result<()> {
        anyhow::bail!("simulated audit-sink outage")
    }
}

/// Resolver that returns exactly one `advance` transition, matching the
/// `happy_path` mock scenario's tool call.
struct SingleAdvanceResolver;

#[async_trait::async_trait]
impl TransitionResolver for SingleAdvanceResolver {
    async fn available_transitions(
        &self,
        _instance: &praxec_core::model::WorkflowInstance,
        _principal: &Principal,
    ) -> anyhow::Result<Vec<Value>> {
        Ok(vec![json!({ "rel": "advance" })])
    }
}

/// CMP-009: when the turn SUCCEEDS (model picked a valid tool, was
/// billed) but the audit sink fails, the executor must surface a
/// RETRYABLE error so the reliability layer re-runs and re-records the
/// turn — NOT a Permanent error that silently discards a billed turn.
#[tokio::test]
async fn audit_failure_on_successful_turn_is_retryable() {
    use praxec_core::error::ErrorClass;

    let audit: Arc<dyn AuditSink> = Arc::new(AlwaysFailingAuditSink);
    let resolver: Arc<dyn TransitionResolver> = Arc::new(SingleAdvanceResolver);
    let factory: Arc<dyn ProviderFactory> = Arc::new(MockProviderFactory::single(
        MockProviderScenarios::happy_path(),
    ));

    let exec = LlmExecutor::with_provider_factory(audit, resolver, factory);

    let request = praxec_core::model::ExecuteRequest {
        workflow: praxec_core::model::WorkflowInstance {
            id: "wf_cmp009".into(),
            definition_id: "demo".into(),
            definition_version: "1.0.0".into(),
            definition: json!({"initialState": "triaging", "states": {}}),
            state: "triaging".into(),
            version: 0,
            input: json!({}),
            context: json!({}),
            started_at: chrono::Utc::now(),
            trace_id: None,
            run_id: None,
            cancelled_at: None,
            cancelled_reason: None,
            depth: 0,
            parent: None,
        },
        transition: None,
        arguments: json!({}),
        executor_config: json!({
            "kind": "llm",
            "model": "anthropic:claude-sonnet-4-6",
            "prompt_template": "go"
        }),
        idempotency_key: None,
        correlation_id: None,
    };

    let err = exec
        .execute(request)
        .await
        .expect_err("audit failure on a successful turn must surface an error");

    // The key assertion: the error must be RETRYABLE, not Permanent —
    // otherwise a transient audit blip permanently discards a billed turn.
    let class = err.class();
    assert_ne!(
        class,
        ErrorClass::Permanent,
        "CMP-009: audit failure on a billed success must be retryable, got {class:?}: {err}"
    );
    assert_eq!(
        class,
        ErrorClass::Connection,
        "CMP-009: expected Connection class so the reliability layer retries"
    );
}
