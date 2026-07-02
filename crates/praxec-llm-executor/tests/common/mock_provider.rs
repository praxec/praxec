//! SPEC §33 D9 — adversarial mock provider factory for the in-runtime LLM
//! executor, on the provider-agnostic [`StreamEvent`] vocabulary (rig is the
//! engine; the mock needs no SDK).
//!
//! The default scenario set is ADVERSARIAL: the happy path is one of many. Every
//! [`StreamEvent`] variant is represented (see [`variant_per_response`]); the
//! FMECA F5 gate fails the build if a future variant lacks a scenario. Zero
//! network: each scenario is a `futures::stream::iter` of canned events.

#![allow(dead_code)]

use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use futures::stream::{self, BoxStream};
use praxec_core::error::ExecutorError;
use praxec_llm_executor::stream_event::{StopReason, StreamEvent, TokenUsage, ToolCallRequest};
use praxec_llm_executor::{ProviderFactory, TurnRequest};
use rig::completion::Message;
use rig::message::UserContent;

/// Extract the user text of a turn's prompt message (for `CapturedTurn`).
fn user_message_text(prompt: &Message) -> String {
    match prompt {
        Message::User { content } => content
            .iter()
            .filter_map(|c| match c {
                UserContent::Text(t) => Some(t.text.clone()),
                _ => None,
            })
            .collect::<Vec<_>>()
            .join(" "),
        _ => String::new(),
    }
}

/// Events the mock yields are `Result<StreamEvent, String>` so a transport-level
/// `Err(_)` can be injected mid-stream alongside model-side `Error` events.
type MockItem = Result<StreamEvent, String>;

fn tool(id: &str, name: &str, args: &str) -> StreamEvent {
    StreamEvent::ToolCall(ToolCallRequest {
        id: id.into(),
        name: name.into(),
        arguments: args.into(),
    })
}

fn usage(input: u64, output: u64) -> StreamEvent {
    StreamEvent::Usage(TokenUsage {
        input_tokens: input,
        output_tokens: output,
        reasoning_tokens: None,
    })
}

/// One scenario = one canned event stream. Cloned per call so chain tests can
/// re-poll deterministically.
#[derive(Debug, Clone)]
pub struct MockScenario {
    pub name: &'static str,
    pub events: Vec<MockItem>,
}

impl MockScenario {
    fn new(name: &'static str, events: Vec<MockItem>) -> Self {
        Self { name, events }
    }
}

/// The canonical adversarial scenario catalog.
pub struct MockProviderScenarios;

impl MockProviderScenarios {
    pub fn happy_path() -> MockScenario {
        MockScenario::new(
            "happy_path",
            vec![
                Ok(StreamEvent::Start {
                    message_id: "msg_happy".into(),
                }),
                Ok(tool("call_1", "advance", "{}")),
                Ok(usage(100, 50)),
                Ok(StreamEvent::Done {
                    stop_reason: Some(StopReason::ToolCalls),
                }),
            ],
        )
    }

    pub fn no_tool_call() -> MockScenario {
        MockScenario::new(
            "no_tool_call",
            vec![
                Ok(StreamEvent::Text {
                    chunk: "I think the answer is 42.".into(),
                }),
                Ok(StreamEvent::Done {
                    stop_reason: Some(StopReason::EndTurn),
                }),
            ],
        )
    }

    pub fn multi_tool_call() -> MockScenario {
        MockScenario::new(
            "multi_tool_call",
            vec![
                Ok(tool("call_a", "advance", "{}")),
                Ok(tool("call_b", "reject", "{}")),
                Ok(StreamEvent::Done {
                    stop_reason: Some(StopReason::ToolCalls),
                }),
            ],
        )
    }

    pub fn malformed_arguments() -> MockScenario {
        MockScenario::new(
            "malformed_arguments",
            vec![
                Ok(tool("call_1", "advance", "not-json")),
                Ok(StreamEvent::Done {
                    stop_reason: Some(StopReason::ToolCalls),
                }),
            ],
        )
    }

    /// `Start` only, then the stream ends — `LlmErrorCode::StreamTruncated`.
    pub fn stream_truncated_after_start() -> MockScenario {
        MockScenario::new(
            "stream_truncated_after_start",
            vec![Ok(StreamEvent::Start {
                message_id: "msg_partial".into(),
            })],
        )
    }

