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

// ---------------------------------------------------------------------------
// Per-state tool scoping (role separation).
//
// `auto_drive_tools` is a GATEWAY-WIDE set handed to every auto-driven leaf, so
// an exploring agent and a fixing agent see the same toolbelt. That defeats role
// separation: the whole point of a promotion loop is that the fixer CANNOT reach
// the approved test. A state may therefore declare its own `tools:`, which
// REPLACES the global set for that leaf — mirroring the per-state `affinity:`
// override directly above it in the composer.
// ---------------------------------------------------------------------------

/// A definition whose single agent state declares `tools` (verbatim value under
/// the state key, so a test can supply an empty array or a non-array too).
fn agent_state_with_tools(tools: serde_json::Value) -> serde_json::Value {
    let mut state = json!({
        "goal": "Explore.",
        "transitions": {
            "submit": {
                "target": "done",
                "actor": "agent",
                "executor": { "kind": "noop" },
                "output": { "verdict": "$.arguments.verdict" }
            }
        }
    });
    state["tools"] = tools;
    json!({
        "version": "1.0.0",
        "workflows": {
            "pipeline": {
                "initialState": "s",
                "states": { "s": state, "done": { "terminal": true } }
            }
        }
    })
}

fn agent_state_with_reasoning_effort(effort: serde_json::Value) -> serde_json::Value {
    let mut state = json!({
        "goal": "Diagnose.",
        "transitions": {
            "submit": {
                "target": "done",
                "actor": "agent",
                "executor": { "kind": "noop" },
                "output": { "verdict": "$.arguments.verdict" }
            }
        }
    });
    state["reasoning_effort"] = effort;
    json!({
        "version": "1.0.0",
        "workflows": {
            "pipeline": {
                "initialState": "s",
                "states": { "s": state, "done": { "terminal": true } }
            }
        }
    })
}

