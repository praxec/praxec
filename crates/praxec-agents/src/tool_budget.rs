use crate::rig_runner::{DEFAULT_MAX_HISTORY_BYTES, DEFAULT_MAX_TURNS, RigSessionRunner};
use crate::spill::SpillStore;
use async_trait::async_trait;
use praxec_core::error::{ErrorClass, ExecutorError};
use praxec_llm_executor::{DefaultProviderFactory, ProviderFactory};
use rig::OneOrMany;
use rig::completion::{AssistantContent, Message, ToolDefinition};
use serde_json::json;
use std::sync::Arc;
/// Exposes + executes the MCP tools an agent may call. **Stateless** by design:
/// `tools` returns each definition paired with its owning connection, and `call`
/// takes that connection explicitly — so a single shared host serves concurrent
/// agents with no per-session state to race on. The production impl lists/calls
/// over the session's declared connections; tests inject a stub.
#[async_trait]
pub trait ToolHost: Send + Sync {
    /// The tools exposed by `connections`, each paired with its connection name
    /// (besides `final_answer`, which the runner adds). A connection that can't
    /// be reached is an **error**, not an empty list: a declared tool the agent
    /// was told it has but silently lacks is a fail-open we refuse (AGENTS / the
    /// no-silent-fallback discipline).
    async fn tools(
        &self,
        connections: &[String],
    ) -> Result<Vec<(ToolDefinition, String)>, ExecutorError>;
    /// Execute `name` on `connection` with JSON `arguments`; return the result
    /// text (or an error string the model sees as the tool's output).
    async fn call(&self, connection: &str, name: &str, arguments: &str) -> Result<String, String>;
}

impl RigSessionRunner {
    pub fn new(factory: Arc<dyn ProviderFactory>) -> Self {
        Self {
            factory,
            tool_host: None,
            max_turns: DEFAULT_MAX_TURNS,
            max_history_bytes: DEFAULT_MAX_HISTORY_BYTES,
            parked_store: None,
            audit: None,
        }
    }

    /// Wire the production rig provider factory (shared with `kind: llm`).
    pub fn with_default_provider() -> Self {
        Self::new(Arc::new(DefaultProviderFactory))
    }

    /// Expose + execute the agent's MCP tools through `host`.
    pub fn with_tool_host(mut self, host: Arc<dyn ToolHost>) -> Self {
        self.tool_host = Some(host);
        self
    }

    /// Override the cumulative-history budget (default
    /// [`DEFAULT_MAX_HISTORY_BYTES`]). Primarily for tests that drive the
    /// elision path without assembling a megabyte of fixture data.
    pub fn with_max_history_bytes(mut self, bytes: usize) -> Self {
        self.max_history_bytes = bytes;
        self
    }

    /// P12 R1.4 — wire the durable [`ParkedSessionStore`] backing suspend /
    /// resume. Required for any session with `await_enabled` (the runner
    /// fails fast without it — a suspend that can't persist would lose the
    /// conversation). The production backend is
    /// `praxec_core::store::SqliteParkedSessionStore`.
    pub fn with_parked_store(
        mut self,
        store: Arc<dyn praxec_core::ports::ParkedSessionStore>,
    ) -> Self {
        self.parked_store = Some(store);
        self
    }

    /// Wire the gateway audit sink so the run emits `agent.heartbeat`
    /// liveness events (per turn boundary + every
    /// [`HEARTBEAT_INTERVAL`](crate::rig_runner::HEARTBEAT_INTERVAL) within a
    /// long silent model call) into the SAME audit log `agent.invoked` /
    /// `agent.completed` land in. Without it (the default) no heartbeats are
    /// emitted; all other behavior is identical.
    pub fn with_audit_sink(mut self, sink: Arc<dyn praxec_core::audit::AuditSink>) -> Self {
        self.audit = Some(sink);
        self
    }

    pub(crate) fn final_answer_tool() -> ToolDefinition {
        ToolDefinition {
            name: "final_answer".to_string(),
            description: "Report the final structured result and end the session. Call this \
                          exactly once, when the task is complete."
                .to_string(),
            parameters: json!({
                "type": "object",
                "required": ["status"],
                "properties": {
                    "status": { "type": "string", "enum": ["success", "failed"] },
                    "output": { "type": "object", "description": "Result projected to the step's output slots." },
                    "internal_monologue": { "type": "string" }
                }
            }),
        }
    }
}

