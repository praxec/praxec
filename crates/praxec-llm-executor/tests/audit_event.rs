//! SPEC §33 D7 — `llm.invocation` audit event payload coverage.
//!
//! The unit tests in `src/audit.rs` could pin most of these, but the
//! payload schema is the operator-facing contract. Keeping the
//! assertions in an integration test makes the field names load-bearing
//! at the crate's public surface: a rename in the builder fails here
//! loudly, not just in a co-located unit test that a refactor might
//! also "fix" without noticing.

use praxec_core::error::LlmErrorCode;
use praxec_llm_executor::audit::{build_invocation_event, InvocationContext, REASONING_ELIDED};
use praxec_llm_executor::response::DrainedResponse;
use praxec_llm_executor::stream_event::{StopReason, TokenUsage, ToolCallRequest};
use serde_json::Value;

fn base_ctx<'a>(
    workflow_id: &'a str,
    state: &'a str,
    model: &'a str,
    capture_reasoning: bool,
) -> InvocationContext<'a> {
    InvocationContext {
        workflow_id,
        state,
        model,
        correlation_id: None,
        latency_ms: 1230,
        cost_usd: None,
        tool_call_emitted: None,
        error_code: None,
        capture_reasoning,
    }
}

fn drained_full() -> DrainedResponse {
    DrainedResponse {
        saw_done: true,
        text: "ok".into(),
        reasoning_text: "the plan is...".into(),
        tool_calls: vec![ToolCallRequest {
            id: "c1".into(),
            name: "mark_as_bug".into(),
            arguments: "{}".into(),
        }],
        usage: Some(TokenUsage {
            input_tokens: 1842,
            output_tokens: 87,
            reasoning_tokens: Some(12),
        }),
        stop_reason: Some(StopReason::EndTurn),
        ..Default::default()
    }
}

#[test]
fn build_invocation_event_with_full_usage() {
    let drained = drained_full();
    let mut ctx = base_ctx("wf_42", "triage", "anthropic:claude-sonnet-4-6", true);
    ctx.tool_call_emitted = Some("mark_as_bug");

    let event = build_invocation_event(ctx, &drained);

    assert_eq!(event.event_type, "llm.invocation");
    assert_eq!(event.actor.as_deref(), Some("agent:llm-executor"));
    assert_eq!(event.workflow_id.as_deref(), Some("wf_42"));

    let payload = &event.payload;
    assert_eq!(payload["event_type"], Value::from("llm.invocation"));
    assert_eq!(payload["workflow_id"], Value::from("wf_42"));
    assert_eq!(payload["state"], Value::from("triage"));
    assert_eq!(payload["model"], Value::from("anthropic:claude-sonnet-4-6"));
    assert_eq!(payload["tokens_in"], Value::from(1842));
    assert_eq!(payload["tokens_out"], Value::from(87));
    assert_eq!(payload["tokens_reasoning"], Value::from(12));
    assert_eq!(payload["latency_ms"], Value::from(1230));
    assert_eq!(payload["cost_usd"], Value::Null);
    assert_eq!(payload["usage_present"], Value::from(true));
    assert_eq!(payload["stop_reason"], Value::from("end_turn"));
    assert_eq!(payload["tool_call_emitted"], Value::from("mark_as_bug"));
    assert_eq!(payload["error_code"], Value::Null);
    assert_eq!(payload["reasoning"], Value::from("the plan is..."));
}

#[test]
fn build_invocation_event_with_missing_usage() {
    let drained = DrainedResponse {
        saw_done: true,
        ..Default::default()
    };
    let mut ctx = base_ctx("wf_1", "thinking", "openai:gpt-5", true);
    ctx.error_code = Some(LlmErrorCode::UsageMissing);

    let event = build_invocation_event(ctx, &drained);

    let payload = &event.payload;
    assert_eq!(payload["usage_present"], Value::from(false));
    assert_eq!(payload["tokens_in"], Value::from(0));
    assert_eq!(payload["tokens_out"], Value::from(0));
    assert_eq!(payload["tokens_reasoning"], Value::Null);
}

#[test]
fn build_invocation_event_with_reasoning_captured() {
    let drained = DrainedResponse {
        saw_done: true,
        reasoning_text: "step-by-step: A, B, C".into(),
        ..Default::default()
    };
    let ctx = base_ctx("wf_1", "thinking", "openai:gpt-5", true);

    let event = build_invocation_event(ctx, &drained);

    assert_eq!(
        event.payload["reasoning"],
        Value::from("step-by-step: A, B, C"),
        "captured reasoning must appear verbatim when capture_reasoning is true"
    );
}

#[test]
fn build_invocation_event_with_reasoning_elided() {
    let drained = DrainedResponse {
        saw_done: true,
        reasoning_text: "secret thought trace".into(),
        ..Default::default()
    };
    // capture_reasoning = false → privacy opt-out.
    let ctx = base_ctx("wf_1", "thinking", "openai:gpt-5", false);

    let event = build_invocation_event(ctx, &drained);

    assert_eq!(
        event.payload["reasoning"],
        Value::from(REASONING_ELIDED),
        "elided form must be the literal `<elided>` sentinel"
    );
    // Real text must NOT leak.
    let payload_json =
        serde_json::to_string(&event.payload).expect("payload must serialize for grep");
    assert!(
        !payload_json.contains("secret thought trace"),
        "elided payload must not carry the original reasoning text"
    );
}

