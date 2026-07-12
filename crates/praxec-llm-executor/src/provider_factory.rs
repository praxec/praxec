//! SPEC §33 D9 — pluggable provider factory, on **rig**.
//!
//! The executor doesn't hard-code which provider to build for a
//! `provider:model-id` string; construction is hoisted behind this trait.
//! Production wires [`DefaultProviderFactory`] (anthropic / openai / gemini /
//! openrouter / ollama via rig's typed clients); integration tests inject a mock
//! that returns canned [`StreamEvent`] streams.
//!
//! rig has no dynamic provider client, so the factory matches the vendor to build
//! the right typed client (the cockpit pattern). The factory maps rig's stream to
//! the executor's provider-agnostic [`StreamEvent`]s **lazily** — each event is
//! forwarded as it arrives (via an `async_stream` generator), not pre-collected.
//! Laziness is load-bearing: the agent runner's per-event stall watchdog can only
//! bound inter-event silence if the underlying model wait actually happens while
//! the consumer polls `.next()`. An earlier version drained the whole turn into a
//! `Vec` before returning, which moved every token (and any first-token hang) to
//! *before* the watchdog saw the stream — so a hung reasoning call sat at 0 CPU
//! until the whole-session wall, and the advertised stall timeout never fired.
//! The trailing token-usage aggregate + `Done` are emitted after the stream ends.

use async_trait::async_trait;
use futures::stream::{BoxStream, Stream, StreamExt};
use praxec_core::error::{ExecutorError, LlmErrorCode};
use praxec_core::providers::ProviderId;
use rig::client::{CompletionClient, ProviderClient};
use rig::completion::{CompletionModel, GetTokenUsage, Message, ToolDefinition};
use rig::providers::{anthropic, gemini, ollama, openai, openrouter};
use rig::streaming::{StreamedAssistantContent, StreamingCompletion};
use serde_json::Value;

use crate::stream_event::{StreamEvent, TokenUsage, ToolCallRequest};

/// One streaming turn's inputs (provider-agnostic). The skill body (when in
/// scope) is the system `preamble`; the rendered prompt is the user message.
/// `Clone` so a retry wrapper can re-issue the same turn across attempts.
#[derive(Clone)]
pub struct TurnRequest {
    pub system: Option<String>,
    /// The new message for this turn — `Message::user(text)` for a plain prompt,
    /// or a tool-result message for an agent tool-loop continuation.
    pub prompt: Message,
    pub tools: Vec<ToolDefinition>,
    /// Prior conversation (ADR-0007 — the agent tool loop carries the goal +
    /// tool-call + tool-result turns here so the model iterates with context).
    /// Empty for a single-turn call (`kind: llm`).
    pub history: Vec<Message>,
    /// Reasoning-effort `additional_params` (per-provider native shape).
    pub reasoning: Option<Value>,
    /// Forced tool choice for this turn. The runner sets `Required` on the
    /// terminal/stalled turn (offering ONLY `final_answer`) to COMPEL the model
    /// to terminate with a result — the structural poka-yoke for AGENT_NO_RESULT
    /// (a model that ignores the completion-protocol prompt and exits empty).
    /// `None` = provider default (Auto).
    pub tool_choice: Option<rig::message::ToolChoice>,
}

/// SPEC §33 D9 — the seam the executor calls to open a streaming turn. Narrow by
/// design: one method mapping `provider:model-id` + a [`TurnRequest`] to the
/// event stream. All knowledge of which providers exist lives behind it.
#[async_trait]
pub trait ProviderFactory: Send + Sync {
    async fn stream(
        &self,
        model_str: &str,
        turn: TurnRequest,
    ) -> Result<BoxStream<'static, Result<StreamEvent, String>>, ExecutorError>;
}

/// Production-default factory wiring rig's five providers (anthropic, openai,
/// gemini, openrouter, ollama). An unknown provider returns a typed
/// `LlmErrorCode::ProviderError`.
#[derive(Debug, Default, Clone, Copy)]
pub struct DefaultProviderFactory;

#[async_trait]
impl ProviderFactory for DefaultProviderFactory {
    async fn stream(
        &self,
        model_str: &str,
        turn: TurnRequest,
    ) -> Result<BoxStream<'static, Result<StreamEvent, String>>, ExecutorError> {
        let (vendor, model) = model_str.split_once(':').ok_or_else(|| {
            ExecutorError::Permanent(format!(
                "LLM executor: model '{model_str}' is not in `provider:model-id` form"
            ))
        })?;
        let TurnRequest {
            system,
            prompt,
            tools,
            history,
            reasoning,
            tool_choice,
        } = turn;