/// Transient (retry-on-same-model) error classes — a blip worth retrying before
/// the executor chain-walk escalates to a different model. Capability/auth/
/// permanent errors are NOT transient (retrying the same model won't help).
pub(crate) fn is_transient(e: &ExecutorError) -> bool {
    matches!(
        e.class(),
        ErrorClass::Timeout
            | ErrorClass::RateLimited
            | ErrorClass::Connection
            | ErrorClass::Transient
    )
}

/// Ceiling on a single tool result's size before it is appended to the
/// conversation. An unbounded MCP result (e.g. a filesystem tool dumping a whole
/// repo) can dwarf the model's context window and hard-fail every subsequent
/// turn with a provider 400; we truncate with a loud marker instead, so the
/// agent sees what happened and can narrow the call. 64 KiB ≈ 16k tokens —
/// generous for real tool output, far below any context budget even across the
/// turn loop.
pub const MAX_TOOL_RESULT_BYTES: usize = 64 * 1024;

/// Map an MCP tool (rmcp's wire `Tool`) to rig's `ToolDefinition` — the agent's
/// external-tool bridge (C5). Lifted out of the binary's `McpToolHost` so the
/// mapping is contract-testable: `name` passes through, a tool with **no
/// description** maps to an empty string, and the JSON-Schema `input_schema`
/// becomes rig's `parameters`.
pub fn tool_definition_from(tool: &rmcp::model::Tool) -> ToolDefinition {
    ToolDefinition {
        name: tool.name.clone().into_owned(),
        description: tool
            .description
            .as_ref()
            .map(|c| c.to_string())
            .unwrap_or_default(),
        parameters: serde_json::Value::Object((*tool.input_schema).clone()),
    }
}

/// Bound a single tool result to [`MAX_TOOL_RESULT_BYTES`] before it re-enters
/// the conversation, appending a marker that tells the model it was cut and how
/// to recover. Truncation lands on a UTF-8 char boundary so we never split a
/// multi-byte sequence.
#[allow(dead_code)]
pub(crate) fn truncate_tool_result(out: String) -> String {
    if out.len() <= MAX_TOOL_RESULT_BYTES {
        return out;
    }
    let mut end = MAX_TOOL_RESULT_BYTES;
    while !out.is_char_boundary(end) {
        end -= 1;
    }
    format!(
        "{}\n\n…[tool result truncated: kept {} of {} bytes — too large for the model context. \
         Re-run the tool with a narrower scope (e.g. a more specific path or query) to get the rest.]",
        &out[..end],
        end,
        out.len()
    )
}

/// Serialized byte-size of the conversation history (what is re-sent every
/// turn). A faithful proxy for the request size the provider sees, so the budget
/// guard measures the same thing the context window limits.
pub(crate) fn history_bytes(history: &[Message]) -> usize {
    history
        .iter()
        .map(|m| serde_json::to_string(m).map(|s| s.len()).unwrap_or(0))
        .sum()
}

/// Tool names the runner injects itself rather than routing to a host
/// (`final_answer`, `spill_read`, `await_human`). A host tool may NOT shadow
/// them, so `sanitize_tool_name` treats these as already-taken and
/// disambiguates a colliding host name — otherwise the injected tool's handler
/// would silently swallow the host call.
pub(crate) const RESERVED_TOOL_NAMES: [&str; 3] = ["final_answer", "spill_read", "await_human"];

/// The provider tool-name rule shared by Anthropic and Google:
/// `^[a-zA-Z0-9_-]{1,128}$`. A name outside it 400s every turn it is offered
/// (this is exactly how a dotted `spill.read` would have failed live), so the
/// runner refuses to expose one — see `run`'s name guard. Poka-yoke: an invalid
/// tool name is made structurally impossible to send rather than caught after a
/// provider rejection.
pub(crate) fn is_valid_tool_name(name: &str) -> bool {
    !name.is_empty()
        && name.len() <= 128
        && name
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b'-')
}