/// The motivating case: a diagnosis state is the hardest reasoning step in a
/// loop, so it must be able to raise its OWN effort. A state's declared
/// `reasoning_effort:` wins over the context/input-derived override, exactly as
/// a state's `affinity:` wins over `affinity_override`.
#[tokio::test]
async fn state_reasoning_effort_overrides_the_context_derived_effort() {
    let exec = std::sync::Arc::new(CapturingExecutor::new(json!({ "verdict": "pass" })));
    let (runtime, _audit) = build_runtime_with_executor(
        agent_state_with_reasoning_effort(json!("xhigh")),
        exec.clone() as std::sync::Arc<dyn praxec_core::ports::Executor>,
    );
    let runtime = runtime.with_auto_drive_agents(true, "reasoning", vec!["github_mcp".into()], 180);
    runtime
        .start(StartWorkflow {
            definition_id: "pipeline".into(),
            // The loop-level override says "minimal"; the state says "xhigh".
            input: json!({ "effort_override": "minimal" }),
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
    assert_eq!(
        config["reasoning_effort"], "xhigh",
        "the state's own declaration must win over the context-derived effort"
    );
}

/// The regression fence. An absent `reasoning_effort:` must be BIT-IDENTICAL to
/// today: the context/input-derived effort applies, and when there is none the
/// key is omitted entirely so the `kind: agent` config (deny_unknown_fields,
/// `reasoning_effort: Option`) stays exactly as it was.
#[tokio::test]
async fn state_without_reasoning_effort_is_bit_identical() {
    // (a) no declaration anywhere → the key is absent from the composed config.
    let exec = std::sync::Arc::new(CapturingExecutor::new(json!({ "verdict": "pass" })));
    let (runtime, _audit) = build_runtime_with_executor(
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
    let config = exec
        .config_for_kind("agent")
        .expect("the agent executor was invoked");
    assert!(
        config.get("reasoning_effort").is_none(),
        "no declaration and no override must leave the key absent, got: {config}"
    );

    // (b) no state declaration but a context override → the override still wins,
    // proving the new branch did not shadow the existing path.
    let exec2 = std::sync::Arc::new(CapturingExecutor::new(json!({ "verdict": "pass" })));
    let (runtime2, _audit2) = build_runtime_with_executor(
        linear_chain_stops_at_agent(),
        exec2.clone() as std::sync::Arc<dyn praxec_core::ports::Executor>,
    );
    let runtime2 = runtime2.with_auto_drive_agents(true, "reasoning", vec![], 180);
    runtime2
        .start(StartWorkflow {
            definition_id: "pipeline".into(),
            input: json!({ "effort_override": "high" }),
            principal: Principal::anonymous(),
            run_env: praxec_core::RunEnv::for_test(),
            depth: 0,
            parent: None,
        })
        .await
        .unwrap();
    assert_eq!(
        exec2
            .config_for_kind("agent")
            .expect("invoked")
            .get("reasoning_effort"),
        Some(&json!("high")),
        "an absent state declaration must leave the context-derived path untouched"
    );
}

/// A typo must NOT silently become the default. The `ReasoningTuning` accessors
/// fall back to `medium` for an unknown level, so an unvalidated `xhig` would
/// quietly become a no-op cap and the author would never learn — the exact
/// silent-degrade class this engine exists to remove.
#[tokio::test]
async fn invalid_state_reasoning_effort_is_rejected_not_defaulted() {
    let exec = std::sync::Arc::new(CapturingExecutor::new(json!({ "verdict": "pass" })));
    let (runtime, _audit) = build_runtime_with_executor(
        agent_state_with_reasoning_effort(json!("xhig")),
        exec.clone() as std::sync::Arc<dyn praxec_core::ports::Executor>,
    );
    let runtime = runtime.with_auto_drive_agents(true, "reasoning", vec!["github_mcp".into()], 180);
    let err = runtime
        .start(StartWorkflow {
            definition_id: "pipeline".into(),
            input: json!({}),
            principal: Principal::anonymous(),
            run_env: praxec_core::RunEnv::for_test(),
            depth: 0,
            parent: None,
        })
        .await
        .expect_err("an unknown effort level must fail the run, not default");
    let msg = err.to_string();
    assert!(
        msg.contains("AUTO_DRIVE_STATE_REASONING_EFFORT_INVALID"),
        "error must name the typed code so an author can find it, got: {msg}"
    );
    assert!(
        msg.contains("xhigh"),
        "error must name the legal values, got: {msg}"
    );
    assert!(
        exec.config_for_kind("agent").is_none(),
        "the agent must NOT be dispatched with an unresolvable effort"
    );
}

/// The same authoring mistake in a different shape (wrong JSON type) must fail
/// identically — never fall through to the default, which would silently grant
/// a different thinking budget than the author asked for.
#[tokio::test]
async fn non_string_state_reasoning_effort_is_rejected() {
    let exec = std::sync::Arc::new(CapturingExecutor::new(json!({ "verdict": "pass" })));
    let (runtime, _audit) = build_runtime_with_executor(
        agent_state_with_reasoning_effort(json!(3)),
        exec.clone() as std::sync::Arc<dyn praxec_core::ports::Executor>,
    );
    let runtime = runtime.with_auto_drive_agents(true, "reasoning", vec!["github_mcp".into()], 180);
    let err = runtime
        .start(StartWorkflow {
            definition_id: "pipeline".into(),
            input: json!({}),
            principal: Principal::anonymous(),
            run_env: praxec_core::RunEnv::for_test(),
            depth: 0,
            parent: None,
        })
        .await
        .expect_err("a non-string `reasoning_effort:` must fail the run");
    assert!(
        err.to_string()
            .contains("AUTO_DRIVE_STATE_REASONING_EFFORT_INVALID")
    );
}

/// `medium` is accepted DELIBERATELY: it is a real key of the shipped tuning
/// maps and means "provider default — do not cap this step", which is a
/// meaningful declaration even though `reasoning_params` emits nothing for it.
/// Accepting it by rule, not by accident.
#[tokio::test]
async fn medium_state_reasoning_effort_is_accepted_as_an_explicit_provider_default() {
    let exec = std::sync::Arc::new(CapturingExecutor::new(json!({ "verdict": "pass" })));
    let (runtime, _audit) = build_runtime_with_executor(
        agent_state_with_reasoning_effort(json!("medium")),
        exec.clone() as std::sync::Arc<dyn praxec_core::ports::Executor>,
    );
    let runtime = runtime.with_auto_drive_agents(true, "reasoning", vec!["github_mcp".into()], 180);
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
        .expect("`medium` is a legal declared level");
    assert_eq!(
        exec.config_for_kind("agent").expect("invoked")["reasoning_effort"],
        "medium"
    );
}

/// Observability must not lie about thinking budget either: the `agent.invoked`
/// event records the EFFECTIVE effort, for the same reason it records the
/// effective tool set — an audit of a raised-effort step must not report the
/// gateway default.
#[tokio::test]
async fn agent_invoked_audit_records_the_effective_reasoning_effort() {
    let exec = std::sync::Arc::new(CapturingExecutor::new(json!({ "verdict": "pass" })));
    let (runtime, audit) = build_runtime_with_executor(
        agent_state_with_reasoning_effort(json!("xhigh")),
        exec.clone() as std::sync::Arc<dyn praxec_core::ports::Executor>,
    );
    let runtime = runtime.with_auto_drive_agents(true, "reasoning", vec!["github_mcp".into()], 180);
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
    let invoked = audit
        .snapshot()
        .into_iter()
        .find(|e| e.event_type == "agent.invoked")
        .expect("an agent.invoked event must be recorded");
    assert_eq!(
        invoked.payload["reasoning_effort"], "xhigh",
        "the audit must record the effort the leaf actually ran with"
    );
}

/// A state's `tools:` REPLACES the gateway-wide auto-drive set, so an exploring
/// leaf can be given browser access without also handing it the filesystem.
#[tokio::test]
async fn state_tools_replace_the_global_auto_drive_set() {
    let exec = std::sync::Arc::new(CapturingExecutor::new(json!({ "verdict": "pass" })));
    let (runtime, _audit) = build_runtime_with_executor(
        agent_state_with_tools(json!(["browser_chrome_1"])),
        exec.clone() as std::sync::Arc<dyn praxec_core::ports::Executor>,
    );
    let runtime = runtime.with_auto_drive_agents(
        true,
        "reasoning",
        vec![
            "browser_chrome_1".into(),
            "github_mcp".into(),
            "file:/repo".into(),
        ],
        180,
    );
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
    assert_eq!(
        config["tools"],
        json!(["browser_chrome_1"]),
        "the state's `tools:` must REPLACE the global set, not merge with it"
    );
}

/// Regression fence: a state WITHOUT `tools:` inherits the gateway-wide set
/// exactly as before. This must pass on the pre-change tree — it is what makes
/// the feature additive and every shipped pack behave bit-identically.
#[tokio::test]
async fn state_without_tools_inherits_the_global_set() {
    let exec = std::sync::Arc::new(CapturingExecutor::new(json!({ "verdict": "pass" })));
    let (runtime, _audit) = build_runtime_with_executor(
        linear_chain_stops_at_agent(),
        exec.clone() as std::sync::Arc<dyn praxec_core::ports::Executor>,
    );
    let runtime = runtime.with_auto_drive_agents(
        true,
        "reasoning",
        vec!["github_mcp".into(), "file:/repo".into()],
        180,
    );
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
    assert_eq!(
        config["tools"],
        json!(["github_mcp", "file:/repo"]),
        "absent `tools:` must inherit the global auto-drive set unchanged"
    );
}

/// `tools: []` is an AUTHORING ERROR, not "no tools". A leaf handed an empty
/// toolbelt cannot act, so it burns its entire step budget producing nothing —
/// the failure class that once made the whole meta pack unusable. Fail fast at
/// composition instead, naming the fix.
#[tokio::test]
async fn empty_state_tools_is_rejected_not_treated_as_no_tools() {
    let exec = std::sync::Arc::new(CapturingExecutor::new(json!({ "verdict": "pass" })));
    let (runtime, _audit) = build_runtime_with_executor(
        agent_state_with_tools(json!([])),
        exec.clone() as std::sync::Arc<dyn praxec_core::ports::Executor>,
    );
    let runtime = runtime.with_auto_drive_agents(true, "reasoning", vec!["github_mcp".into()], 180);
    let err = runtime
        .start(StartWorkflow {
            definition_id: "pipeline".into(),
            input: json!({}),
            principal: Principal::anonymous(),
            run_env: praxec_core::RunEnv::for_test(),
            depth: 0,
            parent: None,
        })
        .await
        .expect_err("an empty `tools:` must fail the run, not silently disarm the agent");
    let msg = err.to_string();
    assert!(
        msg.contains("AUTO_DRIVE_STATE_TOOLS_INVALID"),
        "error must name the typed code so an author can find it, got: {msg}"
    );
    assert!(
        exec.config_for_kind("agent").is_none(),
        "the agent must NOT be dispatched with an empty toolbelt"
    );
}

/// A non-array `tools:` is the same authoring mistake in a different shape and
/// must fail identically — never fall through to the global set, which would
/// silently grant more reach than the author asked for.
#[tokio::test]
async fn non_array_state_tools_is_rejected() {
    let exec = std::sync::Arc::new(CapturingExecutor::new(json!({ "verdict": "pass" })));
    let (runtime, _audit) = build_runtime_with_executor(
        agent_state_with_tools(json!("browser_chrome_1")),
        exec.clone() as std::sync::Arc<dyn praxec_core::ports::Executor>,
    );
    let runtime = runtime.with_auto_drive_agents(true, "reasoning", vec!["github_mcp".into()], 180);
    let err = runtime
        .start(StartWorkflow {
            definition_id: "pipeline".into(),
            input: json!({}),
            principal: Principal::anonymous(),
            run_env: praxec_core::RunEnv::for_test(),
            depth: 0,
            parent: None,
        })
        .await
        .expect_err("a scalar `tools:` must fail the run");
    assert!(err.to_string().contains("AUTO_DRIVE_STATE_TOOLS_INVALID"));
}

/// Observability must not lie about reach: the `agent.invoked` audit event
/// records the EFFECTIVE tool set, not the gateway-wide one. Otherwise an audit
/// of a scoped run reports tools the leaf never had.
#[tokio::test]
async fn agent_invoked_audit_records_the_effective_tool_set() {
    let exec = std::sync::Arc::new(CapturingExecutor::new(json!({ "verdict": "pass" })));
    let (runtime, audit) = build_runtime_with_executor(
        agent_state_with_tools(json!(["browser_chrome_1"])),
        exec.clone() as std::sync::Arc<dyn praxec_core::ports::Executor>,
    );
    let runtime = runtime.with_auto_drive_agents(
        true,
        "reasoning",
        vec!["github_mcp".into(), "file:/repo".into()],
        180,
    );
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

    let invoked = audit
        .snapshot()
        .into_iter()
        .find(|e| e.event_type == "agent.invoked")
        .expect("an agent.invoked event must be recorded");
    assert_eq!(
        invoked.payload["tools"],
        json!(["browser_chrome_1"]),
        "the audit payload must carry the effective set, or observability lies"
    );
}

// ---------------------------------------------------------------------------
// The repo write-exclusion gate on the AUTO-DRIVE path.
//
// `owned_files` is honored in `dispatch_once` (the submit path) only. The
// deterministic chain synthesizes its own agent config and calls
// `execute_with_reliability` directly, so an auto-driven leaf declaring
// `owned_files` took NO lock at all — the declaration was silently inert.
//
// That matters beyond repo files: a shared, stateful resource (a browser whose
// `select_page` pointer is global to its server process) is exactly what
// `owned_files` exists to serialize. A lease that is never taken is worse than
// no lease, because the declaration reads as protection.
// ---------------------------------------------------------------------------

/// An auto-drivable agent transition whose declared executor owns a file.
fn auto_drivable_owning(file: &str) -> serde_json::Value {
    json!({
        "version": "1.0.0",
        "workflows": {
            "pipeline": {
                "initialState": "s",
                "states": {
                    "s": {
                        "goal": "Explore.",
                        "transitions": {
                            "submit": {
                                "target": "done",
                                "actor": "agent",
                                "executor": { "kind": "noop", "owned_files": [file] },
                                "output": { "verdict": "$.arguments.verdict" }
                            }
                        }
                    },
                    "done": { "terminal": true }
                }
            }
        }
    })
}

/// The auto-drive path must ACQUIRE the transition's declared `owned_files`
/// before invoking the agent — the same gate the submit path applies.
#[tokio::test]
async fn auto_driven_leaf_with_owned_files_acquires_the_lock() {
    use praxec_core::repo_locks::{RepoLockSpace, RepoLocks};
    use std::sync::Arc;

    let exec = std::sync::Arc::new(CapturingExecutor::new(json!({ "verdict": "pass" })));
    let (runtime, audit) = build_runtime_with_executor(
        auto_drivable_owning("browser/slot-1"),
        exec.clone() as std::sync::Arc<dyn praxec_core::ports::Executor>,
    );
    let locks: Arc<dyn RepoLocks> = Arc::new(RepoLockSpace::new());
    let runtime = runtime
        .with_auto_drive_agents(true, "reasoning", vec![], 180)
        .with_repo_locks(locks);

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

    let events = audit.snapshot();
    assert!(
        events.iter().any(|e| e.event_type == "lock.acquired"),
        "the auto-drive path must acquire declared owned_files, or the lease is inert"
    );
    assert!(
        events.iter().any(|e| e.event_type == "lock.released"),
        "and release them once the leaf returns"
    );
}

/// The safety property: when the declared file is already held by ANOTHER
/// workflow, the auto-driven agent must NOT run. Executing anyway is the silent
/// corruption this gate exists to prevent — two runs driving one shared
/// resource, each believing it holds it exclusively.
#[tokio::test]
async fn auto_driven_leaf_blocked_by_a_foreign_holder_does_not_execute() {
    use praxec_core::repo_locks::{RepoLockSpace, RepoLocks};
    use std::sync::Arc;
    use std::time::Duration;

    let exec = std::sync::Arc::new(CapturingExecutor::new(json!({ "verdict": "pass" })));
    let (runtime, _audit) = build_runtime_with_executor(
        auto_drivable_owning("browser/slot-1"),
        exec.clone() as std::sync::Arc<dyn praxec_core::ports::Executor>,
    );
    let locks: Arc<dyn RepoLocks> = Arc::new(RepoLockSpace::new());
    locks
        .acquire(
            &[std::path::PathBuf::from("browser/slot-1")],
            "wf:someone_else",
            Duration::from_secs(300),
        )
        .await
        .unwrap();

    let runtime = runtime
        .with_auto_drive_agents(true, "reasoning", vec![], 180)
        .with_repo_locks(locks);

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

    assert!(
        exec.config_for_kind("agent").is_none(),
        "the agent must NOT be dispatched while another holder owns the declared file"
    );
}

/// A leaf that declares no `owned_files` takes no lock — the gate must not
/// serialize ordinary agent work. (Regression fence for the hoist.)
#[tokio::test]
async fn auto_driven_leaf_without_owned_files_takes_no_lock() {
    use praxec_core::repo_locks::{RepoLockSpace, RepoLocks};
    use std::sync::Arc;

    let exec = std::sync::Arc::new(CapturingExecutor::new(json!({ "verdict": "pass" })));
    let (runtime, audit) = build_runtime_with_executor(
        linear_chain_stops_at_agent(),
        exec.clone() as std::sync::Arc<dyn praxec_core::ports::Executor>,
    );
    let locks: Arc<dyn RepoLocks> = Arc::new(RepoLockSpace::new());
    let runtime = runtime
        .with_auto_drive_agents(true, "reasoning", vec![], 180)
        .with_repo_locks(locks);

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

    assert!(
        !audit
            .snapshot()
            .iter()
            .any(|e| e.event_type == "lock.acquired"),
        "an undeclared leaf must not take a lock — the gate is opt-in via owned_files"
    );
    assert!(exec.config_for_kind("agent").is_some(), "and it still runs");
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
