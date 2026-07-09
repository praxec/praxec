//! ADR-0007 — the in-process, rig-driven [`AgentSessionRunner`]. Replaces the
//! aether subprocess for the governed case. Reuses the llm-executor's
//! [`ProviderFactory`] (rig under the hood, one provider wiring shared with
//! `kind: llm`), so it is real and mockable.
//!
//! It runs a **multi-turn tool loop**: each turn exposes the agent's MCP tools
//! (via a [`ToolHost`]) plus `final_answer`; on a tool call it executes the tool,
//! appends the assistant tool-call + tool-result to the conversation, and loops
//! (bounded by `max_turns`); on `final_answer` it returns the structured result.
//! A session that declares `tools` but has no `ToolHost` wired **fails fast** —
//! tools are never silently dropped.

use std::collections::BTreeMap;
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use futures::StreamExt;
use praxec_core::error::{ErrorClass, ExecutorError};

use crate::error::{AgentErrorCode, permanent};
use praxec_llm_executor::{ProviderFactory, StreamEvent, TurnRequest};
use rig::OneOrMany;
use rig::completion::{AssistantContent, Message, ToolDefinition};
use rig::message::{ToolResult, ToolResultContent, UserContent};
use serde_json::{Value, json};

use crate::session::{
    AgentResult, AgentRunOutcome, AgentRunReport, AgentSession, AgentSessionRunner, AgentStatus,
};
pub use crate::tool_budget::{MAX_TOOL_RESULT_BYTES, ToolHost, tool_definition_from};
pub(crate) use crate::tool_budget::{enforce_history_budget, is_transient};
// Test-only: a budget test asserts on `history_bytes` directly; re-export it for
// `tests`' `use super::*` without flagging an unused import in the non-test build.
#[cfg(test)]
pub(crate) use crate::tool_budget::history_bytes;
// Truncation is no longer the production ingress path (spill_on_ingress is), but a
// budget test still exercises it directly — re-export it test-only so `tests`'
// `use super::*` resolves it without an unused-import flag in the prod build.
#[cfg(test)]
pub(crate) use crate::tool_budget::truncate_tool_result;

/// Default ceiling on tool-loop turns — bounds a runaway agent. Set to 24 so a
/// multi-step coding/self-edit turn (search_file → read_range → edit_file across
/// several spans, then final_answer) fits; greenfield writes use far fewer, and
/// 24 still bounds a runaway.
pub const DEFAULT_MAX_TURNS: u32 = 24;

/// Ceiling on the CUMULATIVE conversation history re-sent every turn. The
/// per-result cap above bounds ONE result, but the loop appends each turn's
/// tool results + assistant text to a history that is re-sent in full on every
/// subsequent turn — so across `DEFAULT_MAX_TURNS` turns with several tool calls
/// each, the request balloons without bound (observed live: 2.77M tokens / ~10
/// MB on a codebase-reading design agent, hard-400ing the 1,048,576-token
/// endpoint). This is the LoopGuard for that: before each turn we elide the
/// OLDEST tool-result/assistant turn-pairs (keeping the goal + recent turns)
/// until the history fits, so the request never approaches the context window.
/// 1 MiB ≈ 256k tokens — far under any provider limit, with ample room for the
/// system prompt, the current turn's input, and the model's reply.
pub const DEFAULT_MAX_HISTORY_BYTES: usize = 1024 * 1024;

pub const TOOL_SETUP_TIMEOUT: Duration = Duration::from_secs(60); // Bounding tool setup to prevent hung server stalls

/// The completion protocol the runner ALWAYS injects into the system message.
///
/// The runner owns the `final_answer` tool, so it owns the contract for using
/// it. Without a persistent, authoritative statement of "how you finish," a
/// model deep in a multi-turn tool loop never calls `final_answer` and burns
/// the whole turn budget → `AGENT_NO_RESULT` — model-independently, because the
/// only other cue (a one-shot line at the end of the goal `user` message) scrolls
/// out of attention once turn-N's input becomes tool results. Stating it in the
/// SYSTEM message keeps it in force across every turn. Auto-drive still appends
/// the specific required output keys to the goal; this is the general contract.
pub const COMPLETION_PROTOCOL: &str = "\
You are an autonomous agent. Work toward the goal using the tools provided, one \
step at a time. When — and only when — the task is complete, you MUST end the \
session by calling the `final_answer` tool exactly once with an object: \
`{\"status\": \"success\", \"output\": { ... }}`, where `output` carries the \
result. If you determine the task cannot be completed, call `final_answer` with \
`{\"status\": \"failed\", ...}` and explain why in `internal_monologue`. Do not \
finish with a plain text message — an answer that does not go through \
`final_answer` is not recorded and the run fails.";