/// The injected `spill_read` tool definition, mirroring how `final_answer` is
/// injected.  Backed by the session [`SpillStore`].
pub(crate) fn spill_read_tool() -> ToolDefinition {
    ToolDefinition {
        name: "spill_read".to_string(),
        description: "Read a byte-range window from a spilled tool result.\n\
             args: { slot: string, range?: [start: number, end: number] } — \
             range defaults to a window after the head already shown."
            .to_string(),
        parameters: json!({
            "type": "object",
            "required": ["slot"],
            "properties": {
                "slot": { "type": "string", "description": "Slot id (from a spilling handle)." },
                "range": {
                    "type": "array",
                    "minItems": 2,
                    "maxItems": 2,
                    "items": { "type": "integer" },
                    "description": "Optional [start, end] byte range."
                }
            }
        }),
    }
}

/// P12 R1.4 — the injected suspend-signal tool, offered ONLY when the session
/// opts in (`await_enabled`). Calling it is the explicit trigger for
/// [`AgentRunOutcome::Suspended`](crate::session::AgentRunOutcome::Suspended):
/// the runner stops the loop, persists the conversation durably, and a human's
/// later correlated reply becomes this call's tool result on resume.
pub(crate) fn await_human_tool() -> ToolDefinition {
    ToolDefinition {
        name: crate::park::AWAIT_HUMAN_TOOL.to_string(),
        description: "Pause this session and ask a human. Call this when you need a decision, \
             approval, or information only a human can provide. The session suspends \
             durably; when the human answers, their reply arrives as this tool's \
             result and you continue from here. At most one await_human call per turn."
            .to_string(),
        parameters: json!({
            "type": "object",
            "required": ["prompt"],
            "properties": {
                "prompt": {
                    "type": "string",
                    "description": "The question or request to put to the human, with enough context to answer it."
                }
            }
        }),
    }
}

/// If `out` exceeds [`MAX_TOOL_RESULT_BYTES`], write it to `store` and return a
/// compact JSON handle; otherwise return `out` unchanged.
pub(crate) async fn spill_on_ingress(
    store: &dyn SpillStore,
    tool_name: &str,
    out: String,
) -> String {
    if out.len() <= MAX_TOOL_RESULT_BYTES {
        return out;
    }
    let bytes = out.len();
    let slot = store.put(out.clone()).await;
    // head: first ~2 KiB, char-boundary safe
    let head_end = 2048_usize.min(bytes);
    let mut he = head_end;
    while he > 0 && !out.is_char_boundary(he) {
        he -= 1;
    }
    let head = &out[..he];
    // cheap model-free summary: bytes + line count + sniffed kind
    let line_count = out.lines().count();
    let kind = if out.trim_start().starts_with('{') || out.trim_start().starts_with('[') {
        "json"
    } else if out.len() > 1 && !out.is_ascii() {
        "binary"
    } else {
        "text"
    };
    let summary = format!("{kind}, {} bytes, {} lines", bytes, line_count);
    // Next window suggestion: skip the head already shown
    let read_start = he;
    let read_end = (read_start + 64 * 1024).min(bytes);
    let handle = json!({
        "spilled": true,
        "tool": tool_name,
        "bytes": bytes,
        "slot": slot,
        "head": head,
        "summary": summary,
        "read": {
            "tool": "spill_read",
            "args": { "slot": slot, "range": [read_start, read_end] }
        }
    });
    handle.to_string()
}

/// Read a byte-range window from a spilled tool result, or an `ERROR: …`
/// string when the slot is unknown or arguments are malformed.
pub(crate) async fn read_spill(store: &dyn SpillStore, arguments: &str) -> String {
    let args: serde_json::Value = match serde_json::from_str(arguments) {
        Ok(v) => v,
        Err(e) => return format!("ERROR: {e}"),
    };
    let slot = match args.get("slot").and_then(|v| v.as_str()) {
        Some(s) => s,
        None => return "ERROR: missing required field 'slot'".to_string(),
    };
    let range = args.get("range").and_then(|v| v.as_array());
    let (start, end) = match range {
        Some(arr) if arr.len() == 2 => {
            let s = arr[0].as_u64().unwrap_or(0) as usize;
            let e = arr[1].as_u64().unwrap_or(MAX_TOOL_RESULT_BYTES as u64) as usize;
            (s, e)
        }
        _ => (0, MAX_TOOL_RESULT_BYTES),
    };
    match store.get(slot, start, end).await {
        Ok(window) => window,
        Err(e) => format!("ERROR: {e}"),
    }
}

/// Messages always kept regardless of budget: the goal (index 0) + roughly the
/// two most-recent turn-pairs, so the agent never loses its objective or its
/// immediate working context to elision.
pub(crate) const HISTORY_KEEP_RECENT_MSGS: usize = 4;

