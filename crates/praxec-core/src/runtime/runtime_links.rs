use serde_json::Value;
use serde_json::json;

use crate::model::WorkflowInstance;

/// `linkFilter: byGuards` may be declared on the workflow or per-state.
/// State setting wins when both exist.
pub(crate) fn link_filter_byguards(definition: &Value, state: &str) -> bool {
    let state_setting = definition
        .pointer(&format!("/states/{}/linkFilter", pointer_escape(state)))
        .and_then(Value::as_str);
    if let Some(s) = state_setting {
        return s == "byGuards";
    }
    definition
        .get("linkFilter")
        .and_then(Value::as_str)
        .map(|s| s == "byGuards")
        .unwrap_or(false)
}

pub(crate) fn links(definition: &Value, instance: &WorkflowInstance) -> Vec<Value> {
    if is_terminal(definition, &instance.state) {
        return vec![];
    }

    let path = format!("/states/{}/transitions", pointer_escape(&instance.state));
    let Some(transitions) = definition.pointer(&path).and_then(Value::as_object) else {
        return vec![];
    };

    let library = definition.get("_skillsLibrary").and_then(Value::as_object);

    transitions
        .iter()
        .filter(|(_, t)| t.get("actor").and_then(Value::as_str) != Some("deterministic"))
        .map(|(rel, transition)| {
            // Build the args block. Always carry workflowId / expectedVersion /
            // transition. If the transition declares `prefill`, resolve each
            // value against current scopes and embed under `args.arguments`
            // so an LLM caller can take them verbatim and only generate the
            // fields it actually needs to choose.
            let mut args = serde_json::Map::new();
            args.insert("workflowId".into(), json!(instance.id));
            args.insert("expectedVersion".into(), json!(instance.version));
            args.insert("transition".into(), json!(rel));
            if let Some(prefill) = transition.get("prefill").and_then(Value::as_object) {
                let empty = json!({});
                let mut resolved = serde_json::Map::with_capacity(prefill.len());
                for (k, spec) in prefill {
                    let v = crate::mapping::resolve_value(
                        spec,
                        &empty,             // no caller arguments at link-gen time
                        &instance.context,
                        &instance.input,
                        &empty,             // no executor output at link-gen time
                        Some(&instance.run_env),
                    );
                    resolved.insert(k.clone(), v);
                }
                if !resolved.is_empty() {
                    args.insert("arguments".into(), Value::Object(resolved));
                }
            }

            // SPEC v2 §5.5: transition-scope `skills:` refs ride on the link.
            // They are NOT folded into `guidance.refs` (which carries workflow
            // and state scope) so the model can tell which fragments are
            // tied to taking *this specific* transition.
            let mut link = json!({
                "rel": rel,
                "title": transition.get("title").and_then(Value::as_str).unwrap_or(rel),
                "description": transition.get("description"),
                "method": "praxec.command",
                "actor": transition.get("actor").and_then(Value::as_str).unwrap_or("agent"),
                "args": args,
                "inputSchema": transition.get("inputSchema").cloned().unwrap_or_else(empty_object_schema),
            });
            let refs = resolve_skill_refs(transition.get("skills"), library);
            if !refs.is_empty() {
                link["guidance"] = json!({ "refs": refs });
            }
            link
        })
        .collect()
}

pub(crate) fn transition_definition<'a>(
    definition: &'a Value,
    state: &str,
    transition: &str,
) -> Option<&'a Value> {
    definition.pointer(&format!(
        "/states/{}/transitions/{}",
        pointer_escape(state),
        pointer_escape(transition)
    ))
}

pub fn is_terminal(definition: &Value, state: &str) -> bool {
    definition
        .pointer(&format!("/states/{}/terminal", pointer_escape(state)))
        .and_then(Value::as_bool)
        .unwrap_or(false)
}

pub(crate) fn pointer_escape(value: &str) -> String {
    value.replace('~', "~0").replace('/', "~1")
}

pub(crate) fn empty_object_schema() -> Value {
    json!({
        "type": "object",
        "properties": {},
        "additionalProperties": false
    })
}