        // Each arm builds the provider's typed client + agent; `stream_rig` is
        // generic over the resulting model so the lazy drain is written once. The
        // client-construction error surfaces synchronously (before any stream is
        // returned); a mid-turn connect/stream error is folded into the stream as
        // an `Err` item (matching the prior behavior the runner already handles).
        macro_rules! run {
            ($client:expr_2021) => {{
                let client = $client.map_err(provider_factory_err)?;
                let mut b = client.agent(model);
                if let Some(s) = system.as_deref() {
                    b = b.preamble(s);
                }
                Box::pin(stream_rig(
                    b.build(),
                    prompt,
                    history,
                    tools,
                    reasoning,
                    tool_choice,
                )) as BoxStream<'static, Result<StreamEvent, String>>
            }};
        }

        let stream: BoxStream<'static, Result<StreamEvent, String>> =
            match ProviderId::from_slug(vendor) {
                Some(ProviderId::Anthropic) => run!(anthropic::Client::from_env()),
                // A base-URL override → an OpenAI-*compatible* endpoint (proxy / self-
                // hosted / mock), which speaks **Chat Completions**, not the Responses
                // API that api.openai.com uses. No override → the default Responses
                // client (which still honors rig's native `OPENAI_BASE_URL`).
                Some(ProviderId::Openai) => match openai_base_override() {
                    Some(base) => run!(openai_completions_client(&base)),
                    None => run!(openai::Client::from_env()),
                },
                Some(ProviderId::Gemini) => run!(gemini::Client::from_env()),
                Some(ProviderId::Openrouter) => run!(openrouter::Client::from_env()),
                Some(ProviderId::Ollama) => run!(ollama::Client::from_env()),
                _ => {
                    return Err(ExecutorError::Llm(
                        LlmErrorCode::ProviderError,
                        format!(
                            "LLM executor: provider '{vendor}' is not wired; supported: \
                         anthropic, openai, gemini, openrouter, ollama"
                        ),
                    ));
                }
            };
        Ok(stream)
    }
}

/// The base-URL override for the OpenAI-compatible path: `PRAXEC_LLM_BASE_URL`
/// (praxec's explicit, provider-agnostic knob) or rig's native `OPENAI_BASE_URL`.
/// `None` (or empty) means "use api.openai.com via the Responses API". This is the
/// seam the deterministic mock-endpoint E2E drives.
fn openai_base_override() -> Option<String> {
    pick_base(
        std::env::var("PRAXEC_LLM_BASE_URL").ok(),
        std::env::var("OPENAI_BASE_URL").ok(),
    )
}

/// Pure selection (testable without touching process env): `PRAXEC_LLM_BASE_URL`
/// wins over the native `OPENAI_BASE_URL`; an empty string is treated as unset.
fn pick_base(praxec: Option<String>, openai: Option<String>) -> Option<String> {
    praxec.or(openai).filter(|s| !s.is_empty())
}

/// Build a **Chat Completions** client at `base` — the OpenAI-compatible surface
/// that proxies / self-hosted servers / mocks implement (unlike the Responses
/// API). With a custom endpoint a real key is usually unnecessary, so
/// `OPENAI_API_KEY` is optional (a dummy is used).
fn openai_completions_client(
    base: &str,
) -> Result<openai::CompletionsClient, rig::client::ProviderClientError> {
    let key = std::env::var("OPENAI_API_KEY").unwrap_or_else(|_| "praxec-test".to_string());
    openai::CompletionsClient::builder()
        .api_key(&key)
        .base_url(base)
        .build()
        .map_err(Into::into)
}