#[test]
fn build_invocation_event_with_empty_reasoning_omits_field() {
    let drained = DrainedResponse {
        saw_done: true,
        // reasoning_text default = empty string.
        ..Default::default()
    };
    let ctx = base_ctx("wf_1", "thinking", "openai:gpt-5", true);

    let event = build_invocation_event(ctx, &drained);

    assert!(
        event.payload.get("reasoning").is_none(),
        "empty reasoning must omit the field entirely"
    );
}

#[test]
fn build_invocation_event_with_error_code() {
    let drained = DrainedResponse {
        saw_done: true,
        ..Default::default()
    };
    let mut ctx = base_ctx("wf_1", "thinking", "openai:gpt-5", true);
    ctx.error_code = Some(LlmErrorCode::NoToolCall);

    let event = build_invocation_event(ctx, &drained);

    assert_eq!(
        event.payload["error_code"],
        Value::from("LLM_NO_TOOL_CALL"),
        "error_code must serialize as the wire-stable string"
    );
}

#[test]
fn build_invocation_event_on_success_has_null_error_code() {
    let drained = drained_full();
    let mut ctx = base_ctx("wf_1", "thinking", "openai:gpt-5", true);
    ctx.tool_call_emitted = Some("mark_as_bug");
    // error_code stays None.

    let event = build_invocation_event(ctx, &drained);

    assert_eq!(event.payload["error_code"], Value::Null);
}

#[test]
fn build_invocation_event_includes_stop_reason() {
    // Cover the snake-case mapping for every well-known StopReason variant.
    let variants: Vec<(StopReason, &str)> = vec![
        (StopReason::EndTurn, "end_turn"),
        (StopReason::Length, "length"),
        (StopReason::ToolCalls, "tool_calls"),
        (StopReason::ContentFilter, "content_filter"),
        (StopReason::FunctionCall, "function_call"),
        (StopReason::Unknown, "unknown"),
    ];
    for (reason, expected_wire) in variants {
        let drained = DrainedResponse {
            saw_done: true,
            stop_reason: Some(reason),
            ..Default::default()
        };
        let ctx = base_ctx("wf_1", "thinking", "openai:gpt-5", true);
        let event = build_invocation_event(ctx, &drained);
        assert_eq!(
            event.payload["stop_reason"],
            Value::from(expected_wire),
            "stop_reason for {reason:?} must serialize as {expected_wire}"
        );
    }
}

#[test]
fn build_invocation_event_with_no_tool_call_has_null_tool_emitted() {
    let drained = DrainedResponse {
        saw_done: true,
        ..Default::default()
    };
    let mut ctx = base_ctx("wf_1", "thinking", "openai:gpt-5", true);
    ctx.error_code = Some(LlmErrorCode::NoToolCall);
    // tool_call_emitted stays None.

    let event = build_invocation_event(ctx, &drained);

    assert_eq!(event.payload["tool_call_emitted"], Value::Null);
}

#[test]
fn build_invocation_event_includes_correlation_id_when_set() {
    let drained = drained_full();
    let mut ctx = base_ctx("wf_1", "thinking", "openai:gpt-5", true);
    ctx.correlation_id = Some("cor_abc");
    ctx.tool_call_emitted = Some("mark_as_bug");

    let event = build_invocation_event(ctx, &drained);

    assert_eq!(event.payload["correlation_id"], Value::from("cor_abc"));
    assert_eq!(event.correlation_id, "cor_abc");
}

#[test]
fn build_invocation_event_omits_correlation_id_when_none() {
    let drained = drained_full();
    let mut ctx = base_ctx("wf_1", "thinking", "openai:gpt-5", true);
    ctx.tool_call_emitted = Some("mark_as_bug");
    // correlation_id stays None.

    let event = build_invocation_event(ctx, &drained);

    assert!(
        event.payload.get("correlation_id").is_none(),
        "correlation_id must be omitted from the payload when not set"
    );
}

#[test]
fn build_invocation_event_serializes_cost_usd_when_set() {
    let drained = drained_full();
    let mut ctx = base_ctx("wf_1", "thinking", "openai:gpt-5", true);
    ctx.cost_usd = Some(0.0042);
    ctx.tool_call_emitted = Some("mark_as_bug");

    let event = build_invocation_event(ctx, &drained);

    // f64 equality on a literal is fine here — no arithmetic happened.
    assert_eq!(
        event.payload["cost_usd"].as_f64(),
        Some(0.0042),
        "cost_usd must round-trip the operator-supplied USD figure"
    );
}

#[test]
fn build_invocation_event_payload_pins_field_names() {
    // This test is the canary: it asserts every documented payload key
    // is present (or, for omit-when-none fields, absent in the expected
    // shape). Any future rename — `tokens_in` → `inputTokens`, etc. —
    // fails here loudly, which is the point.
    let drained = drained_full();
    let mut ctx = base_ctx("wf_1", "thinking", "openai:gpt-5", true);
    ctx.tool_call_emitted = Some("mark_as_bug");
    ctx.correlation_id = Some("cor_x");

    let event = build_invocation_event(ctx, &drained);
    let obj = event
        .payload
        .as_object()
        .expect("payload must be a JSON object");

    for key in [
        "event_type",
        "workflow_id",
        "state",
        "model",
        "tokens_in",
        "tokens_out",
        "tokens_reasoning",
        "latency_ms",
        "cost_usd",
        "usage_present",
        "stop_reason",
        "tool_call_emitted",
        "error_code",
        "reasoning",
        "correlation_id",
    ] {
        assert!(
            obj.contains_key(key),
            "operator-facing payload must include `{key}`"
        );
    }
}