/// Append the failed deterministic transition to a response's `links` array
/// so callers have a recovery path back to the broken transition. Shared by
/// the `ChainOutcome::Failed` arms of `start` (runtime.rs) and `dispatch_once`
/// (runtime_submit.rs) — both produce the identical recovery link shape.
///
/// No-ops when `failed_transition` is empty, when the response has no `links`
/// array, or when the transition is not defined on the instance's state.
pub(crate) fn push_failed_chain_recovery_link(
    response: &mut Value,
    definition: &Value,
    instance: &WorkflowInstance,
    failed_transition: &str,
) {
    if failed_transition.is_empty() {
        return;
    }
    if let Some(links) = response.get_mut("links").and_then(Value::as_array_mut) {
        if let Some(t_def) = transition_definition(definition, &instance.state, failed_transition) {
            links.push(json!({
                "rel": failed_transition,
                "title": t_def.get("title").and_then(Value::as_str)
                    .unwrap_or(failed_transition),
                "description": t_def.get("description"),
                "method": "praxec.command",
                "actor": "deterministic",
                "args": {
                    "workflowId": instance.id,
                    "expectedVersion": instance.version,
                    "transition": failed_transition,
                },
                "inputSchema": empty_object_schema(),
            }));
        }
    }
}

/// When a deterministic chain dies with no single transition to point at — a
/// `selection_error`: no viable transition in the current state, because every
/// guard failed (e.g. a producer emitted an out-of-domain discriminant) — attach
/// the state's **legal transitions** as recovery links so the caller is never
/// left at a dead-end. This is the behavioral half of the dead-stall poka-yoke:
/// V23 stops a string-switch with no default from being minted; this guarantees
/// that even an all-guards-fail outcome surfaces a recovery path
/// ("invalid calls always return the current legal links so you can recover").
/// No-ops when the state has no transitions or the response has no `links` array.
pub(crate) fn push_state_recovery_links(
    response: &mut Value,
    definition: &Value,
    instance: &WorkflowInstance,
) {
    let Some(transitions) = definition
        .pointer(&format!(
            "/states/{}/transitions",
            pointer_escape(&instance.state)
        ))
        .and_then(Value::as_object)
    else {
        return;
    };
    if let Some(links) = response.get_mut("links").and_then(Value::as_array_mut) {
        for (t_name, t_def) in transitions {
            links.push(json!({
                "rel": t_name,
                "title": t_def.get("title").and_then(Value::as_str).unwrap_or(t_name),
                "description": t_def.get("description"),
                "method": "praxec.command",
                "actor": t_def.get("actor").and_then(Value::as_str).unwrap_or("deterministic"),
                "args": {
                    "workflowId": instance.id,
                    "expectedVersion": instance.version,
                    "transition": t_name,
                },
                "inputSchema": t_def
                    .get("inputSchema")
                    .cloned()
                    .unwrap_or_else(empty_object_schema),
            }));
        }
    }
}

/// Build the `guidance.refs` array from a workflow snapshot. Pulls subjects
/// from workflow-scope `skills:` and the active state's `skills:` (de-duped,
/// declaration order). Transition-scope refs are surfaced on the link object
/// instead (SPEC §5.5) so callers can tell which fragments are tied to
/// taking *this specific* transition; they are NOT folded in here. Each
/// emitted ref pairs `subject` with the `verb` looked up in the
/// snapshot-stamped `_skillsLibrary`. Subjects with no library entry are
/// skipped — `check` reports those as errors.
pub(crate) fn collect_guidance_refs(definition: &Value, state_def: Option<&Value>) -> Vec<Value> {
    let library = definition.get("_skillsLibrary").and_then(Value::as_object);
    let mut seen = std::collections::BTreeSet::new();
    let mut out = Vec::new();
    push_resolved_refs(definition.get("skills"), library, &mut seen, &mut out);
    push_resolved_refs(
        state_def.and_then(|s| s.get("skills")),
        library,
        &mut seen,
        &mut out,
    );
    out
}

/// Collect the skill subjects in scope for a step: workflow-scope + the
/// active state's scope + (optionally) the firing transition's scope, in
/// broad→specific order, de-duped (first declaration wins).
///
/// This is the single source of truth for *which skills apply to this
/// step*. The guidance-ref path ([`collect_guidance_refs`] for workflow+state
/// and the transition-scope refs on each link) surfaces the same subjects to
/// the orchestrating agent as refs; the `kind: llm` executor resolves the
/// same subjects' bodies into its system message. Both pull from these three
/// scopes so the outward (refs) and inward (bodies) views cannot drift.
pub fn collect_in_scope_skill_subjects(
    definition: &Value,
    state: &str,
    transition: Option<&str>,
) -> Vec<String> {
    let mut seen = std::collections::BTreeSet::new();
    let mut out = Vec::new();
    push_scope_subjects(definition.get("skills"), &mut seen, &mut out);
    let state_def = definition.pointer(&format!("/states/{}", pointer_escape(state)));
    push_scope_subjects(state_def.and_then(|s| s.get("skills")), &mut seen, &mut out);
    if let Some(t) = transition {
        let tdef = transition_definition(definition, state, t);
        push_scope_subjects(tdef.and_then(|t| t.get("skills")), &mut seen, &mut out);
    }
    out
}

