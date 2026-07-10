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

use std::collections::{BTreeMap, HashMap};
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use futures::StreamExt;
use praxec_core::audit::{AuditEvent, AuditSink};
use praxec_core::error::{ErrorClass, ExecutorError};
use praxec_core::model::ParkedAgentSession;
use praxec_core::ports::ParkedSessionStore;

use crate::error::{AgentErrorCode, permanent};
use crate::park::{
    AWAIT_HUMAN_TOOL, ParkedConversation, PendingToolResult, await_prompt_from_args,
};
use praxec_llm_executor::{ProviderFactory, StreamEvent, ToolCallRequest, TurnRequest};
use rig::OneOrMany;
use rig::completion::{AssistantContent, Message, ToolDefinition};
use rig::message::{ToolResult, ToolResultContent, UserContent};
use serde_json::{Value, json};

use crate::session::{
    AgentResult, AgentRunOutcome, AgentRunReport, AgentSession, AgentSessionRunner, AgentStatus,
    AgentSuspension, RunIdentity,
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

/// The audit `event_type` of the in-run liveness pulse — see [`HeartbeatEmitter`].
pub const AGENT_HEARTBEAT_EVENT: &str = "agent.heartbeat";

/// How often a long in-flight model call pulses a within-turn
/// [`AGENT_HEARTBEAT_EVENT`] while the stream is silent, so an operator tailing
/// the audit log can tell "waiting on a slow reasoning model" (pulses) from
/// "hung" (silence) BEFORE the stall watchdog fires. Also bounds heartbeat
/// volume: a turn emits at most one pulse per interval, never per-token.
// TODO: surface in config
pub const HEARTBEAT_INTERVAL: Duration = Duration::from_secs(30);

/// Periodic, externally-observable liveness pulse for a governed agent run.
///
/// From OUTSIDE a headless drive, "waiting on a slow reasoning-model call"
/// (benign, 0 CPU) and "hung" (also 0 CPU) look identical when the audit
/// stream only carries the run's boundaries (`agent.invoked` /
/// `agent.completed`). This emitter closes that gap with `agent.heartbeat`
/// events on the SAME audit sink: one at every tool-loop turn boundary
/// (`phase: "turn"`), plus one every [`HEARTBEAT_INTERVAL`] while a single
/// long model call is in flight (`phase: "waiting_on_model"`). Emit-only —
/// a sink failure never affects the run.
pub(crate) struct HeartbeatEmitter {
    sink: Arc<dyn AuditSink>,
    identity: RunIdentity,
    model: String,
    /// tokio's `Instant` (not std's) so paused-clock tests advance it.
    run_started: tokio::time::Instant,
}

impl HeartbeatEmitter {
    fn new(sink: Arc<dyn AuditSink>, session: &AgentSession) -> Self {
        Self {
            sink,
            identity: session.identity.clone(),
            model: session.model.clone(),
            run_started: tokio::time::Instant::now(),
        }
    }

    /// Record one `agent.heartbeat`. Infallible by design: the heartbeat is
    /// pure observability, so a sink error is dropped rather than failing (or
    /// retrying inside) the run it narrates.
    async fn emit(&self, turn: u32, phase: &str, seconds_since_last_output: u64) {
        let mut event = AuditEvent::new(AGENT_HEARTBEAT_EVENT).with_payload(json!({
            "turn": turn,
            "phase": phase,
            "model": self.model,
            "elapsed_ms": self.run_started.elapsed().as_millis() as u64,
            "seconds_since_last_output": seconds_since_last_output,
            "transition": self.identity.transition,
        }));
        if let Some(w) = &self.identity.workflow_id {
            event = event.with_workflow(w.clone());
        }
        if let Some(c) = &self.identity.correlation_id {
            event = event.with_correlation(c.clone());
        }
        let _ = self.sink.record(event).await;
    }
}

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
    /// P12 R1.4 — the durable park for suspended sessions. `None` disables
    /// the await capability entirely (an `await_enabled` session then fails
    /// fast — never a suspend whose conversation can't survive a power cycle).
    pub(crate) parked_store: Option<Arc<dyn ParkedSessionStore>>,
    /// The gateway audit sink the in-run [`HeartbeatEmitter`] records to —
    /// the SAME sink `agent.invoked`/`agent.completed` land in, so an operator
    /// tails one log. `None` (the default) emits no heartbeats; every other
    /// behavior is unchanged.
    pub(crate) audit: Option<Arc<dyn AuditSink>>,
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

/// The per-run environment `drive_loop` reads but never mutates — assembled
/// once by `run` (fresh) or `resume` (rebuilt from the parked frame).
struct LoopEnv<'a> {
    session: &'a AgentSession,
    base_tools: &'a [ToolDefinition],
    /// Sanitized-exposed-name → (connection, real tool name) routing map.
    tool_conn: &'a HashMap<String, (String, String)>,
    reasoning: &'a Option<Value>,
    system_message: &'a str,
    spill: &'a crate::spill::InMemorySpillStore,
    /// The run's liveness pulse (`None` when no audit sink is wired) —
    /// emit-only, threaded so both the per-turn boundary and the within-turn
    /// watchdog tick pulse through one emitter.
    heartbeat: &'a Option<HeartbeatEmitter>,
}

