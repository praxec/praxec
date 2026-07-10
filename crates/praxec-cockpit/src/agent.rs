//! The in-process Mission Control LLM driver — the central cockpit tool
//! (ADR-0005), on **rig**.
//!
//! The chat LLM drives the cockpit's **canonical** op surface
//! ([`crate::op::op_tools`] / [`crate::op::op_from_tool_call`]) directly,
//! in-process. One LLM call per user turn selects at most one navigation tool,
//! mirroring the §33 executor's one-tool-per-turn rule; the deterministic
//! [`crate::op::parse_command`] stays as the offline fallback.
//!
//! rig has no dynamic provider client (each provider is a distinct typed
//! `CompletionModel`), so [`run_turn_streaming`] matches the vendor to build the
//! right client from env (the keys our setup gate persists), then a single
//! generic [`run_agent`] drives any provider's stream. Reasoning effort is
//! per-provider — rig exposes no unified knob — so [`reasoning_params`] emits
//! each provider's native `additional_params` shape.

use crate::llm::ChatModel;
use crate::op;
use futures::{Stream, StreamExt};
use rig::client::{CompletionClient, ProviderClient};
use rig::completion::{CompletionModel, Message, ToolDefinition};
use rig::providers::{anthropic, gemini, ollama, openai, openrouter};
use rig::streaming::{StreamedAssistantContent, StreamingCompletion};
use serde_json::Value;

/// A tool call the model chose, decoupled from any provider SDK type.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ToolCallRequest {
    pub id: String,
    pub name: String,
    /// JSON arguments, as a string (parsed by the dispatch in `op`).
    pub arguments: String,
}

/// One transcript line, owned so a turn can be spawned with a snapshot of the
/// conversation (no borrow of the UI's `app::ChatLine`).
#[derive(Debug, Clone)]
pub struct Turn {
    pub you: bool,
    pub text: String,
}

/// An incremental event from a turn in flight — emitted as the model streams so
/// the UI can render narration token-by-token.
#[derive(Debug, Clone)]
pub enum AgentEvent {
    /// A chunk of narration text to append to the in-progress reply.
    Text(String),
    /// The turn finished: the model chose at most one tool to call.
    Done(Option<ToolCallRequest>),
    /// The turn failed (provider/network/model error) — narrate and stop.
    Failed(String),
}

const SYSTEM: &str = "You are Mission Control, the conductor of a fleet cockpit. \
The user supervises governed missions on a zoomable map. To move the map, call \
exactly one navigation tool (zoom_into, zoom_out, pan, quit). If the user is \
only talking, reply in one short line and call no tool. Never invent missions.";

/// The prior transcript as rig chat-history messages (the new user message and
/// the system preamble are supplied separately at call time).
pub fn history_messages(history: &[Turn]) -> Vec<Message> {
    history
        .iter()
        .map(|t| {
            if t.you {
                Message::user(t.text.clone())
            } else {
                Message::assistant(t.text.clone())
            }
        })
        .collect()
}

/// The cockpit's ops as rig tool definitions (the canonical schema from
/// [`op::op_tools`]).
pub fn tool_definitions() -> Vec<ToolDefinition> {
    op::op_tools()
        .into_iter()
        .map(|t| ToolDefinition {
            name: t.name.to_string(),
            description: t.description.to_string(),
            parameters: t.schema,
        })
        .collect()
}

/// Per-provider reasoning-effort `additional_params` — rig has no unified knob,
/// so each provider gets its **native** shape. `medium`/empty → `None` (the
/// provider's own default; never send a param that a provider would reject).
/// Per-provider reasoning-effort `additional_params` — the shared core builder
/// (`tuning.reasoning`), used by both the cockpit chat loop and the executor.
pub use praxec_core::tuning::reasoning_params;

