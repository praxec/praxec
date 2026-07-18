//! SPEC §33 D5 — integration coverage for the aether-llm wiring.
//!
//! The unit tests in `src/prompt.rs` and `src/response.rs` exercise
//! the building blocks. This file pins their integration shape from
//! the public surface (`drain_stream`, `validate`,
//! `links_to_tool_definitions`) so refactors that change a module's
//! internals can't quietly break the executor's published contract.
//!
//! No real provider calls — every test feeds `drain_stream` a mock
//! `futures::stream::iter` so the assertions are deterministic.

use futures::stream;
use praxec_core::error::{ExecutorError, LlmErrorCode};
use praxec_llm_executor::stream_event::{StopReason, StreamEvent, TokenUsage, ToolCallRequest};
use praxec_llm_executor::{
    config::LlmExecutorConfig,
    prompt::links_to_tool_definitions,
    response::{DrainedResponse, drain_stream, validate},
};
use serde_json::json;

fn cfg() -> LlmExecutorConfig {
    LlmExecutorConfig {
        model: Some("openai:gpt-5".into()),
        affinity: None,
        strategy: None,
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

fn cfg_with_budget() -> LlmExecutorConfig {
    let mut c = cfg();
    c.max_tokens = Some(1000);
    c
}

fn ok_stream(
    events: Vec<StreamEvent>,
) -> impl futures::Stream<Item = Result<StreamEvent, std::io::Error>> + Unpin + Send {
    stream::iter(
        events
            .into_iter()
            .map(Ok::<StreamEvent, std::io::Error>)
            .collect::<Vec<_>>(),
    )
}

#[tokio::test]
async fn drain_happy_path_text_tool_usage_done() {
    let events = vec![
        StreamEvent::Text {
            chunk: "OK ".into(),
        },
        StreamEvent::ToolCall(ToolCallRequest {
            id: "c1".into(),
            name: "advance".into(),
            arguments: "{\"note\":\"hi\"}".into(),
        }),
        StreamEvent::Usage(TokenUsage {
            input_tokens: 10,
            output_tokens: 5,
            reasoning_tokens: None,
        }),
        StreamEvent::Done {
            stop_reason: Some(StopReason::ToolCalls),
        },
    ];
    let drained = drain_stream(ok_stream(events)).await.unwrap();
    assert_eq!(drained.text, "OK ");
    assert_eq!(drained.tool_calls.len(), 1);
    assert_eq!(drained.tool_calls[0].name, "advance");
    assert!(drained.usage.is_some());
    assert!(drained.saw_done);
    validate(&drained, &cfg(), &["advance"], "thinking").unwrap();
}

#[tokio::test]
async fn drain_reasoning_chunks_accumulate_into_buffer() {
    let events = vec![
        StreamEvent::Reasoning {
            chunk: "Plan A. ".into(),
        },
        StreamEvent::Reasoning {
            chunk: "Plan B.".into(),
        },
        StreamEvent::ToolCall(ToolCallRequest {
            id: "c".into(),
            name: "advance".into(),
            arguments: "{}".into(),
        }),
        StreamEvent::Done { stop_reason: None },
    ];
    let drained = drain_stream(ok_stream(events)).await.unwrap();
    assert_eq!(drained.reasoning_text, "Plan A. Plan B.");
}

#[tokio::test]
async fn drain_mid_stream_error_event_is_provider_error_at_validate() {
    let events = vec![
        StreamEvent::Error {
            message: "model overloaded".into(),
        },
        StreamEvent::Done { stop_reason: None },
    ];
    let drained = drain_stream(ok_stream(events)).await.unwrap();
    let err = validate(&drained, &cfg(), &["advance"], "thinking").unwrap_err();
    match err {
        ExecutorError::Llm(LlmErrorCode::ProviderError, msg) => {
            assert!(msg.contains("model overloaded"), "msg: {msg}");
        }
        other => panic!("expected ProviderError, got {other:?}"),
    }
}

#[tokio::test]
async fn drain_truncated_stream_validates_to_stream_truncated() {
    let events = vec![StreamEvent::Text {
        chunk: "incomplete".into(),
    }];
    let drained = drain_stream(ok_stream(events)).await.unwrap();
    assert!(!drained.saw_done);
    assert!(drained.stream_error.is_none());
    let err = validate(&drained, &cfg(), &["advance"], "thinking").unwrap_err();
    assert!(matches!(
        err,
        ExecutorError::Llm(LlmErrorCode::StreamTruncated, _)
    ));
}

#[test]
fn validate_no_tool_call_returns_no_tool_call() {
    let drained = DrainedResponse {
        saw_done: true,
        text: "the answer is 42".into(),
        ..Default::default()
    };
    let err = validate(&drained, &cfg(), &["advance"], "thinking").unwrap_err();
    match err {
        ExecutorError::Llm(LlmErrorCode::NoToolCall, msg) => {
            assert!(msg.contains("advance"), "msg: {msg}");
        }
        other => panic!("expected NoToolCall, got {other:?}"),
    }
}

#[test]
fn validate_multi_tool_call_returns_multi_tool_call() {
    let drained = DrainedResponse {
        saw_done: true,
        tool_calls: vec![
            ToolCallRequest {
                id: "a".into(),
                name: "advance".into(),
                arguments: "{}".into(),
            },
            ToolCallRequest {
                id: "b".into(),
                name: "reject".into(),
                arguments: "{}".into(),
            },
        ],
        ..Default::default()
    };
    let err = validate(&drained, &cfg(), &["advance", "reject"], "thinking").unwrap_err();
    assert!(matches!(
        err,
        ExecutorError::Llm(LlmErrorCode::MultiToolCall, _)
    ));
}

#[test]
fn validate_unknown_tool_name_returns_unknown_tool() {
    let drained = DrainedResponse {
        saw_done: true,
        tool_calls: vec![ToolCallRequest {
            id: "a".into(),
            name: "explode".into(),
            arguments: "{}".into(),
        }],
        ..Default::default()
    };
    let err = validate(&drained, &cfg(), &["advance"], "thinking").unwrap_err();
    match err {
        ExecutorError::Llm(LlmErrorCode::UnknownTool, msg) => {
            assert!(
                msg.contains("explode") && msg.contains("advance"),
                "msg: {msg}"
            );
        }
        other => panic!("expected UnknownTool, got {other:?}"),
    }
}

#[test]
fn validate_malformed_arguments_returns_malformed_arguments() {
    let drained = DrainedResponse {
        saw_done: true,
        tool_calls: vec![ToolCallRequest {
            id: "a".into(),
            name: "advance".into(),
            arguments: "{not-json".into(),
        }],
        ..Default::default()
    };
    let err = validate(&drained, &cfg(), &["advance"], "thinking").unwrap_err();
    assert!(matches!(
        err,
        ExecutorError::Llm(LlmErrorCode::MalformedArguments, _)
    ));
}

#[test]
fn validate_usage_missing_when_budget_cap_set_returns_usage_missing() {
    let drained = DrainedResponse {
        saw_done: true,
        tool_calls: vec![ToolCallRequest {
            id: "a".into(),
            name: "advance".into(),
            arguments: "{}".into(),
        }],
        ..Default::default()
    };
    let err = validate(&drained, &cfg_with_budget(), &["advance"], "thinking").unwrap_err();
    assert!(matches!(
        err,
        ExecutorError::Llm(LlmErrorCode::UsageMissing, _)
    ));
}

#[test]
fn validate_usage_missing_without_budget_caps_is_ok() {
    let drained = DrainedResponse {
        saw_done: true,
        tool_calls: vec![ToolCallRequest {
            id: "a".into(),
            name: "advance".into(),
            arguments: "{}".into(),
        }],
        ..Default::default()
    };
    validate(&drained, &cfg(), &["advance"], "thinking")
        .expect("missing usage is acceptable when no budget caps are configured");
}

#[test]
fn links_to_tools_rejects_duplicate_rel_with_typed_code() {
    let links = vec![
        json!({ "rel": "advance" }),
        json!({ "rel": "advance", "title": "duplicate" }),
    ];
    let err = links_to_tool_definitions(&links, "thinking").unwrap_err();
    match err {
        ExecutorError::Llm(LlmErrorCode::DuplicateTransitionRel, msg) => {
            assert!(msg.contains("advance"), "msg: {msg}");
            assert!(msg.contains("thinking"), "msg: {msg}");
        }
        other => panic!("expected DuplicateTransitionRel, got {other:?}"),
    }
}

#[test]
fn links_to_tools_missing_input_schema_defaults_to_empty_object() {
    let links = vec![json!({ "rel": "advance" })];
    let tools = links_to_tool_definitions(&links, "thinking").unwrap();
    let params = &tools[0].parameters;
    assert_eq!(params["type"], "object");
    assert_eq!(params["additionalProperties"], false);
    assert!(params["properties"].as_object().unwrap().is_empty());
}
