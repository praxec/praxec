//! SPEC §33 D7 — `llm.invocation` audit event builder.
//!
//! D5 emitted a passthrough JSON blob keyed off `kind: "drained"` /
//! `"stream_error"`. D7 replaces that with a stable, operator-facing
//! payload shape that every consumer of the audit log can rely on. The
//! field names are part of the public contract; never rename them.
//!
//! Payload schema (per SPEC §33 plan FMECA-driven hardening):
//!
//! ```json
//! {
//!   "event_type": "llm.invocation",
//!   "workflow_id": "...",
//!   "state": "...",
//!   "model": "anthropic:claude-sonnet-4-6",
//!   "tokens_in": 1842,
//!   "tokens_out": 87,
//!   "tokens_reasoning": 12,        // or null
//!   "latency_ms": 1230,
//!   "cost_usd": 0.0042,            // or null when D8 catalog misses
//!   "usage_present": true,
//!   "stop_reason": "end_turn",     // or null
//!   "tool_call_emitted": "mark_as_bug", // or null
//!   "error_code": null,            // or "LLM_NO_TOOL_CALL" / etc.
//!   "message_id": "msg_...",       // provider-side request id, or null
//!   "reasoning": "<captured>",     // present iff captured AND non-empty
//!   "correlation_id": "..."        // present iff threaded through
//! }
//! ```
//!
//! Reasoning convention:
//! - When `capture_reasoning == true` AND `drained.reasoning_text` is
//!   non-empty, the literal reasoning text is included.
//! - When `capture_reasoning == false` AND `drained.reasoning_text` is
//!   non-empty, the literal string `"<elided>"` is included instead
//!   (the SPEC §33 D7 privacy / compliance opt-out marker).
//! - When `drained.reasoning_text` is empty, the `reasoning` field is
//!   absent from the payload entirely.

use praxec_core::audit::AuditEvent;
use praxec_core::error::LlmErrorCode;
use serde_json::{Map, Value};

use crate::response::DrainedResponse;

/// Sentinel value used in the `reasoning` field when reasoning was
/// captured but the operator opted out via `capture_reasoning: false`.
/// Operators can grep for this marker to count elided turns.
pub const REASONING_ELIDED: &str = "<elided>";

/// Per-turn context the executor knows at audit-emit time. The builder
/// shapes these plus the [`DrainedResponse`] into the stable payload.
///
/// `cost_usd` is `None` until D8 lands the catalog lookup; operators
/// graph the null-rate to spot catalog drift.
pub struct InvocationContext<'a> {
    pub workflow_id: &'a str,
    pub state: &'a str,
    pub model: &'a str,
    pub correlation_id: Option<&'a str>,
    pub latency_ms: u64,
    /// `None` while D8's catalog lookup is not yet wired; once that
    /// lands, also `None` when the catalog has no entry for `model`.
    pub cost_usd: Option<f64>,
    /// `Some(name)` on the success path (validated tool call); `None`
    /// on any failure path that did not select exactly one valid tool.
    pub tool_call_emitted: Option<&'a str>,
    /// `Some(code)` on the failure path, `None` on success. Serialized
    /// as the wire-stable code string (e.g. `"LLM_NO_TOOL_CALL"`).
    pub error_code: Option<LlmErrorCode>,
    /// Privacy / compliance toggle. `true` (default) includes reasoning
    /// text in the event; `false` substitutes [`REASONING_ELIDED`].
    pub capture_reasoning: bool,
}

