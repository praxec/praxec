//! SPEC §33 D5 — stream drainer + post-stream validation for the
//! in-runtime LLM executor.
//!
//! Streaming is FINAL-ONLY: the executor drains the entire provider stream into
//! a [`DrainedResponse`], then emits ONE outcome to the runtime — either a
//! `NextTransition` or a typed `ExecutorError::Llm(…)`. No per-token
//! pass-through. Reasoning is captured into a buffer for the `llm.invocation`
//! audit payload.
//!
//! Events are the provider-agnostic [`StreamEvent`] (rig is the engine; the
//! provider factory maps rig's stream into these). Every variant is matched
//! explicitly so a future addition breaks compilation instead of silently
//! dropping data.

use std::fmt::Display;

use futures::{Stream, StreamExt};
use praxec_core::error::{ExecutorError, LlmErrorCode};

use crate::config::LlmExecutorConfig;
use crate::stream_event::{StopReason, StreamEvent, TokenUsage, ToolCallRequest};

/// Everything we collect from one provider streaming call. Returned by
/// [`drain_stream`]; consumed by [`validate`] for the typed `LlmErrorCode`
/// classification.
#[derive(Debug, Default, Clone)]
pub struct DrainedResponse {
    /// Concatenation of every `StreamEvent::Text` chunk in order.
    pub text: String,
    /// Concatenation of every `StreamEvent::Reasoning` chunk — the model-readable
    /// summary the `llm.invocation` audit payload carries (D7).
    pub reasoning_text: String,
    /// Encrypted/opaque reasoning blocks captured off the wire. CMP-043: held
    /// in-memory for a future replay seam, NOT written to the audit log (a
    /// privacy review gates that). Do not add them to
    /// `audit::build_invocation_event`.
    pub encrypted_reasoning: Vec<EncryptedReasoningBlock>,
    /// Completed tool calls in stream order. Multi-tool-call rejection happens at
    /// the validate boundary, not here.
    pub tool_calls: Vec<ToolCallRequest>,
    /// Token usage. Captured for F6 (budget caps require it).
    pub usage: Option<TokenUsage>,
    /// `StreamEvent::Done.stop_reason`, if any.
    pub stop_reason: Option<StopReason>,
    /// A model-side `Error` event message, if any. Surfaced as
    /// `LlmErrorCode::ProviderError` by `validate` — distinct from a Rust-level
    /// transport error (`ProviderTransport`).
    pub stream_error: Option<String>,
    /// `true` iff the stream ended with a `Done` event. `validate` uses it to
    /// distinguish a clean completion from a truncated stream.
    pub saw_done: bool,
    /// `StreamEvent::Start.message_id`, captured for audit.
    pub message_id: Option<String>,
}

/// Captured encrypted/opaque reasoning block. SPEC §33 keeps it model-opaque, so
/// we surface only what the wire already exposes.
#[derive(Debug, Clone)]
pub struct EncryptedReasoningBlock {
    pub id: String,
    pub content: String,
}

/// Drain `stream` to completion, accumulating every event into a
/// [`DrainedResponse`]. Returns:
///
/// - `Ok(drained)` if the stream produced any sequence of `Ok(_)` events —
///   including a final model-side `Error` event (not a transport error).
/// - `Err(ExecutorError::Llm(LlmErrorCode::ProviderTransport, …))` if the stream
///   itself produced a transport-level `Err(_)` — distinct from a model-side
///   `Error` event per SPEC §33 audit fixup F6 (operators graph them separately).
///
/// Generic over the error type so unit tests can feed `std::io::Error`-typed
/// streams; production binds the rig stream's error to a string.
pub async fn drain_stream<S, E>(mut stream: S) -> Result<DrainedResponse, ExecutorError>
where
    S: Stream<Item = Result<StreamEvent, E>> + Unpin + Send,
    E: Display,
{
    let mut drained = DrainedResponse::default();
    while let Some(event) = stream.next().await {
        match event {
            Ok(ev) => match ev {
                StreamEvent::Start { message_id } => drained.message_id = Some(message_id),
                StreamEvent::Text { chunk } => drained.text.push_str(&chunk),
                StreamEvent::Reasoning { chunk } => drained.reasoning_text.push_str(&chunk),
                StreamEvent::EncryptedReasoning { id, content } => {
                    drained
                        .encrypted_reasoning
                        .push(EncryptedReasoningBlock { id, content });
                }
                StreamEvent::ToolCall(tool_call) => drained.tool_calls.push(tool_call),
                StreamEvent::Usage(tokens) => drained.usage = Some(tokens),
                StreamEvent::Done { stop_reason } => {
                    drained.stop_reason = stop_reason;
                    drained.saw_done = true;
                }
                StreamEvent::Error { message } => drained.stream_error = Some(message),
            },
            Err(transport_err) => {
                // SPEC §33 audit fixup (F6 STUB-006): transport-level failure
                // (HTTP, TLS, DNS, reset) — distinct from a model-side `Error`
                // event so dashboards attribute network flap separately. Both
                // classify as Connection class so reliability is unchanged.
                return Err(ExecutorError::Llm(
                    LlmErrorCode::ProviderTransport,
                    format!("LLM provider stream errored: {transport_err}"),
                ));
            }
        }
    }
    Ok(drained)
}