/// Drive a rig agent's stream for one turn **lazily**: map each rig event to a
/// provider-agnostic [`StreamEvent`] as it arrives, yielding it to the consumer
/// immediately. The connect (`stream_completion`/`stream`) awaits happen inside
/// the generator too, so the *whole* model wait — connection and first-token —
/// occurs while the runner polls `.next()` and is therefore bounded by its
/// per-event stall watchdog (a hung call surfaces as a Timeout the chain-walk
/// escalates, rather than dead-airing to the whole-session wall). The trailing
/// token-usage aggregate + `Done` are emitted after the underlying stream ends.
/// A connect/stream error is folded in as an `Err` item — the same shape the old
/// eager path produced and the runner already tolerates.
fn stream_rig<M>(
    agent: rig::agent::Agent<M>,
    prompt: Message,
    history: Vec<Message>,
    tools: Vec<ToolDefinition>,
    reasoning: Option<Value>,
    tool_choice: Option<rig::message::ToolChoice>,
) -> impl Stream<Item = Result<StreamEvent, String>> + Send
where
    M: CompletionModel + 'static,
{
    async_stream::stream! {
        let builder = match agent.stream_completion(prompt, history).await {
            Ok(b) => b,
            Err(e) => {
                yield Err(format!("couldn't start the turn: {e}"));
                return;
            }
        };
        let mut builder = builder.tools(tools);
        if let Some(p) = reasoning {
            builder = builder.additional_params(p);
        }
        if let Some(tc) = tool_choice {
            builder = builder.tool_choice(tc);
        }
        let mut stream = match builder.stream().await {
            Ok(s) => s,
            Err(e) => {
                yield Err(format!("couldn't reach the model: {e}"));
                return;
            }
        };

        while let Some(item) = stream.next().await {
            match item {
                Ok(StreamedAssistantContent::Text(t)) => {
                    yield Ok(StreamEvent::Text { chunk: t.text });
                }
                Ok(StreamedAssistantContent::Reasoning(r)) => {
                    let text = r.display_text();
                    if !text.is_empty() {
                        yield Ok(StreamEvent::Reasoning { chunk: text });
                    }
                }
                Ok(StreamedAssistantContent::ToolCall { tool_call, .. }) => {
                    yield Ok(StreamEvent::ToolCall(ToolCallRequest {
                        id: tool_call.id,
                        name: tool_call.function.name,
                        arguments: tool_call.function.arguments.to_string(),
                    }));
                }
                Ok(_) => {} // tool-call deltas, etc. — not surfaced
                Err(e) => {
                    yield Err(format!("stream error: {e}"));
                    return;
                }
            }
        }

        // The final aggregate carries token usage (set as the stream drains).
        if let Some(u) = stream.response.token_usage() {
            yield Ok(StreamEvent::Usage(TokenUsage {
                input_tokens: u.input_tokens,
                output_tokens: u.output_tokens,
                reasoning_tokens: None,
            }));
        }
        yield Ok(StreamEvent::Done { stop_reason: None });
    }
}

fn provider_factory_err(err: impl std::fmt::Display) -> ExecutorError {
    ExecutorError::Llm(
        LlmErrorCode::ProviderError,
        format!("LLM executor: provider construction failed: {err}"),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn turn() -> TurnRequest {
        TurnRequest {
            system: None,
            prompt: Message::user("hi"),
            tools: vec![],
            history: vec![],
            tool_choice: None,
            reasoning: None,
        }
    }

    #[test]
    fn praxec_base_url_wins_over_openai_base_url() {
        let got = pick_base(Some("http://mock".into()), Some("http://other".into()));
        assert_eq!(got.as_deref(), Some("http://mock"));
    }

    #[test]
    fn openai_base_url_is_the_fallback() {
        assert_eq!(
            pick_base(None, Some("http://native".into())).as_deref(),
            Some("http://native")
        );
    }

    #[test]
    fn no_override_means_the_default_endpoint() {
        assert_eq!(pick_base(None, None), None);
    }

    #[test]
    fn an_empty_override_is_treated_as_unset() {
        assert_eq!(pick_base(Some(String::new()), None), None);
    }

    #[tokio::test]
    async fn unknown_provider_is_rejected_as_not_wired() {
        let Err(ExecutorError::Llm(_, msg)) =
            DefaultProviderFactory.stream("nope:model", turn()).await
        else {
            panic!("unknown provider must be rejected with an Llm error");
        };
        assert!(
            msg.contains("is not wired"),
            "expected 'is not wired': {msg}"
        );
        assert!(
            msg.contains("openrouter"),
            "error should name the supported set: {msg}"
        );
    }

    #[tokio::test]
    async fn malformed_model_string_is_permanent() {
        // (Ok holds a BoxStream, which isn't Debug — so match instead of unwrap.)
        let Err(err) = DefaultProviderFactory.stream("no-colon", turn()).await else {
            panic!("a model string without a ':' must be rejected");
        };
        assert!(matches!(err, ExecutorError::Permanent(_)));
    }

    #[tokio::test]
    async fn a_wired_provider_routes_past_the_not_wired_check() {
        // No key set → construction fails ("construction failed"), NOT "is not
        // wired" — proving openrouter is routed to a real build.
        if let Err(e) = DefaultProviderFactory
            .stream("openrouter:some/model", turn())
            .await
        {
            assert!(
                !format!("{e:?}").contains("is not wired"),
                "openrouter must route to a build: {e:?}"
            );
        }
    }
}
