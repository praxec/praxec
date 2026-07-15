//! (1b) Auto-drive of skill-surfacing `actor: agent` states.
//!
//! Atomic behavioral assertions — one per test — for the ENGINE logic, using a
//! mock executor in place of the real (flaky) in-process agent runtime. These
//! prove the auto-drive composition is correct deterministically; any hang in a
//! live run is therefore in the agent runtime, not this engine path.

mod common;
use common::chain::*;

use praxec_core::model::{Principal, StartWorkflow};
use serde_json::json;

/// Baseline: with auto-drive OFF (default), the chain stops at the agent state.
#[tokio::test]
async fn auto_drive_off_stops_at_agent_state() {
    let (runtime, _exec, _audit) = build_runtime(linear_chain_stops_at_agent());
    let resp = runtime
        .start(StartWorkflow {
            definition_id: "pipeline".into(),
            input: json!({}),
            principal: Principal::anonymous(),
            run_env: praxec_core::RunEnv::for_test(),
            depth: 0,
            parent: None,
        })
        .await
        .unwrap();
    assert_eq!(resp["workflow"]["state"], "b");
}

/// With auto-drive ON, the chain auto-drives the lone `actor: agent` transition
/// (invoking the agent executor) and proceeds to the terminal state.
#[tokio::test]
async fn auto_drive_on_advances_through_agent_state() {
    let exec = std::sync::Arc::new(FixedExecutor::new(json!({})));
    let (runtime, _audit) = build_runtime_with_executor(
        linear_chain_stops_at_agent(),
        exec.clone() as std::sync::Arc<dyn praxec_core::ports::Executor>,
    );
    let runtime = runtime.with_auto_drive_agents(true, "reasoning", vec![], 180);
    let resp = runtime
        .start(StartWorkflow {
            definition_id: "pipeline".into(),
            input: json!({}),
            principal: Principal::anonymous(),
            run_env: praxec_core::RunEnv::for_test(),
            depth: 0,
            parent: None,
        })
        .await
        .unwrap();
    assert_eq!(resp["workflow"]["state"], "c");
}

/// The composed agent step must instruct the model to call `final_answer`
/// (matching the runner's result contract) — NOT "return JSON text, no prose",
/// which made the model skip the tool and yield AGENT_NO_RESULT — and must pass
/// the capability's required keys through as `expected_output_keys`.
#[tokio::test]
async fn auto_drive_composes_the_final_answer_contract_and_expected_keys() {
    let cfg = json!({
        "version": "1.0.0",
        "workflows": {
            "pipeline": {
                "initialState": "s",
                "states": {
                    "s": {
                        "goal": "Produce a verdict.",
                        "transitions": {
                            "submit": {
                                "target": "done",
                                "actor": "agent",
                                "executor": { "kind": "noop" },
                                "inputSchema": { "required": ["verdict", "rationale"] },
                                "output": { "verdict": "$.arguments.verdict" }
                            }
                        }
                    },
                    "done": { "terminal": true }
                }
            }
        }
    });
    let exec = std::sync::Arc::new(CapturingExecutor::new(json!({ "verdict": "pass" })));
    let (runtime, _audit) = build_runtime_with_executor(
        cfg,
        exec.clone() as std::sync::Arc<dyn praxec_core::ports::Executor>,
    );
    let runtime = runtime.with_auto_drive_agents(true, "reasoning", vec![], 180);
    runtime
        .start(StartWorkflow {
            definition_id: "pipeline".into(),
            input: json!({}),
            principal: Principal::anonymous(),
            run_env: praxec_core::RunEnv::for_test(),
            depth: 0,
            parent: None,
        })
        .await
        .unwrap();

    let config = exec
        .config_for_kind("agent")
        .expect("the agent executor was invoked");
    let goal = config["goal"].as_str().expect("goal is a string");
    assert!(
        goal.contains("final_answer"),
        "goal must instruct calling final_answer, got: {goal}"
    );
    assert!(
        !goal.contains("No prose") && !goal.contains("Return ONLY"),
        "goal must not tell the model to answer in JSON text, got: {goal}"
    );
    assert_eq!(
        config["expected_output_keys"],
        json!(["verdict", "rationale"]),
        "the capability's required keys must flow to the runner as the criteria"
    );
}

