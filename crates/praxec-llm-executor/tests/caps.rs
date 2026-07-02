//! SPEC §33 D6 — integration coverage for the cumulative-cap path
//! and the synthetic-slot post-turn helper.
//!
//! The unit tests inside `src/caps.rs` pin a couple of obvious edges;
//! this file pins the full public contract, including the cases the
//! `apply_caps` and `build_post_turn_slot_updates` paths only ever see
//! together (a populated context plus a non-zero token usage).
//!
//! No real provider calls — every helper here is pure.

use chrono::{Duration, Utc};
use praxec_core::error::{ExecutorError, LlmErrorCode};
use praxec_core::model::WorkflowInstance;
use praxec_llm_executor::caps::{
    apply_caps, build_post_turn_slot_updates, read_snapshot, session_started_at_key, SlotSnapshot,
    RESERVED_LLM_PREFIX,
};
use praxec_llm_executor::config::LlmExecutorConfig;
use praxec_llm_executor::response::DrainedResponse;
use praxec_llm_executor::stream_event::{StopReason, TokenUsage, ToolCallRequest};
use serde_json::{json, Value};

fn cfg() -> LlmExecutorConfig {
    LlmExecutorConfig {
        model: Some("openai:gpt-5".into()),
        affinity: None,
        needs: vec![],
        prompt_template: "x".into(),
        max_iterations: 3,
        max_seconds: None,
        max_tokens: None,
        max_cost_usd: None,
        reasoning_effort: None,
        capture_reasoning: true,
    }
}

fn make_instance(context: Value) -> WorkflowInstance {
    WorkflowInstance {
        id: "wf_caps".into(),
        definition_id: "demo".into(),
        definition_version: "1.0.0".into(),
        definition: json!({"initialState": "thinking", "states": {}}),
        state: "thinking".into(),
        version: 0,
        input: json!({}),
        context,
        started_at: Utc::now(),
        trace_id: None,
        run_id: None,
        cancelled_at: None,
        cancelled_reason: None,
        depth: 0,
        parent: None,
    }
}

fn drained_with_tool(input: u64, output: u64) -> DrainedResponse {
    DrainedResponse {
        saw_done: true,
        tool_calls: vec![ToolCallRequest {
            id: "c1".into(),
            name: "advance".into(),
            arguments: "{}".into(),
        }],
        usage: Some(TokenUsage {
            input_tokens: input,
            output_tokens: output,
            reasoning_tokens: None,
        }),
        stop_reason: Some(StopReason::ToolCalls),
        ..Default::default()
    }
}

// ── apply_caps ─────────────────────────────────────────────────────────────

#[test]
fn apply_cumulative_caps_passes_with_empty_context() {
    let snap = read_snapshot(&make_instance(json!({})), "thinking");
    apply_caps(&snap, &cfg(), Utc::now()).expect("first turn ever must pass");
}

#[test]
fn apply_cumulative_caps_passes_with_counters_below_caps() {
    let snap = read_snapshot(
        &make_instance(json!({
            "_llm.cumulative_tokens": 500,
            "_llm.cumulative_cost_usd": 0.01,
            "_llm.cumulative_iterations": 2,
            "_llm.consecutive_no_tool_call": 1,
        })),
        "thinking",
    );
    let mut c = cfg();
    c.max_tokens = Some(10_000);
    c.max_cost_usd = Some(1.0);
    apply_caps(&snap, &c, Utc::now()).expect("counters below caps must pass");
}

#[test]
fn apply_cumulative_caps_rejects_when_consecutive_no_tool_call_at_max_iterations() {
    let snap = read_snapshot(
        &make_instance(json!({
            "_llm.consecutive_no_tool_call": 3,
        })),
        "thinking",
    );
    let err = apply_caps(&snap, &cfg(), Utc::now()).unwrap_err();
    match err {
        ExecutorError::Llm(LlmErrorCode::ExecutionExhausted, msg) => {
            assert!(msg.contains("F1"), "expected F1 detail, got: {msg}");
            assert!(msg.contains("max_iterations"), "msg: {msg}");
        }
        other => panic!("expected ExecutionExhausted, got {other:?}"),
    }
}

#[test]
fn apply_cumulative_caps_rejects_when_max_tokens_exceeded() {
    let snap = read_snapshot(
        &make_instance(json!({
            "_llm.cumulative_tokens": 1500,
        })),
        "thinking",
    );
    let mut c = cfg();
    c.max_tokens = Some(1000);
    let err = apply_caps(&snap, &c, Utc::now()).unwrap_err();
    assert!(matches!(
        err,
        ExecutorError::Llm(LlmErrorCode::BudgetExceeded, _)
    ));
}

#[test]
fn apply_cumulative_caps_rejects_when_max_cost_usd_exceeded() {
    let snap = read_snapshot(
        &make_instance(json!({
            "_llm.cumulative_cost_usd": 2.5,
        })),
        "thinking",
    );
    let mut c = cfg();
    c.max_cost_usd = Some(1.0);
    let err = apply_caps(&snap, &c, Utc::now()).unwrap_err();
    assert!(matches!(
        err,
        ExecutorError::Llm(LlmErrorCode::BudgetExceeded, _)
    ));
}

#[test]
fn apply_cumulative_caps_passes_when_max_tokens_not_set() {
    // Counter is far above any sane budget but the cap is None, so the
    // gate is inert. Mirrors how the `_fire_count.*` cap behaves.
    let snap = read_snapshot(
        &make_instance(json!({
            "_llm.cumulative_tokens": 999_999,
        })),
        "thinking",
    );
    apply_caps(&snap, &cfg(), Utc::now())
        .expect("max_tokens=None means the cumulative_tokens gate is inert");
}