/// Append the string subjects in one scope's `skills: [subject]` array,
/// skipping duplicates already in `seen` (first occurrence wins). Shared by
/// [`collect_in_scope_skill_subjects`] and [`push_resolved_refs`] so subject
/// extraction + dedup is defined once.
fn push_scope_subjects(
    scope: Option<&Value>,
    seen: &mut std::collections::BTreeSet<String>,
    out: &mut Vec<String>,
) {
    let Some(arr) = scope.and_then(Value::as_array) else {
        return;
    };
    for entry in arr {
        if let Some(subject) = entry.as_str() {
            if seen.insert(subject.to_string()) {
                out.push(subject.to_string());
            }
        }
    }
}

/// Resolve a single scope's `skills: [subject]` against the library and
/// emit `{verb, subject}` JSON values for the link layer. Used independently
/// of `collect_guidance_refs` so transition-scope refs (which need their own
/// `seen` set per link) don't accidentally consume workflow/state state.
pub(crate) fn resolve_skill_refs(
    scope: Option<&Value>,
    library: Option<&serde_json::Map<String, Value>>,
) -> Vec<Value> {
    let mut seen = std::collections::BTreeSet::new();
    let mut out = Vec::new();
    push_resolved_refs(scope, library, &mut seen, &mut out);
    out
}

pub(crate) fn push_resolved_refs(
    scope: Option<&Value>,
    library: Option<&serde_json::Map<String, Value>>,
    seen: &mut std::collections::BTreeSet<String>,
    out: &mut Vec<Value>,
) {
    let Some(arr) = scope.and_then(Value::as_array) else {
        return;
    };
    for entry in arr {
        let Some(subject) = entry.as_str() else {
            continue;
        };
        if !seen.insert(subject.to_string()) {
            continue;
        }
        // `_skillsLibrary` is `{ subject: { verb, lifecycle, body, hash, source } }`
        // post-§5.7 stamp. Surfaced ref is `{verb, subject, hash}` —
        // body is consulted by `gateway.describe(id, workflowId)` against
        // the snapshot. SPEC §5.4: refs MUST carry hash for cache
        // invalidation; library entries without hash indicate a stamp-time
        // bug, so we surface the ref without hash and let the
        // structural-analysis layer flag it rather than silently dropping.
        let lib_entry = library.and_then(|lib| lib.get(subject));
        let Some(verb) = lib_entry
            .and_then(|entry| entry.get("verb"))
            .and_then(Value::as_str)
        else {
            continue;
        };
        let hash = lib_entry
            .and_then(|entry| entry.get("hash"))
            .and_then(Value::as_str);
        let mut ref_obj = json!({ "verb": verb, "subject": subject });
        if let Some(h) = hash {
            ref_obj["hash"] = Value::String(h.to_string());
        }
        out.push(ref_obj);
    }
}

#[cfg(test)]
mod recovery_tests {
    use super::*;

    fn instance_in(state: &str) -> WorkflowInstance {
        WorkflowInstance {
            id: "wf_test".into(),
            definition_id: "d".into(),
            definition_version: "0".into(),
            definition: json!({}),
            state: state.into(),
            version: 3,
            input: json!({}),
            context: json!({}),
            started_at: chrono::Utc::now(),
            run_env: crate::RunEnv::for_test(),
            cancelled_at: None,
            cancelled_reason: None,
            depth: 0,
            parent: None,
        }
    }

    #[test]
    fn selection_error_recovery_surfaces_every_state_transition() {
        // A `selection_error` leaves the instance in its gate state with no
        // single failed transition. The recovery helper must surface ALL of the
        // state's legal transitions so the caller is never dead-ended.
        let definition = json!({
            "states": {
                "gate": { "transitions": {
                    "to_a": { "actor": "deterministic", "target": "a" },
                    "to_human": { "actor": "human", "target": "h" }
                }}
            }
        });
        let mut response = json!({ "links": [] });
        push_state_recovery_links(&mut response, &definition, &instance_in("gate"));
        assert_eq!(
            response["links"].as_array().unwrap().len(),
            2,
            "both gate transitions must surface as recovery links, got: {response:?}"
        );
    }
}