/// The auto-driven agent's structured output is fed as the transition's
/// `arguments`, so the cap's existing `$.arguments.*` output mapping applies.
#[tokio::test]
async fn auto_drive_feeds_agent_output_as_arguments() {
    let cfg = json!({
        "version": "1.0.0",
        "workflows": {
            "pipeline": {
                "initialState": "s",
                "states": {
                    "s": {
                        "goal": "Produce a verdict.",
                        "transitions": {
                            "submit": {
                                "target": "done",
                                "actor": "agent",
                                "executor": { "kind": "noop" },
                                "output": { "verdict": "$.arguments.verdict" }
                            }
                        }
                    },
                    "done": { "terminal": true }
                }
            }
        }
    });
    let exec = std::sync::Arc::new(FixedExecutor::new(json!({ "verdict": "pass" })));
    let (runtime, _audit) = build_runtime_with_executor(
        cfg,
        exec.clone() as std::sync::Arc<dyn praxec_core::ports::Executor>,
    );
    let runtime = runtime.with_auto_drive_agents(true, "reasoning", vec![], 180);
    let resp = runtime
        .start(StartWorkflow {
            definition_id: "pipeline".into(),
            input: json!({}),
            principal: Principal::anonymous(),
            run_env: praxec_core::RunEnv::for_test(),
            depth: 0,
            parent: None,
        })
        .await
        .unwrap();
    assert_eq!(resp["workflow"]["state"], "done");
    assert_eq!(resp["context"]["verdict"], "pass");
}

/// Per-call cost telemetry: when the auto-driven agent executor returns
/// `ExecutorTelemetry`, the runtime folds `model` / `prompt_tokens` /
/// `completion_tokens` / `cost_usd` into the `agent.completed` audit event so
/// every governed run is cost-attributable.
#[tokio::test]
async fn agent_completed_carries_cost_telemetry() {
    use praxec_core::model::ExecutorTelemetry;

    let exec = std::sync::Arc::new(common::chain::TelemetryExecutor::new(
        json!({}),
        ExecutorTelemetry {
            model: "openrouter:z-ai/glm-5.2".into(),
            prompt_tokens: 1200,
            completion_tokens: 340,
            cost_usd: Some(0.123),
        },
    ));
    let (runtime, audit) = build_runtime_with_executor(
        linear_chain_stops_at_agent(),
        exec.clone() as std::sync::Arc<dyn praxec_core::ports::Executor>,
    );
    let runtime = runtime.with_auto_drive_agents(true, "reasoning", vec![], 180);
    runtime
        .start(StartWorkflow {
            definition_id: "pipeline".into(),
            input: json!({}),
            principal: Principal::anonymous(),
            run_env: praxec_core::RunEnv::for_test(),
            depth: 0,
            parent: None,
        })
        .await
        .unwrap();

    let completed = audit
        .snapshot()
        .into_iter()
        .find(|e| e.event_type == "agent.completed")
        .expect("an agent.completed event must be recorded");
    let p = &completed.payload;
    assert_eq!(p["model"], "openrouter:z-ai/glm-5.2");
    assert_eq!(p["prompt_tokens"], 1200);
    assert_eq!(p["completion_tokens"], 340);
    assert_eq!(p["cost_usd"], 0.123);
    // duration_ms is preserved alongside the new fields.
    assert!(p["duration_ms"].is_u64());
    // The affinity the agent was resolved under rides along, so the cost
    // report can attribute spend to the kind of work without a join.
    assert_eq!(p["affinity"], "reasoning");
}

/// Degrade gracefully: an uncatalogued model leaves `cost_usd: null` on the
/// audit event (mirrors the llm-executor's degrade-to-None) — never an error,
/// and the tokens are still recorded.
#[tokio::test]
async fn agent_completed_cost_is_null_for_uncatalogued_model() {
    use praxec_core::model::ExecutorTelemetry;

    let exec = std::sync::Arc::new(common::chain::TelemetryExecutor::new(
        json!({}),
        ExecutorTelemetry {
            model: "vendor:not-in-catalog".into(),
            prompt_tokens: 10,
            completion_tokens: 5,
            cost_usd: None,
        },
    ));
    let (runtime, audit) = build_runtime_with_executor(
        linear_chain_stops_at_agent(),
        exec.clone() as std::sync::Arc<dyn praxec_core::ports::Executor>,
    );
    let runtime = runtime.with_auto_drive_agents(true, "reasoning", vec![], 180);
    runtime
        .start(StartWorkflow {
            definition_id: "pipeline".into(),
            input: json!({}),
            principal: Principal::anonymous(),
            run_env: praxec_core::RunEnv::for_test(),
            depth: 0,
            parent: None,
        })
        .await
        .unwrap();

    let completed = audit
        .snapshot()
        .into_iter()
        .find(|e| e.event_type == "agent.completed")
        .expect("an agent.completed event must be recorded");
    assert_eq!(completed.payload["cost_usd"], serde_json::Value::Null);
    assert_eq!(completed.payload["completion_tokens"], 5);
}