/// Compose the effective system message: the always-on [`COMPLETION_PROTOCOL`]
/// followed by the session's skill-derived system prompt (when present). The
/// protocol leads so it is never buried beneath a long skill body.
fn compose_system_message(skills: &Option<String>) -> String {
    match skills {
        Some(s) if !s.trim().is_empty() => format!("{COMPLETION_PROTOCOL}\n\n{s}"),
        _ => COMPLETION_PROTOCOL.to_string(),
    }
}

/// Map an MCP tool name to one the provider accepts (`^[a-zA-Z0-9_-]{1,128}$`):
/// every other character becomes `_`, capped at 128. Uniqueness is preserved
/// against names already chosen this run (and the reserved `final_answer`) by
/// appending `_2`, `_3`, … so two real names that sanitize to the same string
/// never collide and shadow each other's routing. `taken` maps an already-chosen
/// exposed name to its `(connection, real_name)`.
fn sanitize_tool_name(
    real: &str,
    taken: &std::collections::HashMap<String, (String, String)>,
) -> String {
    let mut s: String = real
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '_' || c == '-' {
                c
            } else {
                '_'
            }
        })
        .collect();
    if s.is_empty() {
        s.push_str("tool");
    }
    s.truncate(128); // safe: `s` is pure ASCII, so 128 is a char boundary
    let is_taken =
        |n: &str| crate::tool_budget::RESERVED_TOOL_NAMES.contains(&n) || taken.contains_key(n);
    if !is_taken(&s) {
        return s;
    }
    // Disambiguate: trim to make room for the suffix, then append `_N`.
    for i in 2.. {
        let suffix = format!("_{i}");
        let mut candidate = s.clone();
        candidate.truncate(128 - suffix.len());
        candidate.push_str(&suffix);
        if !is_taken(&candidate) {
            return candidate;
        }
    }
    unreachable!("a free suffix always exists below the 128-name space")
}

/// Attempt to recover a usable result from a model turn that answered in text
/// instead of calling `final_answer` (B). Leniency is *earned*: we accept the
/// text only if a JSON object can be parsed out of it AND it meets the criteria
/// (`expected_keys` all present). Handles both the bare `output` object and a
/// full `{status, output}` envelope.
fn salvage_result(
    text: &str,
    expected_keys: &[String],
    expected_types: &BTreeMap<String, String>,
) -> Option<AgentResult> {
    let value = extract_json_object(text)?;
    // Envelope shape `{status, output}` — parse and validate its `output`.
    if value.get("status").is_some() && value.get("output").is_some() {
        if let Ok(r) = serde_json::from_value::<AgentResult>(value.clone()) {
            if conforms(&r.output, expected_keys, expected_types) {
                return Some(r);
            }
        }
    }
    // Otherwise treat the object itself as the `output`.
    if conforms(&value, expected_keys, expected_types) {
        return Some(AgentResult {
            status: AgentStatus::Success,
            output: value,
            internal_monologue: None,
        });
    }
    None
}

/// A candidate `output` conforms iff it is a JSON object that (a) contains every
/// expected top-level key and (b) for every present key with a DECLARED type,
/// the value matches that type. An empty `expected` requires only object-ness;
/// a key without a declared type is not type-checked. Enforcing the type here
/// (and at the `final_answer` boundary) means a non-deterministic agent that
/// returns the right keys with the wrong type is re-prompted in-session rather
/// than failing the post-run snippet contract and wasting the whole run.
fn conforms(output: &Value, expected: &[String], types: &BTreeMap<String, String>) -> bool {
    let Some(obj) = output.as_object() else {
        return false;
    };
    if !expected.iter().all(|k| obj.contains_key(k)) {
        return false;
    }
    types.iter().all(|(k, ty)| match obj.get(k) {
        Some(v) => json_type_matches(v, ty),
        None => true, // absence is covered by the key-presence check above
    })
}

