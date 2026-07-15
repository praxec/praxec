//! Typed human-in-the-loop gates.
//!
//! A mission SUSPENDS whenever it needs a human — either because an agent inside
//! the workflow paused to ask a question (`_agent_await`), or because the only
//! way forward is an `actor: human` transition (an approval / sign-off). Until
//! now both were stringly-typed: the await marker was a `json!()` blob read back
//! with `.pointer("/_agent_await/correlation_id")`, and there was no enumeration
//! at all. This module gives the gates a **type** and a single detection rule, so
//! the MCP surface can list and resolve them without hand-parsing JSON.
//!
//! The store is the source of truth: [`pending_gate`] inspects ONE persisted
//! [`WorkflowInstance`] (its context + its captured definition snapshot) and, if
//! it is parked awaiting a human, returns the typed gate. The MCP-native surface
//! (`praxec.query { approvals: true }`) maps it over every live instance; the
//! same struct is surfaced inline on a `waiting` response as `pending_human`.

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::model::WorkflowInstance;
use crate::runtime::is_terminal;

/// The durable marker an `await_human`/elicitation suspend writes into the
/// instance context under `_agent_await`. Typed so writer and reader agree on
/// the shape at compile time (was a bare `json!()` blob).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AgentAwaitMarker {
    pub correlation_id: String,
    pub prompt: String,
    /// The transition the human's reply resumes.
    pub transition: String,
}

impl AgentAwaitMarker {
    /// The context key the marker lives under.
    pub const CONTEXT_KEY: &'static str = "_agent_await";

    /// Read the marker off an instance context, if present and well-formed.
    pub fn from_context(context: &Value) -> Option<Self> {
        serde_json::from_value(context.get(Self::CONTEXT_KEY)?.clone()).ok()
    }
}

/// Which HITL mechanism parked the mission.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum HitlSource {
    /// An `actor: human` transition — an approval / sign-off gate.
    HumanGate,
    /// An agent inside the workflow paused to ask the human a question.
    AgentAwait,
}

/// A mission parked awaiting a human — the typed HITL gate the MCP surface
/// enumerates and resolves.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PendingHumanGate {
    pub workflow_id: String,
    pub definition_id: String,
    pub state: String,
    /// The version a resolving `submit` must pass as `expectedVersion`.
    pub expected_version: u64,
    /// The transition a human must fire to advance the mission.
    pub transition: String,
    /// What to show the human: the agent's question, or the gate transition's
    /// `goal`/`title`. `None` when the definition supplies neither.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub prompt: Option<String>,
    /// The resolving transition's declared `inputSchema`, if any — a JSON Schema
    /// object the resolving `submit` must satisfy. The MCP elicitation push
    /// renders this into the human's form so the answer maps 1:1 onto the
    /// submit's `arguments`. `None` for a no-argument gate (a bare approval).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub input_schema: Option<Value>,
    pub source: HitlSource,
    /// When the mission started (the closest persisted timestamp — instances do
    /// not carry a last-updated field).
    pub since: DateTime<Utc>,
}

/// The typed HITL gate for ONE instance, or `None` if it is not parked awaiting a
/// human (terminal, running, or waiting on a lock/sub-workflow rather than a
/// person).
///
/// Precedence: an `_agent_await` marker wins — it is the most specific "a human
/// was explicitly asked" signal. Otherwise, a NON-terminal instance whose current
/// state has an outgoing `actor: human` transition is an approval gate. A state
/// with several human transitions surfaces the first (a resolver picks the branch
/// by its arguments); the gate exists regardless of which one fires.
pub fn pending_gate(instance: &WorkflowInstance) -> Option<PendingHumanGate> {
    // 1. Explicit agent-await / elicitation.
    if let Some(marker) = AgentAwaitMarker::from_context(&instance.context) {
        let input_schema =
            transition_input_schema(&instance.definition, &instance.state, &marker.transition);
        return Some(PendingHumanGate {
            workflow_id: instance.id.clone(),
            definition_id: instance.definition_id.clone(),
            state: instance.state.clone(),
            expected_version: instance.version,
            transition: marker.transition,
            prompt: Some(marker.prompt),
            input_schema,
            source: HitlSource::AgentAwait,
            since: instance.started_at,
        });
    }

    // 2. An `actor: human` gate at the current state (only if still in flight).
    if is_terminal(&instance.definition, &instance.state) {
        return None;
    }
    let (t_name, t_def) = human_transition(&instance.definition, &instance.state)?;
    let prompt = ["prompt", "goal", "title"]
        .into_iter()
        .find_map(|k| t_def.get(k).and_then(Value::as_str))
        .map(str::to_string);
    let input_schema = t_def.get("inputSchema").cloned();
    Some(PendingHumanGate {
        workflow_id: instance.id.clone(),
        definition_id: instance.definition_id.clone(),
        state: instance.state.clone(),
        expected_version: instance.version,
        transition: t_name,
        prompt,
        input_schema,
        source: HitlSource::HumanGate,
        since: instance.started_at,
    })
}