/// Run one turn against a real model, emitting [`AgentEvent`]s as it streams.
/// Builds the right rig client for the vendor from env (the keys the setup gate
/// persisted), then a generic agent drive. Provider construction failure is a
/// single `Failed`.
pub async fn run_turn_streaming(
    model: &ChatModel,
    history: &[Turn],
    user_msg: &str,
    emit: impl FnMut(AgentEvent),
) {
    let msgs = history_messages(history);
    let user = user_msg.to_string();
    let extra = reasoning_params(&model.vendor, &model.reasoning_effort);
    let name = model.model.as_str();

    // Each arm builds the provider's typed client + agent; `run_agent` is generic
    // over the resulting model so the streaming drive is written once.
    macro_rules! drive {
        ($client:expr_2021) => {
            match $client {
                Ok(client) => {
                    let agent = client.agent(name).preamble(SYSTEM).build();
                    run_agent(agent, msgs, user, extra, emit).await;
                }
                Err(e) => {
                    let mut emit = emit;
                    emit(AgentEvent::Failed(format!(
                        "couldn't reach {} · {}: {e}",
                        model.vendor, model.model
                    )));
                }
            }
        };
    }

    match model.vendor.as_str() {
        "anthropic" => drive!(anthropic::Client::from_env()),
        "openai" => drive!(openai::Client::from_env()),
        "gemini" => drive!(gemini::Client::from_env()),
        "openrouter" => drive!(openrouter::Client::from_env()),
        "ollama" => drive!(ollama::Client::from_env()),
        other => {
            let mut emit = emit;
            emit(AgentEvent::Failed(format!("unknown vendor: {other}")));
        }
    }
}

/// Drive one streaming turn for an already-built agent: attach the cockpit tools
/// (and any reasoning params), open the stream, and forward events. Generic over
/// the provider model so every vendor shares this path.
async fn run_agent<M>(
    agent: rig::agent::Agent<M>,
    history: Vec<Message>,
    user_msg: String,
    extra: Option<Value>,
    mut emit: impl FnMut(AgentEvent),
) where
    M: CompletionModel,
{
    let builder = match agent
        .stream_completion(Message::user(user_msg), history)
        .await
    {
        Ok(b) => b,
        Err(e) => {
            emit(AgentEvent::Failed(format!("couldn't start the turn: {e}")));
            return;
        }
    };
    let mut builder = builder.tools(tool_definitions());
    if let Some(p) = extra {
        builder = builder.additional_params(p);
    }
    match builder.stream().await {
        Ok(stream) => drive_streaming(stream, emit).await,
        Err(e) => emit(AgentEvent::Failed(format!("couldn't reach the model: {e}"))),
    }
}

/// Drive a rig assistant-content stream, emitting an [`AgentEvent::Text`] per
/// text delta and a terminal [`AgentEvent::Done`] carrying the first completed
/// tool call (one-tool-per-turn). A stream error emits [`AgentEvent::Failed`]
/// and stops (no `Done`). Generic over the content's final-response type `R` and
/// the error type so it is unit-testable with a synthetic stream.
pub async fn drive_streaming<S, R, E>(stream: S, mut emit: impl FnMut(AgentEvent))
where
    S: Stream<Item = Result<StreamedAssistantContent<R>, E>>,
    E: std::fmt::Display,
{
    futures::pin_mut!(stream);
    let mut tool_call: Option<ToolCallRequest> = None;
    while let Some(event) = stream.next().await {
        match event {
            Ok(StreamedAssistantContent::Text(t)) => emit(AgentEvent::Text(t.text)),
            Ok(StreamedAssistantContent::ToolCall { tool_call: tc, .. }) => {
                if tool_call.is_none() {
                    tool_call = Some(ToolCallRequest {
                        id: tc.id,
                        name: tc.function.name,
                        arguments: tc.function.arguments.to_string(),
                    });
                }
            }
            // Reasoning blocks, partial deltas, and the final aggregate are not
            // surfaced to the cockpit narration.
            Ok(_) => {}
            Err(e) => {
                emit(AgentEvent::Failed(format!("stream error: {e}")));
                return;
            }
        }
    }
    emit(AgentEvent::Done(tool_call));
}