#[test]
fn apply_cumulative_caps_rejects_session_timeout_when_max_seconds_set() {
    // Session start was 120s ago; max_seconds is 60. F1 session timeout.
    let started = Utc::now() - Duration::seconds(120);
    let key = session_started_at_key("thinking");
    let snap = read_snapshot(
        &make_instance(json!({
            key.as_str(): started.to_rfc3339(),
        })),
        "thinking",
    );
    let mut c = cfg();
    c.max_seconds = Some(60);
    let err = apply_caps(&snap, &c, Utc::now()).unwrap_err();
    match err {
        ExecutorError::Llm(LlmErrorCode::ExecutionExhausted, msg) => {
            assert!(msg.contains("F1"), "expected F1 detail, got: {msg}");
            assert!(msg.contains("max_seconds"), "msg: {msg}");
        }
        other => panic!("expected ExecutionExhausted, got {other:?}"),
    }
}

#[test]
fn apply_cumulative_caps_session_timeout_inert_without_started_at() {
    // No prior turn recorded a session_started_at → max_seconds gate
    // is moot for the first turn.
    let snap = read_snapshot(&make_instance(json!({})), "thinking");
    let mut c = cfg();
    c.max_seconds = Some(1);
    apply_caps(&snap, &c, Utc::now()).expect("first-turn-ever bypasses the session timer");
}

// ── build_post_turn_slot_updates ──────────────────────────────────────────

// Note: F3 fixup changed the output shape from flat-dotted keys to a
// nested `{"_llm": {...}}` object so the path resolver in
// `core::mapping::read_in_scopes` can address each slot via the
// standard `$.output._llm.<key>` syntax (no bracket-escape exists for
// flat-dotted keys). Tests below access the nested shape directly.

#[test]
fn build_post_turn_slot_updates_increments_iterations() {
    let pre = SlotSnapshot {
        cumulative_iterations: 4,
        ..Default::default()
    };
    let drained = drained_with_tool(0, 0);
    let out =
        build_post_turn_slot_updates(&pre, &drained, None, false, false, "thinking", Utc::now())
            .expect("no cap active");
    assert_eq!(out["_llm"]["cumulative_iterations"], json!(5));
}

#[test]
fn build_post_turn_slot_updates_increments_tokens_from_usage() {
    let pre = SlotSnapshot {
        cumulative_tokens: 100,
        ..Default::default()
    };
    let drained = drained_with_tool(75, 25); // 100 this turn
    let out =
        build_post_turn_slot_updates(&pre, &drained, None, false, false, "thinking", Utc::now())
            .expect("no cap active");
    assert_eq!(out["_llm"]["cumulative_tokens"], json!(200));
}

#[test]
fn build_post_turn_slot_updates_keeps_cost_at_zero_until_d8() {
    let pre = SlotSnapshot {
        cumulative_cost_usd: 0.05,
        ..Default::default()
    };
    let drained = drained_with_tool(10, 5);
    let out =
        build_post_turn_slot_updates(&pre, &drained, None, false, false, "thinking", Utc::now())
            .expect("no cap active");
    assert_eq!(out["_llm"]["cumulative_cost_usd"], json!(0.05));
}

#[test]
fn build_post_turn_slot_updates_resets_consecutive_no_tool_call_on_success() {
    let pre = SlotSnapshot {
        consecutive_no_tool_call: 2,
        ..Default::default()
    };
    let drained = drained_with_tool(10, 5);
    let out =
        build_post_turn_slot_updates(&pre, &drained, None, false, false, "thinking", Utc::now())
            .expect("no cap active");
    assert_eq!(out["_llm"]["consecutive_no_tool_call"], json!(0));
}

#[test]
fn build_post_turn_slot_updates_increments_consecutive_no_tool_call_on_failure() {
    let pre = SlotSnapshot {
        consecutive_no_tool_call: 1,
        ..Default::default()
    };
    let drained = DrainedResponse {
        saw_done: true,
        ..Default::default()
    };
    let out =
        build_post_turn_slot_updates(&pre, &drained, None, true, false, "thinking", Utc::now())
            .expect("no cap active so missing usage folds to 0");
    assert_eq!(out["_llm"]["consecutive_no_tool_call"], json!(2));
}

#[test]
fn build_post_turn_slot_updates_writes_session_start_on_first_turn() {
    let pre = SlotSnapshot::default();
    let drained = drained_with_tool(10, 5);
    let now = Utc::now();
    let out = build_post_turn_slot_updates(&pre, &drained, None, false, false, "thinking", now)
        .expect("no cap active");
    let written = out["_llm"]["session"]["thinking"]["started_at"]
        .as_str()
        .expect("session start must be a string");
    assert_eq!(written, now.to_rfc3339());
}

#[test]
fn build_post_turn_slot_updates_reuses_existing_session_start() {
    let earlier = Utc::now() - Duration::seconds(30);
    let pre = SlotSnapshot {
        session_started_at: Some(earlier),
        ..Default::default()
    };
    let drained = drained_with_tool(10, 5);
    let later = Utc::now();
    let out = build_post_turn_slot_updates(&pre, &drained, None, false, false, "thinking", later)
        .expect("no cap active");
    let written = out["_llm"]["session"]["thinking"]["started_at"]
        .as_str()
        .expect("session start must be a string");
    assert_eq!(written, earlier.to_rfc3339());
}

// ── reserved-prefix surface (for the workflow loader's enforcement) ──────

#[test]
fn reserved_llm_prefix_constant_matches_synthetic_namespace() {
    assert_eq!(RESERVED_LLM_PREFIX, "_llm.");
    let key = session_started_at_key("thinking");
    assert!(key.starts_with(RESERVED_LLM_PREFIX));
}