    /// `Text` then end, no `Done` — the truncated path (rig has no arg deltas).
    pub fn stream_truncated_after_text() -> MockScenario {
        MockScenario::new(
            "stream_truncated_after_text",
            vec![Ok(StreamEvent::Text {
                chunk: "{\"par".into(),
            })],
        )
    }

    pub fn stream_error_midway() -> MockScenario {
        MockScenario::new(
            "stream_error_midway",
            vec![
                Ok(StreamEvent::Text {
                    chunk: "starting...".into(),
                }),
                Ok(StreamEvent::Error {
                    message: "model overloaded".into(),
                }),
                Ok(StreamEvent::Done { stop_reason: None }),
            ],
        )
    }

    pub fn usage_missing_with_done() -> MockScenario {
        MockScenario::new(
            "usage_missing_with_done",
            vec![
                Ok(tool("call_1", "advance", "{}")),
                Ok(StreamEvent::Done {
                    stop_reason: Some(StopReason::ToolCalls),
                }),
            ],
        )
    }

    pub fn reasoning_chunks() -> MockScenario {
        MockScenario::new(
            "reasoning_chunks",
            vec![
                Ok(StreamEvent::Reasoning {
                    chunk: "Step one: ".into(),
                }),
                Ok(StreamEvent::Reasoning {
                    chunk: "evaluate inputs. ".into(),
                }),
                Ok(StreamEvent::Reasoning {
                    chunk: "Step two: pick a transition.".into(),
                }),
                Ok(tool("call_1", "advance", "{}")),
                Ok(StreamEvent::Done {
                    stop_reason: Some(StopReason::ToolCalls),
                }),
            ],
        )
    }

    pub fn encrypted_reasoning() -> MockScenario {
        MockScenario::new(
            "encrypted_reasoning",
            vec![
                Ok(StreamEvent::EncryptedReasoning {
                    id: "er_1".into(),
                    content: "opaque-blob".into(),
                }),
                Ok(tool("call_1", "advance", "{}")),
                Ok(StreamEvent::Done {
                    stop_reason: Some(StopReason::ToolCalls),
                }),
            ],
        )
    }

    /// One event of EVERY `StreamEvent` variant — the FMECA F5 gate's input.
    pub fn variant_per_response() -> MockScenario {
        MockScenario::new(
            "variant_per_response",
            vec![
                Ok(StreamEvent::Start {
                    message_id: "msg_all".into(),
                }),
                Ok(StreamEvent::Text {
                    chunk: "preamble ".into(),
                }),
                Ok(StreamEvent::Reasoning {
                    chunk: "thinking ".into(),
                }),
                Ok(StreamEvent::EncryptedReasoning {
                    id: "er_all".into(),
                    content: "blob".into(),
                }),
                Ok(tool("call_all", "advance", "{}")),
                Ok(usage(10, 5)),
                Ok(StreamEvent::Error {
                    message: "an error in the same stream".into(),
                }),
                Ok(StreamEvent::Done {
                    stop_reason: Some(StopReason::ToolCalls),
                }),
            ],
        )
    }

    pub fn all() -> Vec<MockScenario> {
        vec![
            Self::happy_path(),
            Self::no_tool_call(),
            Self::multi_tool_call(),
            Self::malformed_arguments(),
            Self::stream_truncated_after_start(),
            Self::stream_truncated_after_text(),
            Self::stream_error_midway(),
            Self::usage_missing_with_done(),
            Self::reasoning_chunks(),
            Self::encrypted_reasoning(),
            Self::variant_per_response(),
        ]
    }
}

/// What the executor asked the factory to stream, captured for assertions.
#[derive(Debug, Clone)]
pub struct CapturedTurn {
    pub system: Option<String>,
    pub prompt: String,
    pub tool_count: usize,
}

impl CapturedTurn {
    /// Messages the turn would send: the user prompt, plus the system preamble
    /// when an in-scope skill produced one.
    pub fn message_count(&self) -> usize {
        1 + usize::from(self.system.is_some())
    }
}

/// A [`ProviderFactory`] that streams a canned scenario per call: `single` always
/// returns the same; `scripted` returns the Nth then loops on the last.
pub struct MockProviderFactory {
    scenarios: Mutex<Vec<MockScenario>>,
    pub seen_models: Arc<Mutex<Vec<String>>>,
    pub seen_turns: Arc<Mutex<Vec<CapturedTurn>>>,
}

