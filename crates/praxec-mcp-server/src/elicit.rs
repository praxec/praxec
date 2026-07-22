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
//! This module is the pure, typed form-construction. [`plan`] is the single
//! entry point: it decides between pushing a form ([`FormPlan::Push`]) and
//! refusing to ([`FormPlan::Skip`]) — a gate whose `presents`/`choices`
//! projection is defect-marked, or whose declared `inputSchema` no elicitation
//! answer could ever satisfy, must never reach the human as a doomed Accept.
//! The push/resume orchestration lives on `PraxecServer` (it needs the peer +
//! the governed submit path), but every wire type it hands to rmcp is built
//! here.

use std::collections::BTreeMap;

use praxec_core::hitl::{GateChoices, HitlSource, PendingHumanGate};
use rmcp::model::{
    ConstTitle, ElicitationSchema, EnumSchema, PrimitiveSchema, SingleSelectEnumSchema,
    StringSchema, TitledSingleSelectEnumSchema,
};
use serde_json::Value;

/// Per-value render budget (in chars) for `presented` context blocks inside
/// the elicitation message. A value whose pretty-printed rendering exceeds
/// this is truncated with a self-announcing marker naming where the full value
/// lives (`pending_human.presented["<pointer>"]`) — the human is never shown a
/// silently clipped view.
pub(crate) const PRESENTS_RENDER_BUDGET: usize = 1500;

/// The push/skip decision for a parked human gate.
#[derive(Debug)]
pub(crate) enum FormPlan {
    /// Push an `elicitation/create` round-trip with this message + form.
    Push {
        message: String,
        schema: ElicitationSchema,
    },
    /// Do NOT push. The gate stays parked with its pull handle; `reason` says
    /// why no form could honestly represent it (projection defect, or a
    /// declared schema no elicitation answer could satisfy).
    Skip { reason: String },
}

/// Decide how to surface `gate` to the human.
///
/// Fail-fast fences (never push a doomed or partial form):
/// - a [`defect`](PendingHumanGate::defect)-marked gate (its
///   `presents`/`choices` projection failed) is skipped — the form would be
///   built on missing context;
/// - a declared `inputSchema` that is NOT elicitation-compatible while the
///   submit `require`s fields — with no choice set to answer through — is
///   skipped: the fallback free-text form's Accept could never satisfy the
///   submit's validation.
pub(crate) fn plan(gate: &PendingHumanGate) -> FormPlan {
    if let Some(defect) = &gate.defect {
        return FormPlan::Skip {
            reason: defect.clone(),
        };
    }
    if declared_form(gate).is_none() && gate.choices.is_none() {
        if let Some(required) = incompatible_required(gate) {
            return FormPlan::Skip {
                reason: format!(
                    "doomed form: the transition's inputSchema is not \
                     elicitation-compatible yet declares required {required:?}; an \
                     accepted fallback form could never satisfy the submit — \
                     resolve via the pending_human handle instead"
                ),
            };
        }
    }
    FormPlan::Push {
        message: message(gate),
        schema: form_schema(gate),
    }
}

/// The human-readable message shown above the form.
///
/// First line: the gate's prompt (pre-resolved on the gate by the
/// prompt-source chain) or a synthesized line naming the mission and the
/// transition awaiting a human. Then one labeled block per `presented` entry
/// (each budgeted via [`PRESENTS_RENDER_BUDGET`]), then the numbered choice
/// list — the options stay visible even in clients that render enum forms
/// poorly. Gates without the new fields produce exactly the legacy one-line
/// message.
pub(crate) fn message(gate: &PendingHumanGate) -> String {
    let mut out = match &gate.prompt {
        Some(p) => p.clone(),
        None => format!(
            "Mission {} is waiting on you to '{}'.",
            gate.workflow_id, gate.transition
        ),
    };
    if let Some(presented) = &gate.presented {
        for (pointer, value) in presented {
            out.push_str("\n\n— ");
            out.push_str(pointer);
            out.push_str(" —\n");
            out.push_str(&render_presented_value(pointer, value));
        }
    }
    if let Some(choices) = &gate.choices {
        out.push('\n');
        for (i, option) in choices.options.iter().enumerate() {
            out.push('\n');
            match &option.title {
                Some(title) => out.push_str(&format!("{}. {} — {}", i + 1, option.value, title)),
                None => out.push_str(&format!("{}. {}", i + 1, option.value)),
            }
        }
    }
    out
}