/// Validate a [`DrainedResponse`] against the executor config + the per-turn tool
/// list. `Ok(())` iff exactly one tool call was emitted, its name appears in
/// `valid_names`, its arguments parse as JSON, AND (when budget caps are
/// configured) the provider emitted usage. Each branch produces one typed error.
pub fn validate(
    drained: &DrainedResponse,
    config: &LlmExecutorConfig,
    valid_names: &[&str],
    state: &str,
) -> Result<(), ExecutorError> {
    // 1. Mid-stream `Error` event from the provider.
    if let Some(msg) = &drained.stream_error {
        return Err(ExecutorError::Llm(
            LlmErrorCode::ProviderError,
            format!("Provider emitted Error event at state '{state}': {msg}"),
        ));
    }

    // 2. Truncation: no Done event AND no Error event.
    if !drained.saw_done {
        return Err(ExecutorError::Llm(
            LlmErrorCode::StreamTruncated,
            format!("Provider stream ended at state '{state}' without a `Done` or `Error` event"),
        ));
    }

    // 3. Multi-tool-call rejection (SPEC §33 lock).
    if drained.tool_calls.len() > 1 {
        return Err(ExecutorError::Llm(
            LlmErrorCode::MultiToolCall,
            format!(
                "Model emitted {} tool calls at state '{}'; SPEC §33 requires exactly one per turn",
                drained.tool_calls.len(),
                state
            ),
        ));
    }

    // 4. No tool call — FMECA F1. Permanent: the reliability layer does NOT
    // retry; the workflow author's max_iterations is the only retry budget.
    if drained.tool_calls.is_empty() {
        let valid_listing = format_valid(valid_names);
        return Err(ExecutorError::Llm(
            LlmErrorCode::NoToolCall,
            format!(
                "Model returned final answer instead of selecting a transition at state '{state}'; \
                 valid: [{valid_listing}]"
            ),
        ));
    }

    let tool_call = &drained.tool_calls[0];

    // 5. Unknown tool name.
    if !valid_names.iter().any(|n| *n == tool_call.name) {
        let valid_listing = format_valid(valid_names);
        return Err(ExecutorError::Llm(
            LlmErrorCode::UnknownTool,
            format!(
                "tool '{}' is not a valid transition at state '{state}'; valid: [{valid_listing}]",
                tool_call.name
            ),
        ));
    }

    // 6. Malformed arguments — must parse as JSON. An empty string is valid for
    // tools with no inputs (treated as `{}`).
    let args_str = if tool_call.arguments.trim().is_empty() {
        "{}"
    } else {
        tool_call.arguments.as_str()
    };
    if let Err(err) = serde_json::from_str::<serde_json::Value>(args_str) {
        return Err(ExecutorError::Llm(
            LlmErrorCode::MalformedArguments,
            format!(
                "tool '{}' arguments did not parse as JSON at state '{state}': {err}",
                tool_call.name
            ),
        ));
    }

    // 7. Usage missing while a budget cap is configured (FMECA F6).
    let budget_active = config.max_cost_usd.is_some() || config.max_tokens.is_some();
    if budget_active && drained.usage.is_none() {
        return Err(ExecutorError::Llm(
            LlmErrorCode::UsageMissing,
            format!(
                "Provider stream completed without usage but budget caps are active at state \
                 '{state}'; refusing to silently accept null cost"
            ),
        ));
    }

    Ok(())
}