/// Does `v`'s JSON type satisfy a JSON-Schema `type` token? An unknown token is
/// treated as "don't enforce" (returns true) — we only constrain types we model.
fn json_type_matches(v: &Value, ty: &str) -> bool {
    match ty {
        "string" => v.is_string(),
        "object" => v.is_object(),
        "array" => v.is_array(),
        "boolean" => v.is_boolean(),
        "integer" => v.is_i64() || v.is_u64(),
        "number" => v.is_number(),
        "null" => v.is_null(),
        _ => true,
    }
}

/// Pull the first JSON object out of free-form model text: try the whole
/// (trimmed) string first, then the widest `{ … }` span (which also peels code
/// fences / surrounding prose). Returns only `Value::Object`s.
fn extract_json_object(text: &str) -> Option<Value> {
    let trimmed = text.trim();
    if let Ok(v) = serde_json::from_str::<Value>(trimmed) {
        if v.is_object() {
            return Some(v);
        }
    }
    let start = trimmed.find('{')?;
    let end = trimmed.rfind('}')?;
    if end <= start {
        return None;
    }
    serde_json::from_str::<Value>(&trimmed[start..=end])
        .ok()
        .filter(Value::is_object)
}

/// The in-session nudge when a turn produced no usable result: point the model
/// back at the `final_answer` contract, naming the required keys when known.
fn conformance_feedback(
    expected_keys: &[String],
    expected_types: &BTreeMap<String, String>,
) -> String {
    if expected_keys.is_empty() && expected_types.is_empty() {
        "Your last message was not a usable result. Call the `final_answer` tool with a \
         JSON `output` object — do not answer in prose or code fences."
            .to_string()
    } else {
        // Annotate each key with its required type so the model fixes a
        // wrong-type value (e.g. an object where a string is required).
        let spec = expected_keys
            .iter()
            .map(|k| match expected_types.get(k) {
                Some(ty) => format!("{k} ({ty})"),
                None => k.clone(),
            })
            .collect::<Vec<_>>()
            .join(", ");
        format!(
            "Your last message was not a usable result. Call the `final_answer` tool now, with \
             an `output` object containing these keys (each value must match its stated type): \
             {spec}. Do not answer in prose or code fences."
        )
    }
}

pub struct RigSessionRunner {
    // pub(crate): the builder/`final_answer_tool` impl block lives in `tool_budget`
    // after the StructureOS decomposition, so these fields are now cross-module.
    pub(crate) factory: Arc<dyn ProviderFactory>,
    pub(crate) tool_host: Option<Arc<dyn ToolHost>>,
    pub(crate) max_turns: u32,
    pub(crate) max_history_bytes: usize,
}

/// One drained turn's salient content.
struct TurnResult {
    text: String,
    tool_calls: Vec<praxec_llm_executor::ToolCallRequest>,
    final_answer: Option<AgentResult>,
    /// Token usage the provider reported for this turn (zeroed when absent —
    /// usage is best-effort, never a failure).
    usage: TurnUsage,
    /// The transcript content produced by this turn (streamed text + tool
    /// markers).  The caller appends it to the shared transcript after a
    /// successful call, keeping `drain_turn` a pure function that can be
    /// wrapped in a retry without holding `&mut transcript`.
    transcript_fragment: String,
}

/// Prompt + completion tokens for one turn — folded across the loop into the
/// run's realized totals. A separate small type so the accumulation is unit
/// testable without driving the whole loop.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
struct TurnUsage {
    prompt_tokens: u64,
    completion_tokens: u64,
}

impl std::ops::Add for TurnUsage {
    type Output = TurnUsage;
    fn add(self, rhs: TurnUsage) -> TurnUsage {
        TurnUsage {
            prompt_tokens: self.prompt_tokens + rhs.prompt_tokens,
            completion_tokens: self.completion_tokens + rhs.completion_tokens,
        }
    }
}

/// Sum the per-turn usages into the run total. Folds with `Default` zeros so a
/// turn that reported no usage contributes nothing rather than poisoning the
/// total — the same fold the run loop applies turn-by-turn via `TurnUsage::add`,
/// pinned here as a unit-testable helper.
#[cfg(test)]
fn accumulate_usage(turns: impl IntoIterator<Item = TurnUsage>) -> TurnUsage {
    turns
        .into_iter()
        .fold(TurnUsage::default(), |acc, u| acc + u)
}