/// Read the `inputSchema` of a named transition at a given state, if declared.
fn transition_input_schema(definition: &Value, state: &str, transition: &str) -> Option<Value> {
    definition
        .pointer(&format!(
            "/states/{}/transitions/{}/inputSchema",
            crate::runtime::pointer_escape(state),
            crate::runtime::pointer_escape(transition)
        ))
        .cloned()
}

/// The first `actor: human` transition of `state`, as `(name, def)`.
fn human_transition<'a>(definition: &'a Value, state: &str) -> Option<(String, &'a Value)> {
    let transitions = definition
        .pointer(&format!(
            "/states/{}/transitions",
            crate::runtime::pointer_escape(state)
        ))
        .and_then(Value::as_object)?;
    transitions
        .iter()
        .find(|(_, t)| t.get("actor").and_then(Value::as_str) == Some("human"))
        .map(|(name, def)| (name.clone(), def))
}

/// The id of the sub-workflow a parent is parked waiting on, if any. Read from
/// the `_subworkflow_wait.child_workflow_id` record the runtime durably stamps
/// when a `kind: workflow` transition suspends. Used to surface a CHILD's human
/// gate on the PARENT's response — the fix for the "parent shows `links: []` at
/// a gate that actually lives on its child" defect.
pub fn subworkflow_child_id(instance: &WorkflowInstance) -> Option<String> {
    instance
        .context
        .pointer("/_subworkflow_wait/child_workflow_id")
        .and_then(Value::as_str)
        .map(str::to_string)
}

/// Map [`pending_gate`] over a set of instances, keeping only the parked ones,
/// oldest-first (a human clears the longest-waiting gate first).
pub fn pending_gates(instances: &[WorkflowInstance]) -> Vec<PendingHumanGate> {
    let mut gates: Vec<PendingHumanGate> = instances.iter().filter_map(pending_gate).collect();
    gates.sort_by_key(|g| g.since);
    gates
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn instance(state: &str, definition: Value, context: Value) -> WorkflowInstance {
        WorkflowInstance {
            id: "wf_1".into(),
            definition_id: "cap.gate.thing".into(),
            definition_version: "0".into(),
            definition,
            state: state.into(),
            version: 3,
            input: json!({}),
            context,
            started_at: Utc::now(),
            run_env: crate::RunEnv::for_test(),
            cancelled_at: None,
            cancelled_reason: None,
            depth: 0,
            parent: None,
        }
    }

    fn def_with_human_gate() -> Value {
        json!({ "states": {
            "gating": { "transitions": {
                "approve": { "target": "done", "actor": "human",
                             "goal": "Approve the plan?" }
            }},
            "done": { "terminal": true }
        }})
    }

    #[test]
    fn an_actor_human_gate_is_detected_with_its_prompt() {
        let g = pending_gate(&instance("gating", def_with_human_gate(), json!({}))).unwrap();
        assert_eq!(g.source, HitlSource::HumanGate);
        assert_eq!(g.transition, "approve");
        assert_eq!(g.prompt.as_deref(), Some("Approve the plan?"));
        assert_eq!(g.expected_version, 3);
    }

    #[test]
    fn an_agent_await_marker_wins_and_is_typed() {
        let ctx = json!({ "_agent_await": {
            "correlation_id": "cor_x", "prompt": "Which stack?", "transition": "answer"
        }});
        let g = pending_gate(&instance("asking", json!({ "states": {} }), ctx)).unwrap();
        assert_eq!(g.source, HitlSource::AgentAwait);
        assert_eq!(g.transition, "answer");
        assert_eq!(g.prompt.as_deref(), Some("Which stack?"));
    }

    #[test]
    fn subworkflow_child_id_reads_the_wait_record() {
        let ctx = json!({ "_subworkflow_wait": {
            "child_workflow_id": "wf_child_42", "transition": "build"
        }});
        let inst = instance("building", json!({ "states": {} }), ctx);
        assert_eq!(subworkflow_child_id(&inst).as_deref(), Some("wf_child_42"));
        // Absent record → None (not waiting on a child).
        assert!(subworkflow_child_id(&instance("s", json!({}), json!({}))).is_none());
    }

    #[test]
    fn a_terminal_or_non_human_state_is_not_a_gate() {
        assert!(pending_gate(&instance("done", def_with_human_gate(), json!({}))).is_none());
        let det = json!({ "states": { "s": { "transitions": {
            "go": { "target": "done", "actor": "deterministic" } } } } });
        assert!(pending_gate(&instance("s", det, json!({}))).is_none());
    }
}