#[cfg(test)]
mod tests {
    use super::*;
    use rig::completion::message::{Text, ToolCall, ToolFunction};
    use serde_json::json;

    #[test]
    fn history_replays_prior_turns_in_order() {
        let history = vec![
            Turn {
                you: true,
                text: "hi".into(),
            },
            Turn {
                you: false,
                text: "hello".into(),
            },
        ];
        let msgs = history_messages(&history);
        assert_eq!(msgs.len(), 2);
    }

    #[test]
    fn tool_definitions_carry_the_op_tools() {
        let defs = tool_definitions();
        assert_eq!(defs.len(), op::op_tools().len());
        assert!(defs.iter().any(|d| d.name == "zoom_into"));
    }

    #[test]
    fn reasoning_is_per_provider_native_shape() {
        // OpenRouter / OpenAI: reasoning.effort.
        assert_eq!(
            reasoning_params("openrouter", "high"),
            Some(json!({ "reasoning": { "effort": "high" } }))
        );
        // Anthropic: a thinking token budget.
        let a = reasoning_params("anthropic", "high").unwrap();
        assert_eq!(a["thinking"]["type"], "enabled");
        assert!(a["thinking"]["budget_tokens"].as_i64().unwrap() > 0);
        // medium / default → nothing sent (provider default stands).
        assert_eq!(reasoning_params("openai", "medium"), None);
        // unknown provider → nothing sent.
        assert_eq!(reasoning_params("ollama", "high"), None);
    }

    // ── the streaming drive (synthetic streams; no provider needed) ───────────

    fn text(s: &str) -> StreamedAssistantContent<()> {
        StreamedAssistantContent::Text(Text::from(s))
    }

    fn tool(id: &str, name: &str, args: Value) -> StreamedAssistantContent<()> {
        StreamedAssistantContent::ToolCall {
            tool_call: ToolCall {
                id: id.into(),
                call_id: None,
                function: ToolFunction {
                    name: name.into(),
                    arguments: args,
                },
                signature: None,
                additional_params: None,
            },
            internal_call_id: id.into(),
        }
    }

    fn stream_of(
        events: Vec<StreamedAssistantContent<()>>,
    ) -> impl Stream<Item = Result<StreamedAssistantContent<()>, String>> {
        futures::stream::iter(events.into_iter().map(Ok::<_, String>))
    }

    async fn collect(
        s: impl Stream<Item = Result<StreamedAssistantContent<()>, String>>,
    ) -> Vec<AgentEvent> {
        let mut events = Vec::new();
        drive_streaming(s, |ev| events.push(ev)).await;
        events
    }

    #[tokio::test]
    async fn streams_text_chunks_live_then_done() {
        let events = collect(stream_of(vec![text("Look"), text("ing…")])).await;
        let joined: String = events
            .iter()
            .filter_map(|e| match e {
                AgentEvent::Text(t) => Some(t.as_str()),
                _ => None,
            })
            .collect();
        assert_eq!(joined, "Looking…");
        assert!(matches!(events.last(), Some(AgentEvent::Done(None))));
    }

    #[tokio::test]
    async fn done_carries_the_first_tool_call() {
        let events = collect(stream_of(vec![
            tool("c1", "zoom_into", json!({ "mission": "postgres" })),
            tool("c2", "quit", json!({})),
        ]))
        .await;
        match events.last() {
            Some(AgentEvent::Done(Some(call))) => {
                assert_eq!(call.name, "zoom_into");
                assert!(call.arguments.contains("postgres"));
            }
            other => panic!("expected Done(Some(zoom_into)), got {other:?}"),
        }
    }

    #[tokio::test]
    async fn a_stream_error_emits_failed_and_no_done() {
        let s = futures::stream::iter(vec![Err::<StreamedAssistantContent<()>, String>(
            "boom".into(),
        )]);
        let events = collect(s).await;
        assert!(matches!(&events[..], [AgentEvent::Failed(m)] if m.contains("boom")));
    }
}