/// A model-free one-line descriptor of an elided turn-pair: the tool calls the
/// assistant made (name + a truncated args snippet) and the byte size of the
/// paired tool-results message. Enough signal for the agent to decide whether
/// to `spill_read` the slot — with no model call (token-frugal by design).
pub(crate) fn elided_descriptor(assistant: &Message, user: &Message) -> String {
    let mut tools: Vec<String> = Vec::new();
    if let Message::Assistant { content, .. } = assistant {
        for c in content.iter() {
            if let AssistantContent::ToolCall(tc) = c {
                let mut args = tc.function.arguments.to_string();
                if args.len() > 48 {
                    let mut end = 48;
                    while !args.is_char_boundary(end) {
                        end -= 1;
                    }
                    args.truncate(end);
                    args.push('…');
                }
                tools.push(format!("{}({})", tc.function.name, args));
            }
        }
    }
    let tools_desc = if tools.is_empty() {
        "(no tool calls)".to_string()
    } else {
        tools.join(", ")
    };
    let bytes = serde_json::to_string(user).map(|s| s.len()).unwrap_or(0);
    format!("tools: {tools_desc} → {bytes} B")
}

/// Header marking the recall-ledger block. The ledger is rendered onto the
/// OUTGOING prompt tail each turn (never onto the cached prefix head) — see
/// [`prompt_with_ledger`].
const LEDGER_HEADER: &str = "\n\n--- recallable elided history (use spill_read with the slot) ---";

/// Render the run's recall ledger as one prompt-tail block: the marker header
/// followed by one line per elided turn. `None` until something has been elided,
/// so the common-path prompt is left untouched.
pub(crate) fn render_recall_ledger(ledger: &[String]) -> Option<String> {
    if ledger.is_empty() {
        return None;
    }
    let mut block = String::from(LEDGER_HEADER);
    for line in ledger {
        block.push('\n');
        block.push_str(line);
    }
    Some(block)
}

/// Attach the recall ledger to the OUTGOING prompt only (never persisted into
/// `history`), so the cached prefix head — system message + goal + already-frozen
/// turns — stays byte-identical across the run while the agent still sees the full
/// current ledger every turn on the churny prompt tail. A no-op clone until the
/// first elision.
pub(crate) fn prompt_with_ledger(input: &Message, ledger: &[String]) -> Message {
    use rig::message::{Text, UserContent};
    let Some(block) = render_recall_ledger(ledger) else {
        return input.clone();
    };
    match input {
        Message::User { content } => {
            let mut items: Vec<UserContent> = content.iter().cloned().collect();
            items.push(UserContent::Text(Text::new(block)));
            match OneOrMany::many(items) {
                Ok(content) => Message::User { content },
                // Unreachable: `items` already holds `input`'s content (≥1).
                Err(_) => input.clone(),
            }
        }
        other => other.clone(),
    }
}