fn format_valid(names: &[&str]) -> String {
    names
        .iter()
        .map(|n| format!("'{n}'"))
        .collect::<Vec<_>>()
        .join(", ")
}

#[cfg(test)]
mod tests {
    use super::*;
    use futures::stream;

    fn cfg(max_tokens: Option<u64>, max_cost: Option<f64>) -> LlmExecutorConfig {
        LlmExecutorConfig {
            model: Some("openai:gpt-5".into()),
            affinity: None,
            strategy: None,
            needs: vec![],
            prompt_template: "x".into(),
            max_iterations: 3,
            max_seconds: None,
            max_tokens,
            max_cost_usd: max_cost,
            reasoning_effort: None,
            capture_reasoning: true,
        }
    }

    fn ok_stream(
        events: Vec<StreamEvent>,
    ) -> impl Stream<Item = Result<StreamEvent, std::io::Error>> + Unpin + Send {
        stream::iter(
            events
                .into_iter()
                .map(Ok::<StreamEvent, std::io::Error>)
                .collect::<Vec<_>>(),
        )
    }

    fn call(name: &str, args: &str) -> ToolCallRequest {
        ToolCallRequest {
            id: "c".into(),
            name: name.into(),
            arguments: args.into(),
        }
    }

    #[tokio::test]
    async fn drain_happy_path_text_tool_usage_done() {
        let events = vec![
            StreamEvent::Start {
                message_id: "msg_1".into(),
            },
            StreamEvent::Text {
                chunk: "Sure, ".into(),
            },
            StreamEvent::Text {
                chunk: "advancing.".into(),
            },
            StreamEvent::ToolCall(call("advance", "{}")),
            StreamEvent::Usage(TokenUsage {
                input_tokens: 100,
                output_tokens: 50,
                reasoning_tokens: None,
            }),
            StreamEvent::Done {
                stop_reason: Some(StopReason::ToolCalls),
            },
        ];
        let drained = drain_stream(ok_stream(events)).await.unwrap();
        assert_eq!(drained.text, "Sure, advancing.");
        assert_eq!(drained.tool_calls.len(), 1);
        assert_eq!(drained.tool_calls[0].name, "advance");
        assert!(drained.usage.is_some());
        assert_eq!(drained.stop_reason, Some(StopReason::ToolCalls));
        assert!(drained.saw_done);
        assert_eq!(drained.message_id.as_deref(), Some("msg_1"));
    }

    #[tokio::test]
    async fn drain_accumulates_reasoning_chunks() {
        let events = vec![
            StreamEvent::Reasoning {
                chunk: "Step 1. ".into(),
            },
            StreamEvent::Reasoning {
                chunk: "Step 2.".into(),
            },
            StreamEvent::ToolCall(call("advance", "{}")),
            StreamEvent::Done { stop_reason: None },
        ];
        let drained = drain_stream(ok_stream(events)).await.unwrap();
        assert_eq!(drained.reasoning_text, "Step 1. Step 2.");
    }

    #[tokio::test]
    async fn drain_captures_encrypted_reasoning() {
        let events = vec![
            StreamEvent::EncryptedReasoning {
                id: "r_1".into(),
                content: "opaque".into(),
            },
            StreamEvent::ToolCall(call("advance", "{}")),
            StreamEvent::Done { stop_reason: None },
        ];
        let drained = drain_stream(ok_stream(events)).await.unwrap();
        assert_eq!(drained.encrypted_reasoning.len(), 1);
        assert_eq!(drained.encrypted_reasoning[0].id, "r_1");
        assert_eq!(drained.encrypted_reasoning[0].content, "opaque");
    }

    #[tokio::test]
    async fn drain_mid_stream_error_event_populates_stream_error() {
        let events = vec![
            StreamEvent::Error {
                message: "rate limited".into(),
            },
            StreamEvent::Done { stop_reason: None },
        ];
        let drained = drain_stream(ok_stream(events)).await.unwrap();
        assert_eq!(drained.stream_error.as_deref(), Some("rate limited"));
        assert!(drained.saw_done);
    }

    #[tokio::test]
    async fn drain_truncated_stream_returns_drained_without_done_or_error() {
        let drained = drain_stream(ok_stream(vec![StreamEvent::Text {
            chunk: "...".into(),
        }]))
        .await
        .unwrap();
        assert!(!drained.saw_done);
        assert!(drained.stream_error.is_none());
        assert!(drained.stop_reason.is_none());
    }

