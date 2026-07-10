//! P12 R1.4 (`docs/await-resume-architecture.md`) — the agent-loop half of the
//! durable await/resume primitive: the **suspend signal** (the reserved
//! `await_human` tool) and the **parked conversation** representation the
//! runner persists to the [`ParkedSessionStore`](praxec_core::ports::ParkedSessionStore)
//! and reconstitutes on a correlated resume.
//!
//! Fidelity: the conversation persists as rig's own `Message` values (they are
//! `Serialize`/`Deserialize`), so the reloaded history is byte-faithful to what
//! the model saw. The ONE lossy edge is the per-run in-memory spill store:
//! `spill_read` handles referenced by a parked conversation point at slots that
//! die with the process, so after a power-cycle resume a `spill_read` on an old
//! slot returns the normal `ERROR: unknown slot` tool result and the agent
//! re-runs the tool. Documented, fail-loud-to-the-model — never a panic.

use rig::OneOrMany;
use rig::completion::Message;
use rig::message::{ToolResult, ToolResultContent, UserContent};
use serde::{Deserialize, Serialize};

use crate::error::{AgentErrorCode, permanent};
use praxec_core::error::ExecutorError;

/// The reserved suspend-signal tool name. Reserved exactly like
/// `final_answer`/`spill_read`: a host tool with this name is disambiguated by
/// `sanitize_tool_name`, so only the runner-injected tool ever carries it.
pub const AWAIT_HUMAN_TOOL: &str = "await_human";

/// One tool result from the parked turn. The awaited `await_human` call is the
/// single entry with `text: None` — the hole the human's reply fills on
/// resume. Every other call already executed at park time (side effects happen
/// exactly once) and carries its realized result.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct PendingToolResult {
    /// The provider tool-call id this result answers.
    pub id: String,
    /// The realized result — `None` marks the awaited `await_human` slot.
    pub text: Option<String>,
}

/// Everything the runner needs to re-enter the tool loop at the exact parked
/// turn. Persisted (as opaque JSON, from core's point of view) alongside the
/// serialized [`AgentSession`](crate::session::AgentSession).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ParkedConversation {
    /// The full rig message history at park time — ending with the parked
    /// turn's user input and the assistant message that carries the
    /// `await_human` tool call (so the resume input's tool results pair with
    /// their calls, preserving strict user/assistant alternation).
    pub history: Vec<Message>,
    /// The recall ledger (elided-history lines) at park time.
    pub ledger: Vec<String>,
    /// The parked turn's tool results, in call order, with exactly one
    /// `text: None` hole for the awaited call.
    pub pending: Vec<PendingToolResult>,
    /// Turns consumed before the park — the resume re-enters the loop here so
    /// one session cannot outrun `max_turns` by suspending.
    pub turns_used: u32,
}

impl ParkedConversation {
    /// Build the resume turn's user input: every pending tool result in call
    /// order, with the human `reply` injected as the awaited call's result.
    ///
    /// Typed errors, never panics: a parked payload without exactly one
    /// awaited hole (corruption — `park` always writes exactly one) is
    /// `AGENT_PARKED_SESSION_CORRUPT`.
    pub fn resume_input(&self, reply: &str) -> Result<Message, ExecutorError> {
        let holes = self.pending.iter().filter(|p| p.text.is_none()).count();
        if holes != 1 {
            return Err(permanent(
                AgentErrorCode::ParkedSessionCorrupt,
                format!(
                    "parked conversation must have exactly one awaited tool slot, found {holes}"
                ),
            ));
        }
        let results: Vec<UserContent> = self
            .pending
            .iter()
            .map(|p| {
                let text = p.text.clone().unwrap_or_else(|| reply.to_string());
                UserContent::ToolResult(ToolResult {
                    id: p.id.clone(),
                    call_id: None,
                    content: OneOrMany::one(ToolResultContent::text(text)),
                })
            })
            .collect();
        match OneOrMany::many(results) {
            Ok(content) => Ok(Message::User { content }),
            // Unreachable given holes == 1 ⇒ pending is non-empty, but stay
            // typed rather than expect().
            Err(_) => Err(permanent(
                AgentErrorCode::ParkedSessionCorrupt,
                "parked conversation has no pending tool results",
            )),
        }
    }
}

