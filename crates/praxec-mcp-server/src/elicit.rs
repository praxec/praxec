//! MCP Elicitation push — surface a parked human gate to the driving client.
//!
//! When a `praxec.command` parks a mission on a human gate (an `actor: human`
//! approval or an agent's in-workflow question), the response carries a typed
//! [`PendingHumanGate`] under `pending_human`. A client that advertises the
//! `elicitation` capability (MCP 2025-11-25 / SEP-1319) gets that gate turned
//! into a real `elicitation/create` round-trip: the human sees a form, and on
//! accept the mission resumes in the SAME call — no separate poll, no invisible
//! hang. A client WITHOUT the capability is untouched; it still has the
//! `pending_human` block and its `resolve` handle (the pull-list fallback).
//!
//! This module is the pure, typed form-construction. The push/resume
//! orchestration lives on `PraxecServer` (it needs the peer + the governed
//! submit path), but every wire type it hands to rmcp is built here.

use std::collections::BTreeMap;

use praxec_core::hitl::{HitlSource, PendingHumanGate};
use rmcp::model::{ElicitationSchema, PrimitiveSchema, StringSchema};
use serde_json::Value;

/// The human-readable message shown above the form: the gate's prompt (an
/// agent's question or the transition's `goal`/`title`), or a synthesized line
/// naming the mission and the transition awaiting a human.
pub(crate) fn message(gate: &PendingHumanGate) -> String {
    match &gate.prompt {
        Some(p) => p.clone(),
        None => format!(
            "Mission {} is waiting on you to '{}'.",
            gate.workflow_id, gate.transition
        ),
    }
}

/// Build the elicitation form for a gate.
///
/// Preference order:
/// 1. The resolving transition's declared `inputSchema`, when it is
///    elicitation-compatible (an object of primitive-typed properties). Using it
///    verbatim means the human's answer maps 1:1 onto the submit's `arguments`.
///    A non-primitive schema (nested objects/arrays) cannot be an elicitation
///    form, so we fall through rather than send an invalid one.
/// 2. A single free-text field appropriate to the gate's source. The MCP spec
///    requires a non-empty object schema, so a no-argument approval still gets a
///    one-field form (an optional note); accepting it IS the approval.
pub(crate) fn form_schema(gate: &PendingHumanGate) -> ElicitationSchema {
    if let Some(Value::Object(obj)) = &gate.input_schema {
        if let Ok(schema) = ElicitationSchema::from_json_schema(obj.clone()) {
            if !schema.properties.is_empty() {
                return schema;
            }
        }
    }

    match gate.source {
        HitlSource::AgentAwait => single_string_form(
            "response",
            "Your answer to the workflow's question.",
            true,
            "The workflow is waiting on your input.",
        ),
        HitlSource::HumanGate => single_string_form(
            "note",
            "Optional note recorded with your decision.",
            false,
            "Accept to approve and advance the mission; decline to leave it parked.",
        ),
    }
}

/// A one-property object schema — the minimal valid elicitation form. Built
/// directly (not via the fallible builder) so construction cannot fail.
fn single_string_form(
    name: &str,
    field_desc: &str,
    required: bool,
    form_desc: &str,
) -> ElicitationSchema {
    let mut properties: BTreeMap<String, PrimitiveSchema> = BTreeMap::new();
    properties.insert(
        name.to_string(),
        PrimitiveSchema::String(StringSchema::new().description(field_desc.to_string())),
    );
    let schema = ElicitationSchema::new(properties).with_description(form_desc.to_string());
    if required {
        schema.with_required(vec![name.to_string()])
    } else {
        schema
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::Utc;
    use praxec_core::hitl::HitlSource;
    use serde_json::json;

    fn gate(source: HitlSource, input_schema: Option<Value>) -> PendingHumanGate {
        PendingHumanGate {
            workflow_id: "wf_1".into(),
            definition_id: "cap.gate".into(),
            state: "gating".into(),
            expected_version: 3,
            transition: "approve".into(),
            prompt: Some("Approve the plan?".into()),
            input_schema,
            source,
            since: Utc::now(),
        }
    }

    #[test]
    fn declared_primitive_input_schema_is_used_verbatim() {
        let schema = json!({
            "type": "object",
            "required": ["approved"],
            "properties": {
                "approved": { "type": "boolean" },
                "note": { "type": "string" }
            },
            "additionalProperties": false
        });
        let form = form_schema(&gate(HitlSource::HumanGate, Some(schema)));
        assert!(form.properties.contains_key("approved"));
        assert!(form.properties.contains_key("note"));
        assert_eq!(form.required.as_deref(), Some(&["approved".to_string()][..]));
    }

    #[test]
    fn non_primitive_input_schema_falls_back_to_note_form() {
        // A nested object property is not an elicitation primitive — must not be
        // sent as a form; the fallback single-field form is used instead.
        let schema = json!({
            "type": "object",
            "properties": { "plan": { "type": "object" } }
        });
        let form = form_schema(&gate(HitlSource::HumanGate, Some(schema)));
        assert!(form.properties.contains_key("note"));
        assert!(!form.properties.contains_key("plan"));
    }

    #[test]
    fn agent_await_with_no_schema_asks_for_a_response() {
        let form = form_schema(&gate(HitlSource::AgentAwait, None));
        assert!(form.properties.contains_key("response"));
        assert_eq!(form.required.as_deref(), Some(&["response".to_string()][..]));
    }

    #[test]
    fn message_prefers_the_prompt() {
        assert_eq!(message(&gate(HitlSource::HumanGate, None)), "Approve the plan?");
    }
}