    #[tokio::test]
    async fn drain_transport_error_is_provider_transport() {
        let s = stream::iter(vec![
            Ok(StreamEvent::Text { chunk: "x".into() }),
            Err(std::io::Error::other("network blip")),
        ]);
        match drain_stream(s).await.unwrap_err() {
            ExecutorError::Llm(LlmErrorCode::ProviderTransport, msg) => {
                assert!(msg.contains("network blip"));
            }
            other => panic!("expected ProviderTransport, got {other:?}"),
        }
    }

    #[test]
    fn validate_no_tool_call_returns_no_tool_call() {
        let drained = DrainedResponse {
            saw_done: true,
            ..Default::default()
        };
        match validate(&drained, &cfg(None, None), &["advance"], "thinking").unwrap_err() {
            ExecutorError::Llm(LlmErrorCode::NoToolCall, msg) => assert!(msg.contains("advance")),
            other => panic!("expected NoToolCall, got {other:?}"),
        }
    }

    #[test]
    fn validate_multi_tool_call_returns_multi_tool_call() {
        let drained = DrainedResponse {
            saw_done: true,
            tool_calls: vec![call("advance", "{}"), call("reject", "{}")],
            ..Default::default()
        };
        let err = validate(
            &drained,
            &cfg(None, None),
            &["advance", "reject"],
            "thinking",
        )
        .unwrap_err();
        assert!(matches!(
            err,
            ExecutorError::Llm(LlmErrorCode::MultiToolCall, _)
        ));
    }

    #[test]
    fn validate_unknown_tool_name() {
        let drained = DrainedResponse {
            saw_done: true,
            tool_calls: vec![call("explode", "{}")],
            ..Default::default()
        };
        match validate(&drained, &cfg(None, None), &["advance"], "thinking").unwrap_err() {
            ExecutorError::Llm(LlmErrorCode::UnknownTool, msg) => {
                assert!(msg.contains("explode") && msg.contains("advance"));
            }
            other => panic!("expected UnknownTool, got {other:?}"),
        }
    }

    #[test]
    fn validate_malformed_arguments() {
        let drained = DrainedResponse {
            saw_done: true,
            tool_calls: vec![call("advance", "{not-json")],
            ..Default::default()
        };
        let err = validate(&drained, &cfg(None, None), &["advance"], "thinking").unwrap_err();
        assert!(matches!(
            err,
            ExecutorError::Llm(LlmErrorCode::MalformedArguments, _)
        ));
    }

    #[test]
    fn validate_usage_missing_with_budget_cap() {
        let drained = DrainedResponse {
            saw_done: true,
            tool_calls: vec![call("advance", "{}")],
            ..Default::default()
        };
        let err = validate(&drained, &cfg(Some(1000), None), &["advance"], "thinking").unwrap_err();
        assert!(matches!(
            err,
            ExecutorError::Llm(LlmErrorCode::UsageMissing, _)
        ));
    }

    #[test]
    fn validate_usage_missing_without_budget_caps_is_ok() {
        let drained = DrainedResponse {
            saw_done: true,
            tool_calls: vec![call("advance", "{}")],
            ..Default::default()
        };
        validate(&drained, &cfg(None, None), &["advance"], "thinking")
            .expect("missing usage is fine when no budget caps are set");
    }

    #[test]
    fn validate_truncation_returns_stream_truncated() {
        let err = validate(
            &DrainedResponse::default(),
            &cfg(None, None),
            &["advance"],
            "thinking",
        )
        .unwrap_err();
        assert!(matches!(
            err,
            ExecutorError::Llm(LlmErrorCode::StreamTruncated, _)
        ));
    }

    #[test]
    fn validate_stream_error_returns_provider_error() {
        let drained = DrainedResponse {
            saw_done: true,
            stream_error: Some("503 unavailable".into()),
            ..Default::default()
        };
        let err = validate(&drained, &cfg(None, None), &["advance"], "thinking").unwrap_err();
        assert!(matches!(
            err,
            ExecutorError::Llm(LlmErrorCode::ProviderError, _)
        ));
    }

    #[test]
    fn validate_empty_arguments_treated_as_empty_object() {
        let drained = DrainedResponse {
            saw_done: true,
            tool_calls: vec![call("advance", "")],
            ..Default::default()
        };
        validate(&drained, &cfg(None, None), &["advance"], "thinking")
            .expect("empty argument string should be accepted as {}");
    }
}