/// How one pass of the tool loop ended (before the runner maps it onto
/// [`AgentRunOutcome`]). A separate enum so `drive_loop` is shared verbatim by
/// `run` and `resume`.
enum LoopEnd {
    /// A conforming `final_answer`.
    Answer(AgentResult),
    /// P12 R1.4 — the suspend signal fired and the frame is ALREADY durably
    /// persisted (park happens before this is returned, so a `Suspended`
    /// outcome is always backed by a recoverable row).
    Suspended(AgentSuspension),
    /// Turn budget exhausted without a result.
    Exhausted,
}

impl RigSessionRunner {
    /// List + sanitize the session's MCP tools and validate the whole exposed
    /// set (host + injected) against the provider name rule. Shared verbatim
    /// by `run` and `resume` so a resumed session rebuilds the exact same tool
    /// surface (sanitization is deterministic over the host's tool list).
    async fn prepare_tools(
        &self,
        session: &AgentSession,
    ) -> Result<(Vec<ToolDefinition>, HashMap<String, (String, String)>), ExecutorError> {
        // Per-run map from the *sanitized* tool name the model sees to the
        // (connection, real tool name) needed to route the call back. MCP tool
        // names routinely contain characters (e.g. the `.` in `plan.submit`)
        // that providers reject — Anthropic/Google require
        // `^[a-zA-Z0-9_-]{1,128}$` — so we expose a sanitized name and translate
        // back on invocation (AGENTS — without this, every tool-using agent
        // 400s before it can act).
        let mut base_tools: Vec<ToolDefinition> = Vec::new();
        let mut tool_conn: HashMap<String, (String, String)> = HashMap::new();
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
        // runner-injected final_answer / spill_read / await_human bypass that, so
        // validate the whole exposed set here. Fail-fast, never a silent provider
        // rejection.
        {
            let injected = [
                Self::final_answer_tool(),
                crate::tool_budget::spill_read_tool(),
                crate::tool_budget::await_human_tool(),
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
        Ok((base_tools, tool_conn))
    }

    /// Per-provider reasoning effort (native `additional_params` shape). Uses
    /// the step's explicit `reasoning_effort` when set, else the configured
    /// default (`ReasoningTuning.default_effort`, "low") so a *reasoning*
    /// model can lead a chain without spending the whole turn budget on hidden
    /// reasoning. An empty configured default opts out (provider default).
    fn reasoning_for(session: &AgentSession) -> Option<Value> {
        let effort = session.reasoning_effort.clone().or_else(|| {
            let d = praxec_core::tuning::tuning()
                .reasoning
                .default_effort
                .clone();
            (!d.trim().is_empty()).then_some(d)
        });
        effort.as_deref().and_then(|level| {
            let vendor = session.model.split_once(':').map(|(v, _)| v).unwrap_or("");
            praxec_core::tuning::reasoning_params(vendor, level)
        })
    }

    /// P12 R1.4 — the park: called when the parked turn's assistant message
    /// (carrying the `await_human` call) is already in `history`. Executes the
    /// turn's OTHER tool calls now — side effects happen exactly once and their
    /// realized results persist with the frame, never re-run on resume — then
    /// persists the whole conversation durably keyed by a fresh
    /// `correlation_id`, and only THEN reports `Suspended`. If persistence
    /// fails the run fails loudly (`AGENT_PARK_STORE`): a suspend the store
    /// didn't accept must never be reported as parked.
    #[allow(clippy::too_many_arguments)]
    async fn park_and_suspend(
        &self,
        env: &LoopEnv<'_>,
        history: &[Message],
        ledger: &[String],
        transcript: &mut String,
        calls: &[ToolCallRequest],
        turns_used: u32,
    ) -> Result<LoopEnd, ExecutorError> {
        let store = self.parked_store.as_ref().ok_or_else(|| {
            // Unreachable: `run` fail-fasts `await_enabled` without a store and
            // the tool is only offered when `await_enabled` — but stay typed.
            permanent(
                AgentErrorCode::AwaitUnsupported,
                "await_human was called but no ParkedSessionStore is wired",
            )
        })?;
        let mut pending: Vec<PendingToolResult> = Vec::with_capacity(calls.len());
        let mut prompt: Option<String> = None;
        for c in calls {
            if c.name == AWAIT_HUMAN_TOOL {
                if prompt.is_none() {
                    // The awaited slot: the human's reply becomes this call's
                    // tool result on resume.
                    prompt = Some(await_prompt_from_args(&c.arguments));
                    pending.push(PendingToolResult {
                        id: c.id.clone(),
                        text: None,
                    });
                } else {
                    // Exactly ONE await per turn is honored (one correlation, one
                    // reply). Additional calls get an immediate error result the
                    // model sees on resume — loud, not silently dropped.
                    let msg = "ERROR: only one await_human call is honored per turn; \
                               this call was not delivered"
                        .to_string();
                    transcript.push_str(&format!("\n[tool await_human] {msg}\n"));
                    pending.push(PendingToolResult {
                        id: c.id.clone(),
                        text: Some(msg),
                    });
                }
                continue;
            }
            // Execute the parked turn's other tools NOW (same dispatch as the
            // normal path, incl. spill-on-ingress) so resume never re-runs a
            // side effect.
            let out = if c.name == "spill_read" {
                crate::tool_budget::read_spill(env.spill, &c.arguments).await
            } else {
                let raw = match (env.tool_conn.get(&c.name), self.tool_host.as_ref()) {
                    (Some((conn, real)), Some(host)) => host
                        .call(conn, real, &c.arguments)
                        .await
                        .unwrap_or_else(|e| format!("ERROR: {e}")),
                    // `tool_conn` non-empty implies a host; keep the arm typed.
                    (Some(_), None) => "ERROR: no ToolHost wired".to_string(),
                    (None, _) => format!("ERROR: unknown tool '{}'", c.name),
                };
                crate::tool_budget::spill_on_ingress(env.spill, &c.name, raw).await
            };
            transcript.push_str(&format!("\n[tool {}] {out}\n", c.name));
            pending.push(PendingToolResult {
                id: c.id.clone(),
                text: Some(out),
            });
        }
        let prompt = prompt.ok_or_else(|| {
            ExecutorError::Permanent(
                "BUG: park_and_suspend called without an await_human call".to_string(),
            )
        })?;
        let correlation_id = uuid::Uuid::new_v4().to_string();
        let conversation = ParkedConversation {
            history: history.to_vec(),
            ledger: ledger.to_vec(),
            pending,
            turns_used,
        };
        let to_value = |what: &str, r: serde_json::Result<Value>| {
            r.map_err(|e| {
                permanent(
                    AgentErrorCode::ParkStore,
                    format!("serializing {what}: {e}"),
                )
            })
        };
        let record = ParkedAgentSession {
            correlation_id: correlation_id.clone(),
            prompt: prompt.clone(),
            session: to_value("session", serde_json::to_value(env.session))?,
            conversation: to_value("conversation", serde_json::to_value(&conversation))?,
            parked_at: chrono::Utc::now(),
        };
        store.park(record).await.map_err(|e| {
            permanent(
                AgentErrorCode::ParkStore,
                format!("persisting parked session {correlation_id}: {e}"),
            )
        })?;
        transcript.push_str(&format!(
            "\n[await_human] parked correlation_id={correlation_id} prompt={prompt:?}\n"
        ));
        Ok(LoopEnd::Suspended(AgentSuspension {
            correlation_id,
            prompt,
        }))
    }

    /// The multi-turn tool loop, shared verbatim by `run` (fresh state,
    /// `start_turn = 0`) and `resume` (state reconstituted from the parked
    /// frame, `start_turn = turns_used`) — so a resumed session continues the
    /// SAME loop, budget included, rather than a re-implementation that would
    /// drift.
    #[allow(clippy::too_many_arguments)]
    async fn drive_loop(
        &self,
        env: &LoopEnv<'_>,
        history: &mut Vec<Message>,
        mut input: Message,
        ledger: &mut Vec<String>,
        transcript: &mut String,
        total_usage: &mut TurnUsage,
        start_turn: u32,
    ) -> Result<LoopEnd, ExecutorError> {
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
        for _turn in start_turn..self.max_turns {
            // Observability: the per-turn liveness pulse. Every turn boundary
            // lands one `agent.heartbeat` in the audit stream, so an operator
            // tailing the log sees turn-by-turn progress instead of silence
            // between `agent.invoked` and `agent.completed`. Emit-only.
            if let Some(hb) = env.heartbeat {
                hb.emit(_turn, "turn", 0).await;
            }
            // LoopGuard: bound the cumulative history BEFORE it is re-sent, so
            // a long tool-using loop can never assemble an over-context-window
            // request (the 2.77M-token failure mode). Elides oldest turn-pairs.
            enforce_history_budget(history, self.max_history_bytes, env.spill, ledger).await;
            // Restrict to `final_answer` on the LAST turn (must terminate now)
            // or immediately after a stall (fail-fast to an answer instead of
            // burning the whole turn budget re-prompting). `tool_choice` stays
            // at the provider default (Auto) — see the note above.
            let force_final = _turn + 1 == self.max_turns || stalled_no_progress;
            // Tool-set stability invariant (prompt-cache poka-yoke): the
            // normal-path tool set — `base_tools` (assembled ONCE before the
            // loop) + `final_answer` + `spill_read` (+ `await_human` iff the
            // session opted in — constant per session, so still stable) — is
            // byte-identical every turn, so it stays in the cacheable prefix.
            // `force_final` shrinking to just `final_answer` is the ONLY
            // deliberate variance, and it is terminal-only (last turn /
            // post-stall), so there is no future turn whose cache it could
            // break. Adding per-turn variance to the else-branch would
            // silently invalidate the prefix — don't.
            let tools = if force_final {
                vec![Self::final_answer_tool()]
            } else {
                let mut t = env.base_tools.to_vec();
                t.push(Self::final_answer_tool());
                t.push(crate::tool_budget::spill_read_tool());
                if env.session.await_enabled {
                    // P12 R1.4 — the suspend signal, offered ONLY on opt-in:
                    // a session that didn't declare `await_enabled` can never
                    // suspend (the tool isn't there to call).
                    t.push(crate::tool_budget::await_human_tool());
                }
                t
            };
            let turn = TurnRequest {
                system: Some(env.system_message.to_string()),
                // The recall ledger rides the OUTGOING prompt only — never the
                // cached prefix head (system + goal + frozen turns) and never
                // `history` (`input` is pushed bare below). No-op until the
                // first elision.
                prompt: crate::tool_budget::prompt_with_ledger(&input, ledger),
                tools,
                history: history.clone(),
                reasoning: env.reasoning.clone(),
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
                    &env.session.model,
                    turn.clone(),
                    env.session.stall_timeout,
                    env.heartbeat.as_ref(),
                    _turn,
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
                        tokio::time::sleep(Duration::from_millis(200u64 * (1 << retry_attempt)))
                            .await;
                        continue;
                    }
                    Err(e) => return Err(e),
                }
            };
            *total_usage = *total_usage + result.usage;
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
                // (A turn that calls BOTH final_answer and await_human takes
                // the answer: the model terminated, so the await is moot.)
                if conforms(
                    &answer.output,
                    &env.session.expected_output_keys,
                    &env.session.expected_output_types,
                ) {
                    return Ok(LoopEnd::Answer(answer));
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
                    &env.session.expected_output_keys,
                    &env.session.expected_output_types,
                ));
                continue;
            }
            if result.tool_calls.is_empty() {
                // The model answered without calling `final_answer`. (B) Try
                // to salvage a conformant JSON object from its text — accept
                // it only if it parses and meets the criteria.
                if let Some(answer) = salvage_result(
                    &result.text,
                    &env.session.expected_output_keys,
                    &env.session.expected_output_types,
                ) {
                    transcript.push_str("\n[salvaged-text-answer]\n");
                    return Ok(LoopEnd::Answer(answer));
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
                    &env.session.expected_output_keys,
                    &env.session.expected_output_types,
                ));
                continue;
            }

            // Record this turn: the input (user) + the assistant's text +
            // tool calls. (Pushed BEFORE dispatch so a park below persists a
            // history that already carries the awaited turn.)
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

            // P12 R1.4 — the suspend signal: the model called `await_human`.
            // STOP the loop, execute the turn's other tools once, persist the
            // conversation durably, and report first-class Suspended.
            // Structurally unreachable without opt-in: the tool is only
            // offered when `await_enabled` (a hallucinated call without
            // opt-in falls through to the unknown-tool error result below).
            if env.session.await_enabled
                && result.tool_calls.iter().any(|c| c.name == AWAIT_HUMAN_TOOL)
            {
                return self
                    .park_and_suspend(
                        env,
                        history,
                        ledger,
                        transcript,
                        &result.tool_calls,
                        _turn + 1,
                    )
                    .await;
            }

            let host = self.tool_host.as_ref().ok_or_else(|| {
                ExecutorError::Permanent(
                    "RIG_TOOLS_UNSUPPORTED: model called a tool but no ToolHost is wired"
                        .to_string(),
                )
            })?;

            // Execute the tools; their results become the next user input
            // (one ToolResult per call), keeping strict alternation.
            let mut results: Vec<UserContent> = Vec::new();
            for c in &result.tool_calls {
                let out = if c.name == "spill_read" {
                    crate::tool_budget::read_spill(env.spill, &c.arguments).await
                } else {
                    let raw = match env.tool_conn.get(&c.name) {
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
                    crate::tool_budget::spill_on_ingress(env.spill, &c.name, raw).await
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
        Ok(LoopEnd::Exhausted) // turn budget exhausted
    }

    /// P12 R1.4 — **resume** a durably parked session: load the frame by
    /// `correlation_id`, inject the human `reply` as the awaited `await_human`
    /// call's tool result, and re-enter the SAME tool loop from the parked
    /// turn, running to a normal outcome (`final_answer` → `Completed`, or
    /// even a further `Suspended` under a fresh correlation_id).
    ///
    /// Frame lifecycle: the row is removed when the resumed segment completes
    /// (`Completed`) or re-suspends (superseded by the new frame). It is KEPT
    /// on `NoResult` / `TimedOut` / error so the resume can be retried — note
    /// the documented trade-off that a retry re-runs the segment's tool calls.
    ///
    /// Report scope: `transcript` and token usage cover THIS resumed segment
    /// only (the suspended run's own report already carried the pre-park
    /// segment; each segment is separately auditable).
    ///
    /// Typed errors, never panics: unknown id → `AGENT_UNKNOWN_CORRELATION`;
    /// unparseable payload → `AGENT_PARKED_SESSION_CORRUPT`; store I/O →
    /// `AGENT_PARK_STORE`.
    pub async fn resume(
        &self,
        correlation_id: &str,
        reply: &str,
    ) -> Result<AgentRunReport, ExecutorError> {
        let store = self.parked_store.as_ref().ok_or_else(|| {
            permanent(
                AgentErrorCode::AwaitUnsupported,
                "resume requires a ParkedSessionStore; none is wired",
            )
        })?;
        let record = store.load(correlation_id).await.map_err(|e| {
            permanent(
                AgentErrorCode::ParkStore,
                format!("loading parked session '{correlation_id}': {e}"),
            )
        })?;
        let Some(record) = record else {
            return Err(permanent(
                AgentErrorCode::UnknownCorrelation,
                format!(
                    "no parked agent session for correlation_id '{correlation_id}' \
                     (already resumed, or never parked)"
                ),
            ));
        };
        let session: AgentSession = serde_json::from_value(record.session).map_err(|e| {
            permanent(
                AgentErrorCode::ParkedSessionCorrupt,
                format!("parked session payload for '{correlation_id}': {e}"),
            )
        })?;
        let parked: ParkedConversation =
            serde_json::from_value(record.conversation).map_err(|e| {
                permanent(
                    AgentErrorCode::ParkedSessionCorrupt,
                    format!("parked conversation payload for '{correlation_id}': {e}"),
                )
            })?;
        // The human's reply becomes the awaited call's tool result; the parked
        // turn's already-realized results ride along (never re-executed).
        let input = parked.resume_input(reply)?;

        // Rebuild the run surface exactly as `run` does. Same fail-fasts.
        if !session.tools.is_empty() && self.tool_host.is_none() {
            return Err(ExecutorError::Permanent(format!(
                "RIG_TOOLS_UNSUPPORTED: parked session declares tools {:?} but no ToolHost \
                 is wired on the resuming runner.",
                session.tools
            )));
        }
        let (base_tools, tool_conn) = self.prepare_tools(&session).await?;
        let reasoning = Self::reasoning_for(&session);
        let system_message = compose_system_message(&session.system_prompt);
        // FIDELITY LIMIT (documented): the spill store is per-process, so
        // `spill_read` handles minted before the park are unreadable after a
        // power cycle — such a read returns the normal "ERROR: unknown slot"
        // tool result and the agent re-runs the tool. The conversation itself
        // (rig messages) is byte-faithful.
        let spill = crate::spill::InMemorySpillStore::new();
        // The resumed segment gets its own heartbeat clock (`elapsed_ms`
        // restarts) — mirroring the report scope: each segment is separately
        // auditable, and the parked wall time (days, maybe) is not "elapsed".
        let heartbeat = self
            .audit
            .as_ref()
            .map(|sink| HeartbeatEmitter::new(sink.clone(), &session));

        let mut history = parked.history;
        let mut ledger = parked.ledger;
        let mut transcript = format!(
            "[resume] correlation_id={correlation_id} reply={reply:?} \
             (turns_used={})\n",
            parked.turns_used
        );
        let mut total_usage = TurnUsage::default();
        // Continue the SAME turn budget; if the session parked on its final
        // budgeted turn, grant exactly one (force-final) turn so the human's
        // reply is never dropped on the floor by an already-spent budget.
        let start_turn = parked.turns_used.min(self.max_turns.saturating_sub(1));

        let env = LoopEnv {
            session: &session,
            base_tools: &base_tools,
            tool_conn: &tool_conn,
            reasoning: &reasoning,
            system_message: &system_message,
            spill: &spill,
            heartbeat: &heartbeat,
        };
        let loop_fut = self.drive_loop(
            &env,
            &mut history,
            input,
            &mut ledger,
            &mut transcript,
            &mut total_usage,
            start_turn,
        );
        // A fresh wall-clock window for the resumed segment (the original
        // window elapsed while parked — awaiting a human for days is normal).
        let outcome = match tokio::time::timeout(session.timeout, loop_fut).await {
            Err(_) => AgentRunOutcome::TimedOut,
            Ok(Ok(LoopEnd::Answer(answer))) => AgentRunOutcome::Completed(answer),
            Ok(Ok(LoopEnd::Suspended(s))) => AgentRunOutcome::Suspended(s),
            Ok(Ok(LoopEnd::Exhausted)) => AgentRunOutcome::NoResult,
            Ok(Err(e)) => {
                return Err(e);
            }
        };

        // Frame cleanup: remove on completion or on supersession by a new
        // parked frame; keep on NoResult/TimedOut so the resume is retryable.
        if matches!(
            outcome,
            AgentRunOutcome::Completed(_) | AgentRunOutcome::Suspended(_)
        ) {
            store.remove(correlation_id).await.map_err(|e| {
                permanent(
                    AgentErrorCode::ParkStore,
                    format!("removing resumed parked session '{correlation_id}': {e}"),
                )
            })?;
        }

        Ok(AgentRunReport {
            outcome,
            transcript,
            model: session.model.clone(),
            prompt_tokens: total_usage.prompt_tokens,
            completion_tokens: total_usage.completion_tokens,
        })
    }
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
        // P12 R1.4 poka-yoke: an await-capable session without a durable park
        // can't be honored — a suspend that couldn't persist would lose the
        // conversation, so refuse to start (mirrors RIG_TOOLS_UNSUPPORTED).
        if session.await_enabled && self.parked_store.is_none() {
            return Err(permanent(
                AgentErrorCode::AwaitUnsupported,
                "session declares await_enabled but no ParkedSessionStore is wired; \
                 wire one (with_parked_store) or drop await_enabled",
            ));
        }

        let (base_tools, tool_conn) = self.prepare_tools(&session).await?;
        let reasoning = Self::reasoning_for(&session);

        let mut transcript = String::new();
        let mut history: Vec<Message> = Vec::new();
        // The new message each turn: the goal first, then the tool results. The
        // goal stays in `history` after turn 0 so the agent never loses it, and
        // user/assistant strictly alternate (required by Anthropic et al).
        let input: Message = Message::user(session.user_prompt.clone());
        // Realized token usage summed across every turn (incl. the final-answer
        // turn) — surfaced on the report so the audit can price the run. Lives
        // outside the loop future so it survives the loop's early returns/timeout.
        let mut total_usage = TurnUsage::default();
        // Per-run spill store: persists across all turns of ONE run but is
        // fresh per run.
        let spill = crate::spill::InMemorySpillStore::new();
        // Per-run recall ledger: one line per elided turn-pair. Rendered onto the
        // OUTGOING prompt each turn (never the cached prefix head) so the goal +
        // frozen turns stay byte-stable — see `prompt_with_ledger`. Grows in
        // `enforce_history_budget`.
        let mut ledger: Vec<String> = Vec::new();

        // The runner-owned completion protocol rides in the system message every
        // turn (see COMPLETION_PROTOCOL) — the structural fix for AGENT_NO_RESULT.
        let system_message = compose_system_message(&session.system_prompt);

        // The run's liveness pulse — only when an audit sink is wired.
        let heartbeat = self
            .audit
            .as_ref()
            .map(|sink| HeartbeatEmitter::new(sink.clone(), &session));

        let env = LoopEnv {
            session: &session,
            base_tools: &base_tools,
            tool_conn: &tool_conn,
            reasoning: &reasoning,
            system_message: &system_message,
            spill: &spill,
            heartbeat: &heartbeat,
        };
        let loop_fut = self.drive_loop(
            &env,
            &mut history,
            input,
            &mut ledger,
            &mut transcript,
            &mut total_usage,
            0,
        );

        let outcome = match tokio::time::timeout(session.timeout, loop_fut).await {
            Err(_) => AgentRunOutcome::TimedOut,
            Ok(Ok(LoopEnd::Answer(answer))) => AgentRunOutcome::Completed(answer),
            // P12 R1.4 — first-class suspend: the frame is already durably
            // parked (park-then-report ordering inside `park_and_suspend`).
            Ok(Ok(LoopEnd::Suspended(s))) => AgentRunOutcome::Suspended(s),
            Ok(Ok(LoopEnd::Exhausted)) => AgentRunOutcome::NoResult,
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

    /// P12 R1.4 — the trait surface for the inherent durable resume, so
    /// callers holding `Arc<dyn AgentSessionRunner>` (the agent executor) can
    /// route a human reply back to the parked frame.
    async fn resume(
        &self,
        correlation_id: &str,
        reply: &str,
    ) -> Result<AgentRunReport, ExecutorError> {
        RigSessionRunner::resume(self, correlation_id, reply).await
    }
}

/// Drain one streamed turn: accumulate text, capture tool calls, and parse a
/// `final_answer` envelope if present.
///
/// `heartbeat`/`turn_index`: the run's liveness pulse. While the stream is
/// silent, the stall watchdog's wait is chunked into [`HEARTBEAT_INTERVAL`]
/// ticks; each expired tick that is still inside the stall window emits one
/// within-turn `agent.heartbeat` (`phase: "waiting_on_model"`), so a single
/// slow reasoning call shows a pulse instead of dead air. The stall semantics
/// are unchanged: total silence ≥ `stall_timeout` still raises the same
/// `ExecutorError::Timeout`.
async fn drain_turn(
    factory: &dyn ProviderFactory,
    model: &str,
    turn: TurnRequest,
    stall_timeout: Duration,
    heartbeat: Option<&HeartbeatEmitter>,
    turn_index: u32,
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
    // budget on dead air). `None` from the inner wait = the stream went silent
    // past the whole window. tokio's `Instant` (not std's) so paused-clock
    // tests advance it with the timers.
    let mut last_event = tokio::time::Instant::now();
    'drain: loop {
        // Wait for the next stream event in HEARTBEAT_INTERVAL-bounded ticks
        // (never past the remaining stall window). An expired tick is NOT a
        // stall yet — it pulses the heartbeat and keeps waiting; only total
        // silence ≥ `stall_timeout` escalates.
        let item = 'wait: loop {
            let silent = last_event.elapsed();
            let remaining = stall_timeout.saturating_sub(silent);
            if remaining.is_zero() {
                local_transcript.push_str(&format!(
                    "\n[stalled] no stream event for {}s — escalating\n",
                    stall_timeout.as_secs()
                ));
                return Err(ExecutorError::Timeout(stall_timeout.as_secs()));
            }
            match tokio::time::timeout(remaining.min(HEARTBEAT_INTERVAL), stream.next()).await {
                Ok(Some(item)) => break 'wait item,
                Ok(None) => break 'drain, // stream ended normally
                Err(_) => {
                    // Tick expired, still inside the stall window: pulse (the
                    // 0-CPU "waiting on a slow model" case an observer could
                    // not previously distinguish from a hang), then re-check.
                    let silent = last_event.elapsed();
                    if silent < stall_timeout {
                        if let Some(hb) = heartbeat {
                            hb.emit(turn_index, "waiting_on_model", silent.as_secs())
                                .await;
                        }
                    }
                    continue 'wait;
                }
            }
        };
        last_event = tokio::time::Instant::now();
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