#[async_trait]
impl AgentSessionRunner for RigSessionRunner {
    async fn run(&self, session: AgentSession) -> Result<AgentRunReport, ExecutorError> {
        // Poka-yoke: declared tools with no host can't be honored — fail fast
        // rather than running the agent silently toolless.
        if !session.tools.is_empty() && self.tool_host.is_none() {
            return Err(ExecutorError::Permanent(format!(
                "RIG_TOOLS_UNSUPPORTED: session declares tools {:?} but no ToolHost is wired; \
                 provide a tool host or select the aether runner.",
                session.tools
            )));
        }

        // List the session's tools once; the runner keeps the per-run
        // tool→connection map (so `call` can route, and the shared host stays
        // stateless across concurrent agents).
        // Per-run map from the *sanitized* tool name the model sees to the
        // (connection, real tool name) needed to route the call back. MCP tool
        // names routinely contain characters (e.g. the `.` in `plan.submit`)
        // that providers reject — Anthropic/Google require
        // `^[a-zA-Z0-9_-]{1,128}$` — so we expose a sanitized name and translate
        // back on invocation (AGENTS — without this, every tool-using agent
        // 400s before it can act).
        let mut base_tools: Vec<ToolDefinition> = Vec::new();
        let mut tool_conn: std::collections::HashMap<String, (String, String)> =
            std::collections::HashMap::new();
        if let Some(host) = &self.tool_host {
            // Fail-fast: an unreachable declared connection aborts the run rather
            // than running the agent without a tool it was told it has.
            let listed = tokio::time::timeout(TOOL_SETUP_TIMEOUT, host.tools(&session.tools))
                .await
                .map_err(|_| ExecutorError::Timeout(TOOL_SETUP_TIMEOUT.as_secs()))??;
            for (mut def, conn) in listed {
                let real = def.name.clone();
                let exposed = sanitize_tool_name(&real, &tool_conn);
                def.name = exposed.clone();
                tool_conn.insert(exposed, (conn, real));
                base_tools.push(def);
            }
        }
        // Poka-yoke: an exposed tool name outside the provider rule
        // (^[a-zA-Z0-9_-]{1,128}$) 400s every turn it appears in. Refuse to start
        // the run rather than emit one — host names are sanitized above, but the
        // runner-injected final_answer / spill_read bypass that, so validate the
        // whole exposed set here. Fail-fast, never a silent provider rejection.
        {
            let injected = [
                Self::final_answer_tool(),
                crate::tool_budget::spill_read_tool(),
            ];
            if let Some(bad) = base_tools
                .iter()
                .chain(injected.iter())
                .find(|d| !crate::tool_budget::is_valid_tool_name(&d.name))
            {
                return Err(ExecutorError::Permanent(format!(
                    "RIG_INVALID_TOOL_NAME: exposed tool {:?} violates the provider rule \
                     ^[a-zA-Z0-9_-]{{1,128}}$ and would 400 every turn; rename it.",
                    bad.name
                )));
            }
        }
        // Per-provider reasoning effort (native `additional_params` shape). Use
        // the step's explicit `reasoning_effort` when set, else the configured
        // default (`ReasoningTuning.default_effort`, "low") so a *reasoning*
        // model can lead a chain without spending the whole turn budget on hidden
        // reasoning. An empty configured default opts out (provider default).
        let effort = session.reasoning_effort.clone().or_else(|| {
            let d = praxec_core::tuning::tuning()
                .reasoning
                .default_effort
                .clone();
            (!d.trim().is_empty()).then_some(d)
        });
        let reasoning = effort.as_deref().and_then(|level| {
            let vendor = session.model.split_once(':').map(|(v, _)| v).unwrap_or("");
            praxec_core::tuning::reasoning_params(vendor, level)
        });

        let mut transcript = String::new();
        let mut history: Vec<Message> = Vec::new();
        // The new message each turn: the goal first, then the tool results. The
        // goal stays in `history` after turn 0 so the agent never loses it, and
        // user/assistant strictly alternate (required by Anthropic et al).
        let mut input: Message = Message::user(session.user_prompt.clone());
        // Realized token usage summed across every turn (incl. the final-answer
        // turn) — surfaced on the report so the audit can price the run. Lives
        // outside `loop_fut` so it survives the loop's early returns/timeout.
        let mut total_usage = TurnUsage::default();
        // Per-run spill store: lives outside loop_fut so it persists across all
        // turns of ONE run but is fresh per run.
        let spill = crate::spill::InMemorySpillStore::new();
        // Per-run recall ledger: one line per elided turn-pair. Rendered onto the
        // OUTGOING prompt each turn (never the cached prefix head) so the goal +
        // frozen turns stay byte-stable — see `prompt_with_ledger`. Grows in
        // `enforce_history_budget`.
        let mut ledger: Vec<String> = Vec::new();

        // The runner-owned completion protocol rides in the system message every
        // turn (see COMPLETION_PROTOCOL) — the structural fix for AGENT_NO_RESULT.
        let system_message = compose_system_message(&session.system_prompt);

        let loop_fut = async {
            // AGENT_NO_RESULT poka-yoke state: set when a turn produces neither a
            // final_answer nor a tool call (a text-only stall — the deepseek/glm
            // signature). Forces the NEXT turn to offer ONLY `final_answer`,
            // removing every other tool so the model's sole remaining move is to
            // terminate with a result instead of exiting empty. The
            // COMPLETION_PROTOCOL system message only *asks*; restricting the
            // toolset *steers*.
            //
            // We deliberately do NOT also pin `tool_choice = Required`: thinking-
            // mode models on OpenRouter (qwen3-thinking, glm-5.2) REJECT a forced
            // tool_choice with a HARD 400 ("tool_choice ... does not support being
            // set to required or object in thinking mode") — which bricked the very
            // models that reliably terminate (and hot-looped the retry layer). No
            // tool_choice value both forces a tool AND is accepted in thinking mode
            // (only Auto/None are accepted, neither forces), so the single-tool
            // restriction is the portable steer; the cost-ascending failover chain
            // remains the ultimate termination guarantee.
            let mut stalled_no_progress = false;
            for _turn in 0..self.max_turns {
                // LoopGuard: bound the cumulative history BEFORE it is re-sent, so
                // a long tool-using loop can never assemble an over-context-window
                // request (the 2.77M-token failure mode). Elides oldest turn-pairs.
                enforce_history_budget(&mut history, self.max_history_bytes, &spill, &mut ledger)
                    .await;
                // Restrict to `final_answer` on the LAST turn (must terminate now)
                // or immediately after a stall (fail-fast to an answer instead of
                // burning the whole turn budget re-prompting). `tool_choice` stays
                // at the provider default (Auto) — see the note above.
                let force_final = _turn + 1 == self.max_turns || stalled_no_progress;
                // Tool-set stability invariant (prompt-cache poka-yoke): the
                // normal-path tool set — `base_tools` (assembled ONCE before the
                // loop) + `final_answer` + `spill_read` — is byte-identical every
                // turn, so it stays in the cacheable prefix. `force_final` shrinking
                // to just `final_answer` is the ONLY deliberate variance, and it is
                // terminal-only (last turn / post-stall), so there is no future turn
                // whose cache it could break. Adding per-turn variance to the
                // else-branch would silently invalidate the prefix — don't.
                let tools = if force_final {
                    vec![Self::final_answer_tool()]
                } else {
                    let mut t = base_tools.clone();
                    t.push(Self::final_answer_tool());
                    t.push(crate::tool_budget::spill_read_tool());
                    t
                };
                let turn = TurnRequest {
                    system: Some(system_message.clone()),
                    // The recall ledger rides the OUTGOING prompt only — never the
                    // cached prefix head (system + goal + frozen turns) and never
                    // `history` (`input` is pushed bare below). No-op until the
                    // first elision.
                    prompt: crate::tool_budget::prompt_with_ledger(&input, &ledger),
                    tools,
                    history: history.clone(),
                    reasoning: reasoning.clone(),
                    tool_choice: None,
                };

                // Same-model RETRY on transient failures (execution-policy)
                // BEFORE the executor chain-walk escalates to another model: a
                // transient blip (timeout/connection/rate-limit) is retried up to
                // 3x with backoff; capability/permanent errors are NOT retried
                // (is_transient false) so they fall through to the chain-walk.
                // Bounded same-model RETRY on transient failures, with
                // exponential backoff, BEFORE the executor chain-walk escalates
                // to another model. Only transient classes retry (is_transient);
                // capability/auth/permanent return immediately so the chain-walk
                // handles them. (The execution-policy crate's AsyncFnMut Send
                // bound conflicted with this borrow-heavy run loop — a hand-rolled
                // loop is the same reliability pattern without the HRTB friction.)
                const MAX_RETRY_ATTEMPTS: u32 = 2;
                let mut retry_attempt: u32 = 0;
                let result = loop {
                    match drain_turn(
                        &*self.factory,
                        &session.model,
                        turn.clone(),
                        session.stall_timeout,
                    )
                    .await
                    {
                        Ok(r) => break r,
                        // A stall (the only `Timeout` drain_turn can raise) is NOT
                        // re-run on the same model — re-issuing a model that just
                        // went silent would burn another stall window for nothing;
                        // it falls through to the chain-walk, which escalates to the
                        // next model. Genuine transient blips (connection / 503 /
                        // rate-limit) still retry in-session with backoff.
                        Err(e)
                            if is_transient(&e)
                                && !matches!(e.class(), ErrorClass::Timeout)
                                && retry_attempt < MAX_RETRY_ATTEMPTS =>
                        {
                            retry_attempt += 1;
                            tokio::time::sleep(Duration::from_millis(
                                200u64 * (1 << retry_attempt),
                            ))
                            .await;
                            continue;
                        }
                        Err(e) => return Err(e),
                    }
                };
                total_usage = total_usage + result.usage;
                transcript.push_str(&result.transcript_fragment);
                // Text-only stall? (no final_answer AND no tool call) — read BEFORE
                // `result.final_answer` is moved below; drives `force_final` next turn.
                stalled_no_progress = result.final_answer.is_none() && result.tool_calls.is_empty();
                if let Some(answer) = result.final_answer {
                    // Enforce the output CONTRACT (keys + declared types) at the
                    // boundary. A conforming answer is accepted; a non-conforming
                    // one (missing key OR wrong type) is RE-PROMPTED in-session
                    // rather than returned — otherwise a wrong-type-but-right-keys
                    // answer would be accepted here only to fail the post-run
                    // snippet contract, wasting the whole (expensive) run.
                    if conforms(
                        &answer.output,
                        &session.expected_output_keys,
                        &session.expected_output_types,
                    ) {
                        return Ok(Some(answer));
                    }
                    transcript.push_str("\n[non-conforming final_answer → contract feedback]\n");
                    history.push(input.clone());
                    history.push(Message::Assistant {
                        id: None,
                        content: OneOrMany::one(AssistantContent::text(
                            serde_json::to_string(&answer.output).unwrap_or_default(),
                        )),
                    });
                    input = Message::user(conformance_feedback(
                        &session.expected_output_keys,
                        &session.expected_output_types,
                    ));
                    continue;
                }
                if result.tool_calls.is_empty() {
                    // The model answered without calling `final_answer`. (B) Try
                    // to salvage a conformant JSON object from its text — accept
                    // it only if it parses and meets the criteria.
                    if let Some(answer) = salvage_result(
                        &result.text,
                        &session.expected_output_keys,
                        &session.expected_output_types,
                    ) {
                        transcript.push_str("\n[salvaged-text-answer]\n");
                        return Ok(Some(answer));
                    }
                    // Not conforming. Rather than give up on the first miss,
                    // feed the contract back and retry in the same session
                    // (bounded by `max_turns`). Keep strict user/assistant
                    // alternation while doing so.
                    transcript.push_str("\n[non-conforming answer → contract feedback]\n");
                    history.push(input.clone());
                    let said = if result.text.is_empty() {
                        "(no content)".to_string()
                    } else {
                        result.text.clone()
                    };
                    history.push(Message::Assistant {
                        id: None,
                        content: OneOrMany::one(AssistantContent::text(said)),
                    });
                    input = Message::user(conformance_feedback(
                        &session.expected_output_keys,
                        &session.expected_output_types,
                    ));
                    continue;
                }
                let host = self.tool_host.as_ref().ok_or_else(|| {
                    ExecutorError::Permanent(
                        "RIG_TOOLS_UNSUPPORTED: model called a tool but no ToolHost is wired"
                            .to_string(),
                    )
                })?;

                // Record this turn: the input (user) + the assistant's text +
                // tool calls.
                history.push(input.clone());
                let mut assistant: Vec<AssistantContent> = Vec::new();
                if !result.text.is_empty() {
                    assistant.push(AssistantContent::text(result.text.clone()));
                }
                for c in &result.tool_calls {
                    let args = serde_json::from_str(&c.arguments).unwrap_or_else(|_| json!({}));
                    assistant.push(AssistantContent::tool_call(
                        c.id.clone(),
                        c.name.clone(),
                        args,
                    ));
                }
                history.push(Message::Assistant {
                    id: None,
                    content: OneOrMany::many(assistant).expect("at least one tool call"),
                });

                // Execute the tools; their results become the next user input
                // (one ToolResult per call), keeping strict alternation.
                let mut results: Vec<UserContent> = Vec::new();
                for c in &result.tool_calls {
                    let out = if c.name == "spill_read" {
                        crate::tool_budget::read_spill(&spill, &c.arguments).await
                    } else {
                        let raw = match tool_conn.get(&c.name) {
                            Some((conn, real)) => host
                                .call(conn, real, &c.arguments)
                                .await
                                .unwrap_or_else(|e| format!("ERROR: {e}")),
                            None => format!("ERROR: unknown tool '{}'", c.name),
                        };
                        // Guard the context window: an unbounded tool result would
                        // 400 every subsequent turn (FM — observed live at 1.86M
                        // tokens from a filesystem dump). Spill oversized results
                        // to the per-run store; the model sees a compact handle
                        // it can page through via spill.read.
                        crate::tool_budget::spill_on_ingress(&spill, &c.name, raw).await
                    };
                    transcript.push_str(&format!("\n[tool {}] {out}\n", c.name));
                    results.push(UserContent::ToolResult(ToolResult {
                        id: c.id.clone(),
                        call_id: None,
                        content: OneOrMany::one(ToolResultContent::text(out)),
                    }));
                }
                input = Message::User {
                    content: OneOrMany::many(results).expect("at least one tool result"),
                };
            }
            Ok::<Option<AgentResult>, ExecutorError>(None) // turn budget exhausted
        };

        let outcome = match tokio::time::timeout(session.timeout, loop_fut).await {
            Err(_) => AgentRunOutcome::TimedOut,
            Ok(Ok(Some(answer))) => AgentRunOutcome::Completed(answer),
            Ok(Ok(None)) => AgentRunOutcome::NoResult,
            // A typed failure (malformed final_answer, provider Error, …) must
            // NOT be downgraded to NoResult — propagate it so the step fails with
            // the real cause (AGENTS-02/AGENTS-03), not a silent empty result.
            Ok(Err(e)) => {
                transcript.push_str(&format!("\n[loop-error] {e:?}\n"));
                return Err(e);
            }
        };

        Ok(AgentRunReport {
            outcome,
            transcript,
            model: session.model.clone(),
            prompt_tokens: total_usage.prompt_tokens,
            completion_tokens: total_usage.completion_tokens,
        })
    }
}

