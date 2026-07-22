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
use serde_json::{Map, Value};

use crate::model::WorkflowInstance;
use crate::runtime::is_terminal;

/// Budget for the total serialized `presents` projection carried on a gate.
/// A projection over this many bytes is NOT truncated — the gate is
/// defect-marked instead (all-or-nothing), so the human never acts on a
/// silently-clipped view of the decision context.
pub const PRESENTS_BYTE_BUDGET: usize = 32 * 1024;

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

/// One selectable option a gate's `choices` declaration projected: the enum
/// `value` a resolving submit passes as `arguments[field]`, plus an optional
/// human-facing display `title`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GateChoice {
    pub value: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub title: Option<String>,
}

/// The projected choice set for a gate: the submit-argument `field` the answer
/// fills, and the options resolved from live context at gate time.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GateChoices {
    pub field: String,
    pub options: Vec<GateChoice>,
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
    /// What to show the human: the agent's question, or — for a human gate —
    /// the first link of the prompt chain: the transition's `prompt`/`goal`/
    /// `title`, else the instance context's non-empty `prompt` string, else
    /// the STATE's `goal` rendered through the same template renderer as
    /// guidance. `None` when no link supplies one.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub prompt: Option<String>,
    /// The resolving transition's declared `inputSchema`, if any — a JSON Schema
    /// object the resolving `submit` must satisfy. The MCP elicitation push
    /// renders this into the human's form so the answer maps 1:1 onto the
    /// submit's `arguments`. `None` for a no-argument gate (a bare approval).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub input_schema: Option<Value>,
    /// Decision-relevant context the transition's `presents` declares — each
    /// declared `$.context.<key>` pointer mapped to its cloned value.
    /// All-or-nothing: `None` whenever any pointer is malformed/unresolvable
    /// or the projection exceeds [`PRESENTS_BYTE_BUDGET`] (the gate is then
    /// [`defect`](Self::defect)-marked — never a partial view).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub presented: Option<Map<String, Value>>,
    /// The transition's `choices` declaration resolved against live context —
    /// the typed option set an elicitation form renders as a single-select.
    /// Same all-or-nothing rule as `presented`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub choices: Option<GateChoices>,
    /// Why `presented`/`choices` could NOT be projected —
    /// `"PRESENTS_UNRESOLVED: …"` or `"CHOICES_UNRESOLVED: …"`. A defective
    /// gate still surfaces (the mission IS parked) but must never be pushed
    /// as a form built on missing context.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub defect: Option<String>,
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
            presented: None,
            choices: None,
            defect: None,
            source: HitlSource::AgentAwait,
            since: instance.started_at,
        });
    }

    // 2. An `actor: human` gate at the current state (only if still in flight).
    if is_terminal(&instance.definition, &instance.state) {
        return None;
    }
    let (t_name, t_def) = human_transition(&instance.definition, &instance.state)?;
    // Prompt chain: transition `prompt`/`goal`/`title` → the instance
    // context's non-empty `prompt` string (the caller-seeded convention) →
    // the STATE's `goal`, rendered through the same template renderer as
    // state guidance.
    let prompt = ["prompt", "goal", "title"]
        .into_iter()
        .find_map(|k| t_def.get(k).and_then(Value::as_str))
        .map(str::to_string)
        .or_else(|| {
            instance
                .context
                .get("prompt")
                .and_then(Value::as_str)
                .filter(|s| !s.is_empty())
                .map(str::to_string)
        })
        .or_else(|| {
            instance
                .definition
                .pointer(&format!(
                    "/states/{}/goal",
                    crate::runtime::pointer_escape(&instance.state)
                ))
                .and_then(Value::as_str)
                .map(|g| crate::templating::render_template(g, instance))
        });
    let input_schema = t_def.get("inputSchema").cloned();
    let (presented, choices, defect) = project_gate_context(t_def, &instance.context);
    Some(PendingHumanGate {
        workflow_id: instance.id.clone(),
        definition_id: instance.definition_id.clone(),
        state: instance.state.clone(),
        expected_version: instance.version,
        transition: t_name,
        prompt,
        input_schema,
        presented,
        choices,
        defect,
        source: HitlSource::HumanGate,
        since: instance.started_at,
    })
}