/// Build the `llm.invocation` audit event from a per-turn context plus
/// the drained provider response. Returns an [`AuditEvent`] ready for
/// the sink.
///
/// On the failure path, callers pass an `InvocationContext` with
/// `error_code = Some(code)` and `tool_call_emitted = None`; the
/// builder still captures whatever the drainer collected (usage,
/// stop_reason, reasoning, etc.) so operators can post-mortem the
/// failure from a single record.
pub fn build_invocation_event(ctx: InvocationContext<'_>, drained: &DrainedResponse) -> AuditEvent {
    let usage_present = drained.usage.is_some();

    let (tokens_in, tokens_out, tokens_reasoning) = match drained.usage {
        Some(u) => (
            Value::from(u.input_tokens),
            Value::from(u.output_tokens),
            match u.reasoning_tokens {
                Some(n) => Value::from(n),
                None => Value::Null,
            },
        ),
        None => (Value::from(0_u32), Value::from(0_u32), Value::Null),
    };

    let stop_reason = drained
        .stop_reason
        .as_ref()
        .map(format_stop_reason)
        .map_or(Value::Null, Value::from);

    let tool_call_emitted = ctx
        .tool_call_emitted
        .map_or(Value::Null, |name| Value::from(name.to_string()));

    let error_code = ctx
        .error_code
        .map_or(Value::Null, |code| Value::from(code.as_wire_code()));

    let cost_usd = match ctx.cost_usd {
        Some(c) => Value::from(c),
        None => Value::Null,
    };

    let mut payload = Map::new();
    payload.insert("event_type".into(), Value::from("llm.invocation"));
    payload.insert("workflow_id".into(), Value::from(ctx.workflow_id));
    payload.insert("state".into(), Value::from(ctx.state));
    payload.insert("model".into(), Value::from(ctx.model));
    payload.insert("tokens_in".into(), tokens_in);
    payload.insert("tokens_out".into(), tokens_out);
    payload.insert("tokens_reasoning".into(), tokens_reasoning);
    payload.insert("latency_ms".into(), Value::from(ctx.latency_ms));
    payload.insert("cost_usd".into(), cost_usd);
    payload.insert("usage_present".into(), Value::from(usage_present));
    payload.insert("stop_reason".into(), stop_reason);
    payload.insert("tool_call_emitted".into(), tool_call_emitted);
    payload.insert("error_code".into(), error_code);

    // CMP-043: the provider's `Start { message_id }` is captured in the
    // drained response specifically so audit can carry it; emit it here
    // (null when the stream never produced a Start event) so operators
    // can correlate a turn back to the provider-side request id.
    let message_id = drained
        .message_id
        .as_ref()
        .map_or(Value::Null, |id| Value::from(id.clone()));
    payload.insert("message_id".into(), message_id);

    // Reasoning: present iff non-empty. Elided form when capture is off.
    if !drained.reasoning_text.is_empty() {
        let reasoning_value = if ctx.capture_reasoning {
            Value::from(drained.reasoning_text.clone())
        } else {
            Value::from(REASONING_ELIDED)
        };
        payload.insert("reasoning".into(), reasoning_value);
    }

    if let Some(cor) = ctx.correlation_id {
        payload.insert("correlation_id".into(), Value::from(cor));
    }

    let mut event = AuditEvent::new("llm.invocation")
        .with_actor("agent:llm-executor")
        .with_workflow(ctx.workflow_id)
        .with_payload(Value::Object(payload));

    if let Some(cor) = ctx.correlation_id {
        event = event.with_correlation(cor);
    }

    event
}

/// Format a [`StopReason`](crate::stream_event::StopReason) as the wire-stable
/// snake_case string operators read off the audit log.
fn format_stop_reason(reason: &crate::stream_event::StopReason) -> String {
    use crate::stream_event::StopReason;
    match reason {
        StopReason::EndTurn => "end_turn",
        StopReason::Length => "length",
        StopReason::ToolCalls => "tool_calls",
        StopReason::ContentFilter => "content_filter",
        StopReason::FunctionCall => "function_call",
        StopReason::Unknown => "unknown",
    }
    .to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ctx() -> InvocationContext<'static> {
        InvocationContext {
            workflow_id: "wf_1",
            state: "thinking",
            model: "anthropic:claude-sonnet-4-6",
            correlation_id: None,
            latency_ms: 10,
            cost_usd: None,
            tool_call_emitted: None,
            error_code: None,
            capture_reasoning: true,
        }
    }

    /// CMP-043: `message_id` is captured off the provider's `Start`
    /// event specifically so audit can carry it; the builder must emit
    /// it into the payload.
    #[test]
    fn message_id_is_emitted_when_present() {
        let drained = DrainedResponse {
            message_id: Some("msg_abc".into()),
            ..Default::default()
        };
        let event = build_invocation_event(ctx(), &drained);
        assert_eq!(
            event.payload.get("message_id"),
            Some(&Value::from("msg_abc"))
        );
    }

    /// CMP-043: when no `Start` event arrived the field is still present
    /// (null), so the payload shape is stable across turns.
    #[test]
    fn message_id_is_null_when_absent() {
        let drained = DrainedResponse::default();
        let event = build_invocation_event(ctx(), &drained);
        assert_eq!(event.payload.get("message_id"), Some(&Value::Null));
    }
}