/// Drain one streamed turn: accumulate text, capture tool calls, and parse a
/// `final_answer` envelope if present.
async fn drain_turn(
    factory: &dyn ProviderFactory,
    model: &str,
    turn: TurnRequest,
    stall_timeout: Duration,
) -> Result<TurnResult, ExecutorError> {
    let mut stream = factory
        .stream(model, turn)
        .await
        .map_err(|e| permanent(AgentErrorCode::ProviderError, e))?;
    let mut text = String::new();
    let mut tool_calls = Vec::new();
    let mut final_answer = None;
    let mut usage = TurnUsage::default();
    let mut local_transcript = String::new();
    // No-progress (stall) watchdog: bound the silence between stream events, not
    // just the whole-run wall. ANY event (thinking/text/tool-call/usage) resets
    // the window — so a streaming-but-slow model runs unbounded while a model
    // that hangs at first token is caught in `stall_timeout` and surfaces as a
    // Timeout the chain-walk escalates (rather than burning the full session
    // budget on dead air). `None` from the timeout = the stream went silent.
    loop {
        let item = match tokio::time::timeout(stall_timeout, stream.next()).await {
            Ok(Some(item)) => item,
            Ok(None) => break, // stream ended normally
            Err(_) => {
                local_transcript.push_str(&format!(
                    "\n[stalled] no stream event for {}s — escalating\n",
                    stall_timeout.as_secs()
                ));
                return Err(ExecutorError::Timeout(stall_timeout.as_secs()));
            }
        };
        match item {
            Ok(StreamEvent::Text { chunk }) => {
                local_transcript.push_str(&chunk);
                text.push_str(&chunk);
            }
            Ok(StreamEvent::ToolCall(c)) if c.name == "final_answer" => {
                local_transcript.push_str(&format!("\n[final_answer] {}\n", c.arguments));
                // AGENTS-02: the result-terminating tool call must yield a
                // result. A well-formed envelope is taken as-is; a payload that
                // omits the `{status, output}` envelope (e.g. the model emits the
                // output content at top level, missing `status` —
                // AGENT_MALFORMED_RESULT) is SALVAGED, not hard-failed: wrap it as
                // Success + output (the payload's `output` field if present, else
                // the whole payload). A still-non-conforming salvage is NOT
                // accepted blindly — it flows to the runner's conformance
                // re-prompt. Only a payload that isn't JSON at all is left
                // unparsed, so the runner re-prompts via the no-final_answer path
                // (bounded by `max_turns`). This composes with the single-tool
                // restriction on the forced turn: restricting to `final_answer`
                // steers the model to *answer*; this tolerates an answer that
                // skips the envelope.
                if final_answer.is_none() {
                    match serde_json::from_str::<AgentResult>(&c.arguments) {
                        Ok(parsed) => final_answer = Some(parsed),
                        // RECOVERABLE: valid JSON, just not the {status, output}
                        // envelope (the model emitted the output content at top
                        // level, omitting `status` — AGENT_MALFORMED_RESULT).
                        // Salvage as Success + output (the payload's `output`
                        // field if present, else the whole payload) instead of
                        // discarding the result. A still-non-conforming salvage is
                        // NOT accepted blindly — it flows to the runner's
                        // conformance re-prompt.
                        Err(_) => match serde_json::from_str::<Value>(&c.arguments) {
                            Ok(v) => {
                                let output = v.get("output").cloned().unwrap_or(v);
                                final_answer = Some(AgentResult {
                                    status: AgentStatus::Success,
                                    output,
                                    internal_monologue: None,
                                });
                                local_transcript.push_str(
                                    "\n[salvaged final_answer: missing envelope status → wrapped]\n",
                                );
                            }
                            // UNRECOVERABLE: not JSON at all. Fail-fast LOUD
                            // (AGENTS-02) — never a hollow NoResult that hides the
                            // terminating call's failure.
                            Err(e) => {
                                return Err(permanent(
                                    AgentErrorCode::MalformedResult,
                                    format!("final_answer payload is not JSON: {e}"),
                                ));
                            }
                        },
                    }
                }
            }
            Ok(StreamEvent::ToolCall(c)) => tool_calls.push(c),
            // Realized token usage for this turn (cost telemetry). The rig
            // provider factory emits this from the response aggregate; a
            // provider that reports none simply never sends the event and the
            // turn contributes zero — never a failure.
            Ok(StreamEvent::Usage(u)) => {
                usage.prompt_tokens += u.input_tokens;
                usage.completion_tokens += u.output_tokens;
            }
            // AGENTS-03: a provider-side Error event (rate-limit/503/auth) is a
            // real failure — propagate it as a typed ProviderError (mirrors the
            // llm-executor treating StreamEvent::Error as ProviderError), rather
            // than burying it in the transcript and yielding a hollow NoResult.
            Ok(StreamEvent::Error { message }) => {
                local_transcript.push_str(&format!("\n[error] {message}\n"));
                return Err(permanent(AgentErrorCode::ProviderError, message));
            }
            Ok(_) => {}
            // A per-item transport error (defensible to fold): note it and keep
            // draining — distinct from a provider-emitted Error event above.
            Err(e) => local_transcript.push_str(&format!("\n[stream-error] {e}\n")),
        }
    }
    Ok(TurnResult {
        text,
        tool_calls,
        final_answer,
        usage,
        transcript_fragment: local_transcript,
    })
}

#[cfg(test)]
mod tests;