/// Project a gate transition's declared `presents` and `choices` against live
/// context. ALL-OR-NOTHING: the first violation returns `(None, None,
/// Some(defect))` — a human never sees a partial projection.
fn project_gate_context(
    t_def: &Value,
    context: &Value,
) -> (
    Option<Map<String, Value>>,
    Option<GateChoices>,
    Option<String>,
) {
    let presented = match resolve_presents(t_def, context) {
        Ok(p) => p,
        Err(detail) => return (None, None, Some(format!("PRESENTS_UNRESOLVED: {detail}"))),
    };
    let choices = match t_def.get("choices") {
        None => None,
        Some(_) => {
            let Some(decl) = transition_choices(t_def) else {
                return (
                    None,
                    None,
                    Some(
                        "CHOICES_UNRESOLVED: malformed `choices` declaration — expected \
                         { field, from, value, title? } strings"
                            .to_string(),
                    ),
                );
            };
            match resolve_choices(&decl, context) {
                Ok(options) => Some(GateChoices {
                    field: decl.field,
                    options,
                }),
                Err(detail) => {
                    return (None, None, Some(format!("CHOICES_UNRESOLVED: {detail}")));
                }
            }
        }
    };
    (presented, choices, None)
}

/// Resolve the transition's `presents` declaration — an array of
/// `"$.context.<key>"` strings (single key segment) — into the projected map
/// (declared pointer → cloned value). `Ok(None)` when undeclared; the first
/// malformed/unresolvable pointer, or a projection over
/// [`PRESENTS_BYTE_BUDGET`], is an `Err(detail)`.
fn resolve_presents(t_def: &Value, context: &Value) -> Result<Option<Map<String, Value>>, String> {
    let Some(presents) = t_def.get("presents") else {
        return Ok(None);
    };
    let entries = presents
        .as_array()
        .ok_or("`presents` must be an array of \"$.context.<key>\" strings")?;
    let mut projected = Map::new();
    for entry in entries {
        let pointer = entry
            .as_str()
            .ok_or_else(|| format!("presents entry {entry} is not a string"))?;
        let key = presents_key(pointer).ok_or_else(|| {
            format!("presents entry '{pointer}' must be \"$.context.<key>\" (single key segment)")
        })?;
        let value = match context.get(key) {
            Some(v) if !v.is_null() => v.clone(),
            _ => return Err(format!("'{pointer}' resolves to nothing in context")),
        };
        projected.insert(pointer.to_string(), value);
    }
    let bytes = serde_json::to_string(&projected)
        .expect("invariant: context values are already valid JSON")
        .len();
    if bytes > PRESENTS_BYTE_BUDGET {
        return Err(format!(
            "projection is {bytes} bytes, over the {PRESENTS_BYTE_BUDGET}-byte budget"
        ));
    }
    Ok(Some(projected))
}

/// The single context key a `presents`/`choices.from` pointer names, or `None`
/// when the pointer is not exactly `$.context.<key>` (one segment — no nested
/// paths, no indexing).
fn presents_key(pointer: &str) -> Option<&str> {
    let key = pointer.strip_prefix("$.context.")?;
    (!key.is_empty() && !key.contains('.') && !key.contains('[')).then_some(key)
}

/// The typed `choices` declaration on a gate transition:
/// `{ field, from: "$.context.<key>", value: <element dot-path>, title?: <element dot-path> }`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ChoicesDecl {
    /// The submit-argument property the chosen `value` fills.
    pub field: String,
    /// The context pointer the option array is read from.
    pub from: String,
    /// Per-element dot-path yielding the option's enum value.
    pub value: String,
    /// Optional per-element dot-path yielding the option's display title.
    pub title: Option<String>,
}