/// Build the elicitation form for a gate.
///
/// Preference order:
/// 1. The resolving transition's declared `inputSchema`, when it is
///    elicitation-compatible (an object of primitive-typed properties). Using
///    it verbatim means the human's answer maps 1:1 onto the submit's
///    `arguments`. When the gate also carries `choices`, the choice field is
///    replaced with a single-select enum (titled `oneOf` when every option
///    has a title).
/// 2. No compatible schema but `choices`: a minimal synthesized form — the
///    (required) choice field plus an optional `rationale` string.
/// 3. Neither: a single free-text field appropriate to the gate's source. The
///    MCP spec requires a non-empty object schema, so a no-argument approval
///    still gets a one-field form (an optional note); accepting it IS the
///    approval.
pub(crate) fn form_schema(gate: &PendingHumanGate) -> ElicitationSchema {
    match (declared_form(gate), &gate.choices) {
        (Some(schema), None) => schema,
        (Some(mut schema), Some(choices)) => {
            schema
                .properties
                .insert(choices.field.clone(), choice_property(choices));
            schema
        }
        (None, Some(choices)) => minimal_choice_form(choices),
        (None, None) => match gate.source {
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
        },
    }
}

/// The declared `inputSchema` as an elicitation form, or `None` when there is
/// no declaration or it is not elicitation-compatible (nested objects/arrays
/// cannot be elicitation fields; an empty object is not a form).
fn declared_form(gate: &PendingHumanGate) -> Option<ElicitationSchema> {
    let Some(Value::Object(obj)) = &gate.input_schema else {
        return None;
    };
    let schema = ElicitationSchema::from_json_schema(obj.clone()).ok()?;
    (!schema.properties.is_empty()).then_some(schema)
}

/// The non-empty `required` list of a declared-but-incompatible `inputSchema`
/// — the signature of a doomed form: the submit will demand these fields, and
/// no answer the fallback form can produce will carry them. `None` when there
/// is no declaration or nothing is required.
fn incompatible_required(gate: &PendingHumanGate) -> Option<Vec<String>> {
    let Some(Value::Object(obj)) = &gate.input_schema else {
        return None;
    };
    let required: Vec<String> = obj
        .get("required")?
        .as_array()?
        .iter()
        .filter_map(|v| v.as_str().map(str::to_string))
        .collect();
    (!required.is_empty()).then_some(required)
}

/// Pretty-render one `presented` value under the per-value budget. An
/// over-budget value keeps its first [`PRESENTS_RENDER_BUDGET`] chars plus a
/// marker naming the true size and where the full value lives.
fn render_presented_value(pointer: &str, value: &Value) -> String {
    let full = serde_json::to_string_pretty(value)
        .expect("invariant: a serde_json::Value always serializes");
    let total = full.chars().count();
    if total <= PRESENTS_RENDER_BUDGET {
        return full;
    }
    let kept: String = full.chars().take(PRESENTS_RENDER_BUDGET).collect();
    format!(
        "{kept}\n… [truncated: {total} chars total — full value in \
         pending_human.presented[\"{pointer}\"]]"
    )
}

/// The choice field's schema: a titled single-select (`oneOf` of const+title
/// pairs, MCP 2025-11-25) when EVERY option has a title, else a plain enum —
/// never a half-titled `oneOf`.
fn choice_property(choices: &GateChoices) -> PrimitiveSchema {
    let titles: Option<Vec<&str>> = choices.options.iter().map(|o| o.title.as_deref()).collect();
    match titles {
        Some(titles) if !choices.options.is_empty() => {
            let one_of = choices
                .options
                .iter()
                .zip(titles)
                .map(|(option, title)| ConstTitle::new(option.value.clone(), title))
                .collect();
            PrimitiveSchema::Enum(EnumSchema::Single(SingleSelectEnumSchema::Titled(
                TitledSingleSelectEnumSchema::new(one_of),
            )))
        }
        _ => {
            let values = choices.options.iter().map(|o| o.value.clone()).collect();
            PrimitiveSchema::Enum(EnumSchema::builder(values).build())
        }
    }
}