/// Extract the human-facing prompt from an `await_human` call's arguments.
/// Lenient by design (the model already committed to suspending): a missing /
/// malformed `prompt` falls back to the raw argument string so the human
/// always sees *something* actionable.
pub(crate) fn await_prompt_from_args(arguments: &str) -> String {
    serde_json::from_str::<serde_json::Value>(arguments)
        .ok()
        .and_then(|v| v.get("prompt").and_then(|p| p.as_str()).map(String::from))
        .unwrap_or_else(|| arguments.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn pending(id: &str, text: Option<&str>) -> PendingToolResult {
        PendingToolResult {
            id: id.into(),
            text: text.map(String::from),
        }
    }

    fn parked(pending_entries: Vec<PendingToolResult>) -> ParkedConversation {
        ParkedConversation {
            history: vec![Message::user("goal")],
            ledger: vec![],
            pending: pending_entries,
            turns_used: 1,
        }
    }

    #[test]
    fn resume_input_fills_the_awaited_hole_with_the_reply() {
        let conv = parked(vec![pending("t1", Some("looked up")), pending("a1", None)]);
        let input = conv.resume_input("yes, approved").unwrap();
        let json = serde_json::to_string(&input).unwrap();
        assert!(json.contains("yes, approved"), "reply injected: {json}");
        assert!(json.contains("looked up"), "realized results kept: {json}");
        assert!(
            json.contains("\"t1\"") && json.contains("\"a1\""),
            "both call ids answered: {json}"
        );
    }

    #[test]
    fn resume_input_with_no_awaited_hole_is_typed_corruption() {
        let conv = parked(vec![pending("t1", Some("done"))]);
        let err = conv.resume_input("reply").unwrap_err();
        assert!(
            format!("{err:?}").contains("AGENT_PARKED_SESSION_CORRUPT"),
            "got {err:?}"
        );
    }

    #[test]
    fn resume_input_with_two_awaited_holes_is_typed_corruption() {
        let conv = parked(vec![pending("a1", None), pending("a2", None)]);
        let err = conv.resume_input("reply").unwrap_err();
        assert!(
            format!("{err:?}").contains("AGENT_PARKED_SESSION_CORRUPT"),
            "got {err:?}"
        );
    }

    #[test]
    fn a_parked_conversation_round_trips_through_json() {
        // The persist-then-reload contract: what is persisted is JSON, so the
        // fidelity bar is JSON-level — deserializing and re-serializing yields
        // the same document the provider would see. (Struct-level equality is
        // deliberately NOT asserted for `history`: rig's flattened
        // `additional_params` deserializes a serialized `None` as `Some({})`,
        // which is semantically identical on the wire.)
        let conv = ParkedConversation {
            history: vec![Message::user("the goal"), Message::user("more context")],
            ledger: vec!["elided #1 · tools: x → 5 B · recall: spill_read slot=H1".into()],
            pending: vec![pending("t1", Some("result")), pending("a1", None)],
            turns_used: 3,
        };
        let json = serde_json::to_value(&conv).unwrap();
        let back: ParkedConversation = serde_json::from_value(json.clone()).unwrap();
        // Reload-then-re-persist is byte-stable (the durable fixpoint)…
        assert_eq!(serde_json::to_value(&back).unwrap(), json);
        // …and every non-message field round-trips exactly.
        assert_eq!(back.ledger, conv.ledger);
        assert_eq!(back.pending, conv.pending);
        assert_eq!(back.turns_used, conv.turns_used);
        assert_eq!(back.history.len(), conv.history.len());
    }

    #[test]
    fn await_prompt_prefers_the_prompt_field_and_falls_back_to_raw() {
        assert_eq!(
            await_prompt_from_args(r#"{"prompt":"ship it?"}"#),
            "ship it?"
        );
        assert_eq!(
            await_prompt_from_args(r#"{"question":"?"}"#),
            r#"{"question":"?"}"#
        );
        assert_eq!(await_prompt_from_args("not json"), "not json");
    }
}