/// Parse a transition's `choices` declaration. The single source of truth for
/// the declared shape — gate-time projection AND the submit-time choice guard
/// both go through here, so the two can never disagree on what was declared.
/// `None` when the key is absent OR the declaration is malformed (a caller
/// that saw the key must treat `None` as a defect, not silence).
pub fn transition_choices(t_def: &Value) -> Option<ChoicesDecl> {
    let decl = t_def.get("choices")?.as_object()?;
    Some(ChoicesDecl {
        field: decl.get("field")?.as_str()?.to_string(),
        from: decl.get("from")?.as_str()?.to_string(),
        value: decl.get("value")?.as_str()?.to_string(),
        title: match decl.get("title") {
            None => None,
            Some(t) => Some(t.as_str()?.to_string()),
        },
    })
}

/// Resolve a parsed `choices` declaration against live context into the
/// projected option list. Shared by gate-time projection and the submit-time
/// choice guard. Any violation — `from` not `$.context.<key>`, not a
/// non-empty array, an element whose `value`/`title` dot-path does not
/// resolve to a string or number — is an `Err(detail)` (all-or-nothing).
pub fn resolve_choices(decl: &ChoicesDecl, context: &Value) -> Result<Vec<GateChoice>, String> {
    let key = presents_key(&decl.from).ok_or_else(|| {
        format!(
            "choices.from '{}' must be \"$.context.<key>\" (single key segment)",
            decl.from
        )
    })?;
    let options = context
        .get(key)
        .and_then(Value::as_array)
        .ok_or_else(|| format!("choices.from '{}' did not resolve to an array", decl.from))?;
    if options.is_empty() {
        return Err(format!(
            "choices.from '{}' resolved to an empty array",
            decl.from
        ));
    }
    options
        .iter()
        .enumerate()
        .map(|(i, element)| {
            let value = choice_string(element, &decl.value).ok_or_else(|| {
                format!(
                    "choices value path '{}' did not resolve to a string or number on element {i}",
                    decl.value
                )
            })?;
            let title = match &decl.title {
                None => None,
                Some(path) => Some(choice_string(element, path).ok_or_else(|| {
                    format!(
                        "choices title path '{path}' did not resolve to a string or number \
                         on element {i}"
                    )
                })?),
            };
            Ok(GateChoice { value, title })
        })
        .collect()
}