impl MockProviderFactory {
    pub fn single(scenario: MockScenario) -> Self {
        Self {
            scenarios: Mutex::new(vec![scenario]),
            seen_models: Arc::new(Mutex::new(Vec::new())),
            seen_turns: Arc::new(Mutex::new(Vec::new())),
        }
    }

    pub fn scripted(scenarios: Vec<MockScenario>) -> Self {
        assert!(
            !scenarios.is_empty(),
            "scripted factory requires at least one scenario"
        );
        Self {
            scenarios: Mutex::new(scenarios),
            seen_models: Arc::new(Mutex::new(Vec::new())),
            seen_turns: Arc::new(Mutex::new(Vec::new())),
        }
    }

    pub fn models_seen(&self) -> Vec<String> {
        match self.seen_models.lock() {
            Ok(g) => g.clone(),
            Err(p) => p.into_inner().clone(),
        }
    }

    pub fn turns_seen(&self) -> Vec<CapturedTurn> {
        match self.seen_turns.lock() {
            Ok(g) => g.clone(),
            Err(p) => p.into_inner().clone(),
        }
    }
}

#[async_trait]
impl ProviderFactory for MockProviderFactory {
    async fn stream(
        &self,
        model_str: &str,
        turn: TurnRequest,
    ) -> Result<BoxStream<'static, MockItem>, ExecutorError> {
        // Match the production factory's malformed-model-string error.
        if model_str.split_once(':').is_none() {
            return Err(ExecutorError::Permanent(format!(
                "LLM executor: model '{model_str}' is not in `provider:model-id` form"
            )));
        }
        if let Ok(mut seen) = self.seen_models.lock() {
            seen.push(model_str.to_string());
        }
        if let Ok(mut seen) = self.seen_turns.lock() {
            seen.push(CapturedTurn {
                system: turn.system.clone(),
                prompt: user_message_text(&turn.prompt),
                tool_count: turn.tools.len(),
            });
        }
        let scenario = {
            let mut guard = match self.scenarios.lock() {
                Ok(g) => g,
                Err(p) => p.into_inner(),
            };
            if guard.len() > 1 {
                guard.remove(0)
            } else if let Some(last) = guard.first() {
                last.clone()
            } else {
                return Err(ExecutorError::Permanent(
                    "MockProviderFactory exhausted with no scenarios".into(),
                ));
            }
        };
        Ok(Box::pin(stream::iter(scenario.events)))
    }
}

// ── FMECA F5 variant-coverage gate ──────────────────────────────────────────

fn tag_of(ev: &StreamEvent) -> &'static str {
    match ev {
        StreamEvent::Start { .. } => "Start",
        StreamEvent::Text { .. } => "Text",
        StreamEvent::Reasoning { .. } => "Reasoning",
        StreamEvent::EncryptedReasoning { .. } => "EncryptedReasoning",
        StreamEvent::ToolCall(_) => "ToolCall",
        StreamEvent::Usage(_) => "Usage",
        StreamEvent::Done { .. } => "Done",
        StreamEvent::Error { .. } => "Error",
    }
}

/// Every `StreamEvent` variant the mock catalog must cover. `tag_of`'s exhaustive
/// match makes the compiler fail first if a variant is added.
pub const ALL_STREAM_EVENT_VARIANTS: &[&str] = &[
    "Start",
    "Text",
    "Reasoning",
    "EncryptedReasoning",
    "ToolCall",
    "Usage",
    "Done",
    "Error",
];

#[test]
fn fmeca_f5_every_variant_has_a_scenario() {
    let scenario = MockProviderScenarios::variant_per_response();
    let mut observed: std::collections::HashSet<&'static str> = std::collections::HashSet::new();
    for ev in scenario.events.iter().flatten() {
        observed.insert(tag_of(ev));
    }
    for variant in ALL_STREAM_EVENT_VARIANTS {
        assert!(
            observed.contains(variant),
            "FMECA F5: variant `{variant}` is not represented in variant_per_response"
        );
    }
    assert_eq!(observed.len(), ALL_STREAM_EVENT_VARIANTS.len());
}

#[test]
fn scenarios_catalog_lists_all_scenarios() {
    assert!(
        MockProviderScenarios::all().len() >= 11,
        "the adversarial catalog must ship at least 11 scenarios"
    );
}