/// The minimal synthesized form for a gate with `choices` but no compatible
/// declared `inputSchema`: the (required) choice field plus an optional
/// free-text rationale.
fn minimal_choice_form(choices: &GateChoices) -> ElicitationSchema {
    let mut properties: BTreeMap<String, PrimitiveSchema> = BTreeMap::new();
    properties.insert(
        "rationale".to_string(),
        PrimitiveSchema::String(
            StringSchema::new().description("Optional rationale recorded with your choice."),
        ),
    );
    // Inserted after `rationale` so a (degenerate) choice field named
    // "rationale" keeps the enum, not the free-text property.
    properties.insert(choices.field.clone(), choice_property(choices));
    ElicitationSchema::new(properties)
        .with_required(vec![choices.field.clone()])
        .with_description("Select one option to resolve the gate.")
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
    use praxec_core::hitl::{GateChoice, HitlSource};
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
            presented: None,
            choices: None,
            defect: None,
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
        assert_eq!(
            form.required.as_deref(),
            Some(&["approved".to_string()][..])
        );
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
        assert_eq!(
            form.required.as_deref(),
            Some(&["response".to_string()][..])
        );
    }

    #[test]
    fn message_prefers_the_prompt() {
        assert_eq!(
            message(&gate(HitlSource::HumanGate, None)),
            "Approve the plan?"
        );
    }

    #[test]
    fn choices_render_a_titled_single_select() {
        let mut g = gate(
            HitlSource::HumanGate,
            Some(json!({
                "type": "object",
                "required": ["chosen_id"],
                "properties": {
                    "chosen_id": { "type": "string" },
                    "rationale": { "type": "string" }
                }
            })),
        );
        g.choices = Some(GateChoices {
            field: "chosen_id".into(),
            options: vec![
                GateChoice {
                    value: "m1".into(),
                    title: Some("Model One".into()),
                },
                GateChoice {
                    value: "m2".into(),
                    title: Some("Model Two".into()),
                },
            ],
        });
        let FormPlan::Push {
            message: msg,
            schema,
        } = plan(&g)
        else {
            panic!("a titled choice gate must be pushed");
        };
        // Every option titled → the choice field is a titled single-select
        // (`oneOf` of const+title pairs), the MCP 2025-11-25 enum shape.
        assert_eq!(
            serde_json::to_string(&schema.properties["chosen_id"]).unwrap(),
            r#"{"type":"string","oneOf":[{"const":"m1","title":"Model One"},{"const":"m2","title":"Model Two"}]}"#
        );
        // The declared schema's other properties and `required` are preserved.
        assert!(schema.properties.contains_key("rationale"));
        assert_eq!(
            schema.required.as_deref(),
            Some(&["chosen_id".to_string()][..])
        );
        // Options are ALSO in the message for clients that render enums poorly.
        assert!(msg.contains("1. m1 — Model One"), "{msg}");
        assert!(msg.contains("2. m2 — Model Two"), "{msg}");
    }

    #[test]
    fn untitled_choices_render_an_enum() {
        // One option lacks a title → plain enum (no half-titled oneOf); and
        // with no declared inputSchema the minimal choice form is synthesized:
        // the required choice field + an optional rationale.
        let mut g = gate(HitlSource::HumanGate, None);
        g.choices = Some(GateChoices {
            field: "chosen_id".into(),
            options: vec![
                GateChoice {
                    value: "m1".into(),
                    title: Some("Model One".into()),
                },
                GateChoice {
                    value: "m2".into(),
                    title: None,
                },
            ],
        });
        let FormPlan::Push {
            message: msg,
            schema,
        } = plan(&g)
        else {
            panic!("a choice gate must be pushed");
        };
        assert_eq!(
            serde_json::to_string(&schema.properties["chosen_id"]).unwrap(),
            r#"{"type":"string","enum":["m1","m2"]}"#
        );
        assert!(schema.properties.contains_key("rationale"));
        assert_eq!(
            schema.required.as_deref(),
            Some(&["chosen_id".to_string()][..])
        );
        // Titled options keep their title in the list; untitled show the value.
        assert!(msg.contains("1. m1 — Model One"), "{msg}");
        assert!(msg.contains("\n2. m2"), "{msg}");
        assert!(!msg.contains("2. m2 —"), "{msg}");
    }

    #[test]
    fn presented_context_is_rendered_with_budget_and_marker() {
        let mut g = gate(HitlSource::HumanGate, None);
        let big = "x".repeat(2000);
        let mut presented = serde_json::Map::new();
        presented.insert("$.context.blob".to_string(), json!(big.clone()));
        presented.insert(
            "$.context.candidates".to_string(),
            json!([{ "id": "m1", "name": "Model One" }]),
        );
        g.presented = Some(presented);
        let FormPlan::Push { message: msg, .. } = plan(&g) else {
            panic!("a presented-context gate must be pushed");
        };
        // Prompt first, then one labeled block per presented entry.
        assert!(msg.starts_with("Approve the plan?"), "{msg}");
        assert!(msg.contains("— $.context.candidates —"), "{msg}");
        assert!(msg.contains("\"id\": \"m1\""), "{msg}");
        // The 2000-char string renders as 2002 chars (quotes) — over the
        // 1500-char budget → truncated with a self-announcing marker naming
        // the pointer and the true size. The full value is never inlined.
        assert!(!msg.contains(&big), "the full value must not be inlined");
        assert!(
            msg.contains(
                "… [truncated: 2002 chars total — full value in pending_human.presented[\"$.context.blob\"]]"
            ),
            "{msg}"
        );
        // The in-budget value carries no marker.
        assert!(
            !msg.contains("pending_human.presented[\"$.context.candidates\"]"),
            "{msg}"
        );
    }

    #[test]
    fn a_defective_gate_is_skipped_not_pushed() {
        let mut g = gate(HitlSource::HumanGate, None);
        g.defect = Some(
            "PRESENTS_UNRESOLVED: '$.context.candidates' resolves to nothing in context".into(),
        );
        match plan(&g) {
            FormPlan::Skip { reason } => assert_eq!(
                reason,
                "PRESENTS_UNRESOLVED: '$.context.candidates' resolves to nothing in context"
            ),
            FormPlan::Push { .. } => panic!("a defective gate must never be pushed as a form"),
        }
    }

    #[test]
    fn an_incompatible_required_schema_is_skipped_not_doomed() {
        // The observed Accept-can-never-succeed defect: `chosen: object` cannot
        // be an elicitation field, yet the submit requires it — the fallback
        // note form's Accept would always fail validation. Never push it.
        let schema = json!({
            "type": "object",
            "required": ["chosen"],
            "properties": { "chosen": { "type": "object" } }
        });
        match plan(&gate(HitlSource::HumanGate, Some(schema.clone()))) {
            FormPlan::Skip { reason } => {
                assert!(reason.starts_with("doomed form:"), "{reason}");
                assert!(reason.contains("chosen"), "{reason}");
            }
            FormPlan::Push { .. } => panic!("a doomed form must not be pushed"),
        }
        // A resolvable choice set makes the same gate answerable again (the
        // synthesized choice form replaces the incompatible declaration).
        let mut g = gate(HitlSource::HumanGate, Some(schema));
        g.choices = Some(GateChoices {
            field: "chosen_id".into(),
            options: vec![GateChoice {
                value: "m1".into(),
                title: None,
            }],
        });
        assert!(matches!(plan(&g), FormPlan::Push { .. }));
    }

    #[test]
    fn legacy_gates_produce_byte_identical_message_and_form() {
        // Fence: serializations captured from the pre-FormPlan implementation.
        // Gates without presented/choices/defect must not drift by a byte —
        // through `plan` AND the legacy `message`/`form_schema` delegates.
        let note_form = r#"{"type":"object","properties":{"note":{"type":"string","description":"Optional note recorded with your decision."}},"description":"Accept to approve and advance the mission; decline to leave it parked."}"#;
        let cases: Vec<(PendingHumanGate, &str)> = vec![
            (
                gate(
                    HitlSource::HumanGate,
                    Some(json!({
                        "type": "object",
                        "required": ["approved"],
                        "properties": {
                            "approved": { "type": "boolean" },
                            "note": { "type": "string" }
                        },
                        "additionalProperties": false
                    })),
                ),
                r#"{"type":"object","properties":{"approved":{"type":"boolean"},"note":{"type":"string"}},"required":["approved"]}"#,
            ),
            (gate(HitlSource::HumanGate, None), note_form),
            (
                gate(HitlSource::AgentAwait, None),
                r#"{"type":"object","properties":{"response":{"type":"string","description":"Your answer to the workflow's question."}},"required":["response"],"description":"The workflow is waiting on your input."}"#,
            ),
            // Non-primitive schema WITHOUT `required` still falls back to the
            // note form (not doomed — an Accept can succeed).
            (
                gate(
                    HitlSource::HumanGate,
                    Some(json!({
                        "type": "object",
                        "properties": { "plan": { "type": "object" } }
                    })),
                ),
                note_form,
            ),
        ];
        for (g, expected_schema) in cases {
            assert_eq!(message(&g), "Approve the plan?");
            assert_eq!(
                serde_json::to_string(&form_schema(&g)).unwrap(),
                expected_schema
            );
            match plan(&g) {
                FormPlan::Push {
                    message: msg,
                    schema,
                } => {
                    assert_eq!(msg, "Approve the plan?");
                    assert_eq!(serde_json::to_string(&schema).unwrap(), expected_schema);
                }
                FormPlan::Skip { reason } => panic!("legacy gate must push, got skip: {reason}"),
            }
        }
        // The synthesized no-prompt line is unchanged too.
        let mut g = gate(HitlSource::HumanGate, None);
        g.prompt = None;
        assert_eq!(message(&g), "Mission wf_1 is waiting on you to 'approve'.");
    }
}