/// Read an element dot-path as a display string: strings verbatim, numbers
/// rendered. Anything else (missing, object, array, bool, null) is `None`.
fn choice_string(element: &Value, path: &str) -> Option<String> {
    match element.pointer(&crate::guards::path_to_pointer(path))? {
        Value::String(s) => Some(s.clone()),
        Value::Number(n) => Some(n.to_string()),
        _ => None,
    }
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
///
/// `purpose: ask` transitions are skipped: they are the agent's ask CHANNEL
/// (SPEC §29 injected self-loops), not approval gates — an ask surfaces via
/// the `_agent_await` marker (whose `prompt` is a required String) only when
/// an agent actually asks. Surfacing the bare channel here would mint a
/// promptless pseudo-approval on every state of an `enable_human_ask`
/// workflow. Parity fence: V33 exempts the same shape in `validate.rs`.
fn human_transition<'a>(definition: &'a Value, state: &str) -> Option<(String, &'a Value)> {
    let transitions = definition
        .pointer(&format!(
            "/states/{}/transitions",
            crate::runtime::pointer_escape(state)
        ))
        .and_then(Value::as_object)?;
    transitions
        .iter()
        .find(|(_, t)| {
            t.get("actor").and_then(Value::as_str) == Some("human")
                && t.get("purpose").and_then(Value::as_str) != Some("ask")
        })
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

    /// A single-human-gate definition whose `pick` transition carries the given
    /// extra keys (`presents`, `choices`, prompt sources, …) and whose gating
    /// state optionally declares a `goal`.
    fn def_with_gate(transition_extra: Value, state_goal: Option<&str>) -> Value {
        let mut transition = json!({ "target": "done", "actor": "human" });
        if let Value::Object(extra) = transition_extra {
            for (k, v) in extra {
                transition[k.as_str()] = v;
            }
        }
        let mut state = json!({ "transitions": { "pick": transition } });
        if let Some(goal) = state_goal {
            state["goal"] = json!(goal);
        }
        json!({ "states": { "gating": state, "done": { "terminal": true } } })
    }

    #[test]
    fn prompt_chain_prefers_transition_key() {
        let def = def_with_gate(
            json!({ "goal": "From the transition" }),
            Some("From the state"),
        );
        let ctx = json!({ "prompt": "From the context" });
        let g = pending_gate(&instance("gating", def, ctx)).unwrap();
        assert_eq!(g.prompt.as_deref(), Some("From the transition"));
    }

    #[test]
    fn context_prompt_beats_state_goal() {
        let def = def_with_gate(json!({}), Some("From the state"));
        let ctx = json!({ "prompt": "From the context" });
        let g = pending_gate(&instance("gating", def, ctx)).unwrap();
        assert_eq!(g.prompt.as_deref(), Some("From the context"));
    }

    #[test]
    fn state_goal_is_the_last_link() {
        let def = def_with_gate(json!({}), Some("Pick a shape for {{ $.context.topic }}"));
        // Rendered via the same template renderer as state guidance.
        let g = pending_gate(&instance("gating", def.clone(), json!({ "topic": "auth" }))).unwrap();
        assert_eq!(g.prompt.as_deref(), Some("Pick a shape for auth"));
        // An EMPTY context `prompt` is not a prompt source — the chain falls
        // through to the state goal rather than surfacing a blank prompt.
        let ctx = json!({ "topic": "auth", "prompt": "" });
        let g = pending_gate(&instance("gating", def, ctx)).unwrap();
        assert_eq!(g.prompt.as_deref(), Some("Pick a shape for auth"));
    }

    #[test]
    fn presents_projects_declared_context_values() {
        let def = def_with_gate(
            json!({ "presents": ["$.context.candidates", "$.context.notes"] }),
            None,
        );
        let ctx = json!({ "candidates": [{ "id": "a" }], "notes": "careful" });
        let g = pending_gate(&instance("gating", def, ctx)).unwrap();
        let presented = g.presented.unwrap();
        assert_eq!(
            presented.get("$.context.candidates").unwrap(),
            &json!([{ "id": "a" }])
        );
        assert_eq!(presented.get("$.context.notes").unwrap(), &json!("careful"));
        assert!(g.defect.is_none());
    }

    #[test]
    fn an_unresolvable_presents_pointer_marks_the_gate_defective_not_partial() {
        // One resolvable + one unresolvable pointer → NO partial projection.
        let def = def_with_gate(
            json!({ "presents": ["$.context.candidates", "$.context.missing"] }),
            None,
        );
        let g = pending_gate(&instance("gating", def, json!({ "candidates": [1] }))).unwrap();
        assert!(g.presented.is_none());
        assert!(g.choices.is_none());
        let defect = g.defect.unwrap();
        assert!(defect.starts_with("PRESENTS_UNRESOLVED:"), "{defect}");
        assert!(defect.contains("$.context.missing"), "{defect}");
        // A malformed pointer (nested path — not a single key segment) is
        // equally a defect, never partial.
        let def = def_with_gate(json!({ "presents": ["$.context.a.b"] }), None);
        let g = pending_gate(&instance("gating", def, json!({ "a": { "b": 1 } }))).unwrap();
        assert!(g.presented.is_none());
        assert!(g.defect.unwrap().starts_with("PRESENTS_UNRESOLVED:"));
    }

    #[test]
    fn choices_project_value_and_title() {
        let def = def_with_gate(
            json!({ "choices": { "field": "chosen_id", "from": "$.context.candidates",
                                 "value": "id", "title": "name" } }),
            None,
        );
        let ctx = json!({ "candidates": [
            { "id": "a", "name": "Alpha" },
            { "id": 2, "name": "Beta" },
        ]});
        let g = pending_gate(&instance("gating", def, ctx.clone())).unwrap();
        let choices = g.choices.unwrap();
        assert_eq!(choices.field, "chosen_id");
        assert_eq!(choices.options.len(), 2);
        assert_eq!(choices.options[0].value, "a");
        assert_eq!(choices.options[0].title.as_deref(), Some("Alpha"));
        // A numeric `value` renders to its string form.
        assert_eq!(choices.options[1].value, "2");
        assert!(g.defect.is_none());
        // `title` is optional in the declaration — options then carry none.
        let def = def_with_gate(
            json!({ "choices": { "field": "chosen_id", "from": "$.context.candidates",
                                 "value": "id" } }),
            None,
        );
        let g = pending_gate(&instance("gating", def, ctx)).unwrap();
        assert!(g.choices.unwrap().options[0].title.is_none());
    }

    #[test]
    fn empty_or_non_array_choices_source_is_a_defect() {
        let decl = json!({ "choices": { "field": "f", "from": "$.context.candidates",
                                        "value": "id" } });
        for ctx in [
            json!({ "candidates": [] }),
            json!({ "candidates": "not-an-array" }),
            json!({}),
        ] {
            let def = def_with_gate(decl.clone(), None);
            let g = pending_gate(&instance("gating", def, ctx)).unwrap();
            assert!(g.choices.is_none());
            assert!(g.presented.is_none());
            let defect = g.defect.unwrap();
            assert!(defect.starts_with("CHOICES_UNRESOLVED:"), "{defect}");
        }
    }

    #[test]
    fn oversized_projection_is_a_defect() {
        let def = def_with_gate(json!({ "presents": ["$.context.blob"] }), None);
        let ctx = json!({ "blob": "x".repeat(PRESENTS_BYTE_BUDGET + 1) });
        let g = pending_gate(&instance("gating", def, ctx)).unwrap();
        assert!(g.presented.is_none());
        let defect = g.defect.unwrap();
        assert!(defect.starts_with("PRESENTS_UNRESOLVED:"), "{defect}");
        assert!(defect.contains("budget"), "{defect}");
    }

    #[test]
    fn agent_await_is_untouched_by_presents_and_choices() {
        // Even when the current state's human transition declares both keys
        // (and they would be defective), an `_agent_await` marker wins and the
        // gate carries NO projection and NO defect.
        let def = def_with_gate(
            json!({ "presents": ["$.context.missing"],
                    "choices": { "field": "f", "from": "$.context.missing", "value": "id" } }),
            None,
        );
        let ctx = json!({ "_agent_await": {
            "correlation_id": "cor_x", "prompt": "Which stack?", "transition": "answer"
        }});
        let g = pending_gate(&instance("gating", def, ctx)).unwrap();
        assert_eq!(g.source, HitlSource::AgentAwait);
        assert!(g.presented.is_none());
        assert!(g.choices.is_none());
        assert!(g.defect.is_none());
    }

    #[test]
    fn a_gate_without_new_fields_serializes_byte_identically() {
        // The pre-change serialization of a legacy gate (no presents/choices/
        // defect in the definition) — the new optional fields must not appear.
        let g = pending_gate(&instance("gating", def_with_human_gate(), json!({}))).unwrap();
        let since = serde_json::to_string(&g.since).unwrap();
        let expected = format!(
            "{{\"workflow_id\":\"wf_1\",\"definition_id\":\"cap.gate.thing\",\
             \"state\":\"gating\",\"expected_version\":3,\"transition\":\"approve\",\
             \"prompt\":\"Approve the plan?\",\"source\":\"human_gate\",\"since\":{since}}}"
        );
        assert_eq!(serde_json::to_string(&g).unwrap(), expected);
    }

    /// The injected ask CHANNEL must not surface as an approval gate: a state
    /// whose only human transition is `purpose: ask` has no pending gate, and
    /// a state with both surfaces the real approval, not the channel.
    #[test]
    fn an_ask_purpose_transition_is_not_surfaced_as_a_human_gate() {
        let ask_only = json!({
            "states": { "working": { "transitions": {
                "ask_human": { "target": "working", "actor": "human", "purpose": "ask" }
            } } }
        });
        assert!(pending_gate(&instance("working", ask_only, json!({}))).is_none());

        let both = json!({
            "states": { "gating": { "transitions": {
                "ask_human": { "target": "gating", "actor": "human", "purpose": "ask" },
                "approve": { "target": "done", "actor": "human", "title": "Approve the plan." }
            } } }
        });
        let g = pending_gate(&instance("gating", both, json!({}))).expect("gate");
        assert_eq!(g.transition, "approve");
    }
}