/// LoopGuard for the context window: elide the OLDEST turn-pairs until the
/// re-sent history fits `budget`. History is `[user(goal), assistant₀,
/// user(results₀), assistant₁, user(results₁), …]` (every continuing turn pushes
/// exactly one `[user, assistant]` pair), so removing indices `(1, 2)` takes the
/// oldest assistant (with its `tool_calls`) together with its paired
/// `tool_results` — preserving strict user/assistant alternation AND
/// `tool_call`/`tool_result` pairing. The goal and recent turns are always kept.
/// This is what makes "never send a 2.77M-token request" structurally true: the
/// per-result cap bounds one result, this bounds the cumulative sum across turns.
///
/// Elided pairs are NOT dropped — each is `put()` into the per-run [`SpillStore`]
/// (lossless + addressable) and recorded as a compact model-free line pushed onto
/// the run's `ledger`, which the runner renders onto the OUTGOING prompt tail each
/// turn (never onto the cached prefix head — see [`prompt_with_ledger`]), so the
/// agent can recall any of them on demand via the injected `spill_read` tool
/// (ADR-0012 bounded working set over a lossless store).
pub(crate) async fn enforce_history_budget(
    history: &mut Vec<Message>,
    budget: usize,
    store: &dyn SpillStore,
    ledger: &mut Vec<String>,
) {
    // Hysteresis: only engage once the ceiling is crossed, then drain to a
    // low-water mark (~⅔ budget) in this single pass. Each elision mutates the
    // middle of the re-sent prefix and so invalidates the cache from that point;
    // batching to low-water means the next turn does not immediately re-trigger.
    if history_bytes(history) <= budget {
        return;
    }
    let low_water = budget.saturating_mul(2) / 3;
    while history_bytes(history) > low_water
        && history.len() >= 3
        && history.len() > HISTORY_KEEP_RECENT_MSGS + 1
    {
        let assistant = history.remove(1); // oldest assistant (carries tool_calls)
        let user = history.remove(1); // its paired user (the matching tool_results)
        let descriptor = elided_descriptor(&assistant, &user);
        // Lossless: the whole pair is recoverable from this one slot.
        let payload = serde_json::to_string(&[&assistant, &user]).unwrap_or_default();
        let slot = store.put(payload).await;
        let index = ledger.len() + 1;
        ledger.push(format!(
            "elided #{index} · {descriptor} · recall: spill_read slot={slot}"
        ));
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::spill::MemSpillStore;
    use rig::OneOrMany;
    use serde_json::Value;

    #[tokio::test]
    async fn spill_on_ingress_below_threshold_returns_unchanged() {
        let store = MemSpillStore::new();
        let small = "small result".to_string();
        let out = spill_on_ingress(&store, "lookup", small.clone()).await;
        assert_eq!(out, small);
    }

    #[tokio::test]
    async fn spill_on_ingress_above_threshold_returns_handle() {
        let store = MemSpillStore::new();
        let big = "x".repeat(MAX_TOOL_RESULT_BYTES + 1);
        let out = spill_on_ingress(&store, "lookup", big).await;
        let v: serde_json::Value = serde_json::from_str(&out).expect("handle must be valid JSON");
        assert_eq!(v["spilled"], json!(true));
    }

    #[tokio::test]
    async fn read_spill_returns_the_requested_window() {
        let store = MemSpillStore::new();
        let payload = "The quick brown fox jumps over the lazy dog";
        let slot = store.put(payload.to_string()).await;
        let args = format!(r#"{{"slot":"{}","range":[4,19]}}"#, slot);
        let out = read_spill(&store, &args).await;
        // "quick brown fox" — bytes 4..19 of the payload
        assert_eq!(out, "quick brown fox");
    }

    #[tokio::test]
    async fn read_spill_on_unknown_slot_returns_error() {
        let store = MemSpillStore::new();
        let args = r#"{"slot":"nonexistent","range":[0,100]}"#;
        let out = read_spill(&store, args).await;
        assert!(out.starts_with("ERROR:"));
    }

    #[test]
    fn a_dotted_tool_name_is_rejected_by_the_provider_rule() {
        assert!(!is_valid_tool_name("spill.read"));
    }

    #[test]
    fn the_injected_spill_read_name_is_provider_valid() {
        assert!(is_valid_tool_name(&spill_read_tool().name));
    }

    #[test]
    fn the_injected_final_answer_name_is_provider_valid() {
        assert!(is_valid_tool_name(
            &RigSessionRunner::final_answer_tool().name
        ));
    }

    // ── history-spill ────────────────────────────────────────────────────────

    fn tool_pair(id: &str, args: Value) -> (Message, Message) {
        use rig::message::{ToolResult, ToolResultContent, UserContent};
        let assistant = Message::Assistant {
            id: None,
            content: OneOrMany::one(AssistantContent::tool_call(id, "read_file", args)),
        };
        let user = Message::User {
            content: OneOrMany::one(UserContent::ToolResult(ToolResult {
                id: id.into(),
                call_id: None,
                content: OneOrMany::one(ToolResultContent::text("X".repeat(500))),
            })),
        };
        (assistant, user)
    }

    #[test]
    fn elided_descriptor_names_the_tool_and_args() {
        let (assistant, user) = tool_pair("c1", json!({ "path": "gateway.rs" }));
        let d = elided_descriptor(&assistant, &user);
        assert!(
            d.contains("read_file") && d.contains("gateway.rs"),
            "descriptor must name the tool + args; got: {d}"
        );
    }

    #[test]
    fn render_recall_ledger_is_one_section_with_all_lines() {
        let ledger = vec![
            "elided #1 · tools: a → 5 B · recall: spill_read slot=H1".to_string(),
            "elided #2 · tools: b → 6 B · recall: spill_read slot=H2".to_string(),
        ];
        let block = render_recall_ledger(&ledger).expect("non-empty ledger renders");
        assert_eq!(
            block.matches("--- recallable elided history").count(),
            1,
            "exactly one ledger section header; got:\n{block}"
        );
        assert!(block.contains("slot=H1") && block.contains("slot=H2"));
        assert!(
            render_recall_ledger(&[]).is_none(),
            "an empty ledger renders nothing (common-path prompt untouched)"
        );
    }

    #[test]
    fn prompt_with_ledger_rides_the_prompt_not_the_head() {
        let input = Message::user("do the task");
        // No elision yet → the prompt is byte-identical (no cache churn).
        assert_eq!(
            serde_json::to_string(&prompt_with_ledger(&input, &[])).unwrap(),
            serde_json::to_string(&input).unwrap(),
        );
        // After elision → the ledger rides on the outgoing prompt, goal text kept.
        let ledger = vec!["elided #1 · tools: a → 5 B · recall: spill_read slot=H1".to_string()];
        let with = serde_json::to_string(&prompt_with_ledger(&input, &ledger)).unwrap();
        assert!(with.contains("recallable elided history") && with.contains("slot=H1"));
        assert!(
            with.contains("do the task"),
            "original prompt text preserved"
        );
    }

    fn budget_history(pairs: usize) -> Vec<Message> {
        let mut history = vec![Message::user("GOAL")];
        for i in 0..pairs {
            let (assistant, user) = tool_pair(&format!("c{i}"), json!({ "n": i }));
            history.push(assistant);
            history.push(user);
        }
        history
    }

    #[tokio::test]
    async fn enforce_history_budget_spills_the_oldest_pair_recall_ably() {
        let store = MemSpillStore::new();
        let mut history = budget_history(3); // goal + 3 pairs (7 msgs)
        let mut ledger: Vec<String> = Vec::new();
        enforce_history_budget(&mut history, 300, &store, &mut ledger).await;
        // The ledger names a slot; reading it back yields the exact elided turn.
        let line = ledger.first().expect("a ledger line");
        let slot = line
            .split("slot=")
            .nth(1)
            .and_then(|s| s.split_whitespace().next())
            .expect("a ledger line with a slot");
        let recalled = store.get(slot, 0, 1_000_000).await.unwrap();
        assert!(
            recalled.contains("read_file"),
            "the spilled slot must round-trip to the elided turn; got: {recalled}"
        );
    }

    #[tokio::test]
    async fn enforce_history_budget_keeps_alternation_after_eliding() {
        let store = MemSpillStore::new();
        let mut history = budget_history(3);
        let mut ledger: Vec<String> = Vec::new();
        enforce_history_budget(&mut history, 300, &store, &mut ledger).await;
        let starts_with_user = matches!(history.first(), Some(Message::User { .. }));
        assert!(
            starts_with_user,
            "history must still start with the user goal"
        );
    }

    #[tokio::test]
    async fn enforce_history_budget_never_spills_the_goal_or_recent_turns() {
        let store = MemSpillStore::new();
        let mut history = budget_history(3);
        let mut ledger: Vec<String> = Vec::new();
        // A budget far below any single pair still never strips goal + recent-N:
        // the length guard stops at HISTORY_KEEP_RECENT_MSGS + 1.
        enforce_history_budget(&mut history, 1, &store, &mut ledger).await;
        assert!(
            history.len() > HISTORY_KEEP_RECENT_MSGS,
            "goal + recent-N must survive even an impossibly small budget; len={}",
            history.len()
        );
    }

    #[tokio::test]
    async fn spill_leaves_history_head_byte_identical() {
        // The cache-safety invariant: eliding never mutates history[0] (the goal,
        // the within-run cache anchor). The recall ledger rides the prompt tail.
        let store = MemSpillStore::new();
        let mut history = budget_history(4); // goal + 4 pairs → forces elision at 300
        let head_before = serde_json::to_string(&history[0]).unwrap();
        let mut ledger: Vec<String> = Vec::new();
        enforce_history_budget(&mut history, 300, &store, &mut ledger).await;
        assert!(
            !ledger.is_empty(),
            "precondition: elision must have happened for this to prove anything"
        );
        let head_after = serde_json::to_string(&history[0]).unwrap();
        assert_eq!(
            head_before, head_after,
            "the goal (cache anchor) must be byte-identical after spill"
        );
    }
}
