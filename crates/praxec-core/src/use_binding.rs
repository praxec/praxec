//! SPEC §6 — `use:` binding helpers for cross-workflow invocation.
//!
//! A `kind: workflow` executor invoking a capability declares a `use:`
//! block:
//!
//! ```yaml
//! executor:
//!   kind: workflow
//!   definitionId: cap.plan.vet
//!   use:
//!     inputs:
//!       plan: "$.context.draft_plan"
//!       max_iterations: 3
//!     outputs:
//!       "$.context.vet_verdict": verdict
//!       "$.context.vet_findings": findings
//! ```
//!
//! This module provides the three pure helpers the runtime composes:
//!
//! 1. [`resolve_use_inputs`] — for each `use.inputs` entry, dereference
//!    the RHS expression (`$.context.foo` style) or pass a literal through
//!    unchanged. The resulting map becomes the sub-workflow's
//!    `StartWorkflow.input`.
//! 2. [`project_use_outputs`] — after the sub-workflow terminates, walk
//!    `use.outputs` and pull each declared output name from the child's
//!    final context. The returned map is keyed by the HOST JSON paths so
//!    the existing `merge_output` projection layer (see
//!    `runtime_submit::merge_output`) can write to them.
//! 3. [`validate_outputs_against_snippet`] — given the capability's
//!    declared `snippet.outputs` schemas + the actual projected output
//!    values, run each through `jsonschema::validator_for`. Returns a
//!    list of structured violations on the first batch of failures.
//!
//! Putting these in their own module keeps the executor at
//! `crates/praxec-executors/src/workflow.rs` thin (decision tree
//! + audit emission) and the policy logic unit-testable in isolation.

use serde_json::{Map, Value};

use crate::mapping::read_in_scopes;

/// Structured snippet-output schema violation. `slot` is the cap-declared
/// output name; `reason` carries the jsonschema validator's message,
/// joined with `;` when multiple errors fire on the same slot. The shape
/// matches `audit::AuditEvent` payload conventions so the executor can
/// emit a `cap.output.schema_violation` event without further glue.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SchemaViolation {
    pub slot: String,
    pub reason: String,
}

/// SPEC §6.1 — Resolve a `use.inputs` map into the literal value map that
/// becomes the sub-workflow's `StartWorkflow.input`.
///
/// Each `(input_name, expr)` pair:
/// - if `expr` is a string starting with `$.`, resolved via
///   [`read_in_scopes`] against the host's `arguments`, `context`, and
///   `workflow.input`. Unresolved expressions yield `Value::Null` (the
///   typed-input check happens inside the sub-workflow itself).
/// - otherwise, the literal value is passed through unchanged.
/// - object values recurse so nested `{ a: { b: "$.context.x" } }`
///   shapes resolve correctly.
///
/// `use_inputs` may legitimately be empty (`{}`); the function returns an
/// empty map without erroring.
pub fn resolve_use_inputs(
    use_inputs: &Value,
    host_arguments: &Value,
    host_context: &Value,
    host_workflow_input: &Value,
) -> Map<String, Value> {
    let Some(obj) = use_inputs.as_object() else {
        return Map::new();
    };
    let mut resolved = Map::new();
    for (name, expr) in obj {
        let value = resolve_one(expr, host_arguments, host_context, host_workflow_input);
        resolved.insert(name.clone(), value);
    }
    resolved
}

fn resolve_one(expr: &Value, args: &Value, ctx: &Value, wf_input: &Value) -> Value {
    match expr {
        Value::String(s) if s.starts_with("$.") => {
            read_in_scopes(s, args, ctx, wf_input, None).unwrap_or(Value::Null)
        }
        Value::Object(map) => {
            let mut out = Map::new();
            for (k, v) in map {
                out.insert(k.clone(), resolve_one(v, args, ctx, wf_input));
            }
            Value::Object(out)
        }
        Value::Array(arr) => Value::Array(
            arr.iter()
                .map(|v| resolve_one(v, args, ctx, wf_input))
                .collect(),
        ),
        other => other.clone(),
    }
}

/// SPEC §6.1 — Project a sub-workflow's terminal context into the value
/// map shape expected by `runtime_submit::merge_output`. Returns
/// `{ <host_path>: <child_value>, ... }` where `<host_path>` is the
/// LHS of each `use.outputs` entry and `<child_value>` is the value the
/// child stored at `$.context.<cap_output_name>`.
///
/// Outputs declared in `use.outputs` but absent from the child's final
/// context are projected as `Value::Null`. The schema validator in
/// [`validate_outputs_against_snippet`] is the gate that decides whether
/// `Null` is a contract violation — that lives one step downstream so
/// this function stays purely structural.
pub fn project_use_outputs(use_outputs: &Value, child_context: &Value) -> Map<String, Value> {
    let Some(obj) = use_outputs.as_object() else {
        return Map::new();
    };
    let child_obj = child_context.as_object();
    let mut out = Map::new();
    for (host_path, cap_output_name) in obj {
        let Some(name_str) = cap_output_name.as_str() else {
            continue;
        };
        let value = child_obj
            .and_then(|o| o.get(name_str))
            .cloned()
            .unwrap_or(Value::Null);
        out.insert(host_path.clone(), value);
    }
    out
}

/// SPEC §5.3 — Validate the cap-projected outputs against the capability's
/// declared `snippet.outputs` schemas.
///
/// `snippet_outputs` is the capability's full `snippet.outputs` block —
/// shape `{ <name>: <jsonschema-fragment>, ... }`. `projected` is the
/// LHS-keyed map returned by [`project_use_outputs`]. `output_paths`
/// names which LHS host paths map to which cap output names (i.e., the
/// raw `use.outputs` block) — needed because `projected` is keyed by
/// host path, but schemas are keyed by cap output name.
///
/// Returns `Err(violations)` with ALL violations collected (not just the
/// first) so the audit event carries a complete diff. Returns `Ok(())`
/// when every projected value is valid AND every required snippet output
/// is present. Snippet outputs whose name is absent from `output_paths`
/// are skipped (the host flow legitimately ignored that output);
/// completeness of the host binding is V12 (load-time), not a runtime
/// concern.
pub fn validate_outputs_against_snippet(
    snippet_outputs: &Value,
    output_paths: &Value,
    projected: &Map<String, Value>,
) -> Result<(), Vec<SchemaViolation>> {
    let Some(schemas) = snippet_outputs.as_object() else {
        // No schemas declared → nothing to validate. The cap effectively
        // declares "any output shape is fine"; load-time validation
        // (V4) ensures snippet.outputs is at minimum an object.
        return Ok(());
    };
    let Some(bindings) = output_paths.as_object() else {
        return Ok(());
    };
    // Invert bindings: cap-output-name -> host-path.
    let mut by_name: std::collections::HashMap<&str, &str> = std::collections::HashMap::new();
    for (host_path, cap_name_value) in bindings {
        if let Some(cap_name) = cap_name_value.as_str() {
            by_name.insert(cap_name, host_path.as_str());
        }
    }

    let mut violations: Vec<SchemaViolation> = Vec::new();
    for (cap_name, schema) in schemas {
        let Some(host_path) = by_name.get(cap_name.as_str()) else {
            // Cap declares this output but host didn't bind it. That's
            // V12 territory if mandatory; runtime treats it as a no-op.
            continue;
        };
        let value = projected.get(*host_path).cloned().unwrap_or(Value::Null);
        // Registry-aware (strictly widening): a slot cap's `snippet.outputs`
        // fragment may `$ref` the shipped HOP vocabulary (e.g.
        // `{ "$ref": "praxec://hop#/$defs/verifyOut" }`); the registry resolves
        // it. Self-contained snippet schemas behave exactly as before.
        let validator = match crate::hop::compile_validator(schema) {
            Ok(v) => v,
            Err(e) => {
                // A snippet schema that itself fails to compile is a
                // contract bug; surface it as a violation rather than
                // silently passing — load-time V4 will normally have
                // caught this first.
                violations.push(SchemaViolation {
                    slot: cap_name.clone(),
                    reason: format!("invalid snippet.outputs schema: {e}"),
                });
                continue;
            }
        };
        if !validator.is_valid(&value) {
            let reason: Vec<String> = validator
                .iter_errors(&value)
                .map(|e| e.to_string())
                .collect();
            violations.push(SchemaViolation {
                slot: cap_name.clone(),
                reason: reason.join("; "),
            });
        }
    }
    if violations.is_empty() {
        Ok(())
    } else {
        Err(violations)
    }
}

/// Deterministic-repair rung (P12 R3.1). Coerce a projected output that is
/// `Null` into the schema's unambiguous empty/default value BEFORE validation,
/// so a commodity model that emits an explicit `null` for an array/object
/// field (or omits a field carrying a declared `default`) does not hard-fail
/// the hop over a mechanically-repairable gap — zero model calls.
///
/// Only the unambiguously-repairable cases are touched: an explicit schema
/// `default`, or `type: array` (→ `[]`) / `type: object` (→ `{}`), including a
/// nullable union like `["array","null"]`. A `Null` under a scalar schema
/// (string/number/boolean) or a `$ref`/untyped schema is LEFT AS-IS so genuine
/// contract violations still surface at [`validate_outputs_against_snippet`].
///
/// Mutates `projected` in place — the repaired value is what propagates
/// forward — and returns the cap-output names that were repaired, for
/// observability (which rung resolved the miss).
pub fn repair_outputs_against_snippet(
    snippet_outputs: &Value,
    output_paths: &Value,
    projected: &mut Map<String, Value>,
) -> Vec<String> {
    let (Some(schemas), Some(bindings)) = (snippet_outputs.as_object(), output_paths.as_object())
    else {
        return Vec::new();
    };
    let mut by_name: std::collections::HashMap<&str, &str> = std::collections::HashMap::new();
    for (host_path, cap_name_value) in bindings {
        if let Some(cap_name) = cap_name_value.as_str() {
            by_name.insert(cap_name, host_path.as_str());
        }
    }
    let mut repaired: Vec<String> = Vec::new();
    for (cap_name, schema) in schemas {
        let Some(host_path) = by_name.get(cap_name.as_str()) else {
            continue;
        };
        // Repair only a present-but-Null value (or a bound-but-absent one).
        if !matches!(projected.get(*host_path), Some(Value::Null) | None) {
            continue;
        }
        if let Some(empty) = deterministic_empty_for(schema) {
            projected.insert((*host_path).to_string(), empty);
            repaired.push(cap_name.clone());
        }
    }
    repaired
}

/// The unambiguous empty/default value for a snippet output schema, or `None`
/// when there is no safe deterministic repair (scalars, `$ref`, untyped).
fn deterministic_empty_for(schema: &Value) -> Option<Value> {
    // An explicit `default` always wins — it is the author's declared intent.
    if let Some(default) = schema.get("default") {
        return Some(default.clone());
    }
    match schema.get("type") {
        Some(Value::String(t)) if t == "array" => Some(Value::Array(vec![])),
        Some(Value::String(t)) if t == "object" => Some(Value::Object(Map::new())),
        // Nullable unions, e.g. ["array","null"] → []; ["object","null"] → {}.
        Some(Value::Array(types)) => {
            let has = |name: &str| types.iter().any(|v| v.as_str() == Some(name));
            if has("array") {
                Some(Value::Array(vec![]))
            } else if has("object") {
                Some(Value::Object(Map::new()))
            } else {
                None
            }
        }
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn resolve_use_inputs_dereferences_context_path() {
        let use_inputs = json!({ "plan": "$.context.draft_plan" });
        let ctx = json!({ "draft_plan": { "title": "ship it" } });
        let r = resolve_use_inputs(&use_inputs, &json!({}), &ctx, &json!({}));
        assert_eq!(r.get("plan"), Some(&json!({ "title": "ship it" })));
    }

    #[test]
    fn resolve_use_inputs_passes_literal_values_through() {
        let use_inputs = json!({ "max_iterations": 3, "label": "primary" });
        let r = resolve_use_inputs(&use_inputs, &json!({}), &json!({}), &json!({}));
        assert_eq!(r.get("max_iterations"), Some(&json!(3)));
        assert_eq!(r.get("label"), Some(&json!("primary")));
    }

    #[test]
    fn resolve_use_inputs_returns_null_for_unresolved_path() {
        let use_inputs = json!({ "missing": "$.context.does_not_exist" });
        let r = resolve_use_inputs(&use_inputs, &json!({}), &json!({}), &json!({}));
        assert_eq!(r.get("missing"), Some(&Value::Null));
    }

    #[test]
    fn resolve_use_inputs_recurses_into_nested_objects() {
        let use_inputs = json!({
            "config": { "plan": "$.context.plan", "limit": 5 }
        });
        let ctx = json!({ "plan": "P1" });
        let r = resolve_use_inputs(&use_inputs, &json!({}), &ctx, &json!({}));
        assert_eq!(r.get("config"), Some(&json!({ "plan": "P1", "limit": 5 })));
    }

    #[test]
    fn project_use_outputs_keys_by_host_path() {
        let use_outputs = json!({
            "$.context.vet_verdict":  "verdict",
            "$.context.vet_findings": "findings",
        });
        let child_context = json!({
            "verdict":  "pass",
            "findings": ["all good"],
            "internal_secret": "TOPSECRET", // not declared as output, must NOT propagate
        });
        let p = project_use_outputs(&use_outputs, &child_context);
        assert_eq!(p.get("$.context.vet_verdict"), Some(&json!("pass")));
        assert_eq!(p.get("$.context.vet_findings"), Some(&json!(["all good"])));
        assert!(!p.contains_key("internal_secret"));
        assert_eq!(p.len(), 2, "only declared outputs propagate");
    }

    #[test]
    fn project_use_outputs_null_when_child_did_not_emit() {
        let use_outputs = json!({ "$.context.verdict": "verdict" });
        let child_context = json!({});
        let p = project_use_outputs(&use_outputs, &child_context);
        assert_eq!(p.get("$.context.verdict"), Some(&Value::Null));
    }

    #[test]
    fn validate_outputs_against_snippet_passes_when_value_matches_schema() {
        let snippet_outputs = json!({
            "verdict": { "type": "string", "enum": ["pass", "fail", "needs-revision"] }
        });
        let output_paths = json!({ "$.context.verdict": "verdict" });
        let mut projected = Map::new();
        projected.insert("$.context.verdict".into(), json!("pass"));

        validate_outputs_against_snippet(&snippet_outputs, &output_paths, &projected)
            .expect("valid output should pass");
    }

    #[test]
    fn validate_outputs_against_snippet_collects_violations() {
        let snippet_outputs = json!({
            "verdict": { "type": "string", "enum": ["pass", "fail", "needs-revision"] },
            "score":   { "type": "integer", "minimum": 0, "maximum": 100 }
        });
        let output_paths = json!({
            "$.context.verdict": "verdict",
            "$.context.score":   "score",
        });
        let mut projected = Map::new();
        projected.insert("$.context.verdict".into(), json!("approved"));
        projected.insert("$.context.score".into(), json!(150));

        let err = validate_outputs_against_snippet(&snippet_outputs, &output_paths, &projected)
            .expect_err("both outputs invalid");
        let slots: Vec<&str> = err.iter().map(|v| v.slot.as_str()).collect();
        assert!(
            slots.contains(&"verdict"),
            "should report verdict violation"
        );
        assert!(slots.contains(&"score"), "should report score violation");
        assert_eq!(
            err.len(),
            2,
            "both violations collected, not short-circuited"
        );
    }

    #[test]
    fn validate_outputs_against_snippet_skips_unbound_cap_outputs() {
        // Cap declares `findings` but host doesn't bind it via use.outputs.
        // That's allowed — host chose to ignore it. Validate only the
        // bound ones.
        let snippet_outputs = json!({
            "verdict":  { "type": "string", "enum": ["pass", "fail"] },
            "findings": { "type": "array" }
        });
        let output_paths = json!({ "$.context.verdict": "verdict" });
        let mut projected = Map::new();
        projected.insert("$.context.verdict".into(), json!("pass"));

        validate_outputs_against_snippet(&snippet_outputs, &output_paths, &projected)
            .expect("only bound outputs validated");
    }

    // ── Deterministic-repair rung (P12 R3.1) ──────────────────────────────

    #[test]
    fn repair_coerces_explicit_null_to_empty_array_via_type() {
        // The exact dogfood bug: a commodity model emitted `artifacts: null`
        // for a `type: array` output. Repair must coerce it to `[]`.
        let snippet_outputs = json!({ "artifacts": { "type": "array" } });
        let output_paths = json!({ "$.context.artifacts": "artifacts" });
        let mut projected = Map::new();
        projected.insert("$.context.artifacts".into(), Value::Null);

        let repaired =
            repair_outputs_against_snippet(&snippet_outputs, &output_paths, &mut projected);
        assert_eq!(projected.get("$.context.artifacts"), Some(&json!([])));
        assert_eq!(repaired, vec!["artifacts".to_string()]);
    }

    #[test]
    fn repair_uses_explicit_default_over_type() {
        let snippet_outputs = json!({ "tags": { "type": "array", "default": ["seed"] } });
        let output_paths = json!({ "$.context.tags": "tags" });
        let mut projected = Map::new();
        projected.insert("$.context.tags".into(), Value::Null);

        repair_outputs_against_snippet(&snippet_outputs, &output_paths, &mut projected);
        assert_eq!(projected.get("$.context.tags"), Some(&json!(["seed"])));
    }

    #[test]
    fn repair_coerces_null_to_empty_object() {
        let snippet_outputs = json!({ "meta": { "type": "object" } });
        let output_paths = json!({ "$.context.meta": "meta" });
        let mut projected = Map::new();
        projected.insert("$.context.meta".into(), Value::Null);

        repair_outputs_against_snippet(&snippet_outputs, &output_paths, &mut projected);
        assert_eq!(projected.get("$.context.meta"), Some(&json!({})));
    }

    #[test]
    fn repair_handles_nullable_union_type() {
        let snippet_outputs = json!({ "items": { "type": ["array", "null"] } });
        let output_paths = json!({ "$.context.items": "items" });
        let mut projected = Map::new();
        projected.insert("$.context.items".into(), Value::Null);

        repair_outputs_against_snippet(&snippet_outputs, &output_paths, &mut projected);
        assert_eq!(projected.get("$.context.items"), Some(&json!([])));
    }

    #[test]
    fn repair_leaves_null_scalar_alone() {
        // A null string has no unambiguous empty repair (empty string is a
        // distinct, meaningful value) — leave it so the genuine violation
        // still surfaces at validation.
        let snippet_outputs = json!({ "verdict": { "type": "string" } });
        let output_paths = json!({ "$.context.verdict": "verdict" });
        let mut projected = Map::new();
        projected.insert("$.context.verdict".into(), Value::Null);

        let repaired =
            repair_outputs_against_snippet(&snippet_outputs, &output_paths, &mut projected);
        assert_eq!(projected.get("$.context.verdict"), Some(&Value::Null));
        assert!(repaired.is_empty());
    }

    #[test]
    fn repair_leaves_valid_non_null_value_untouched() {
        let snippet_outputs = json!({ "artifacts": { "type": "array" } });
        let output_paths = json!({ "$.context.artifacts": "artifacts" });
        let mut projected = Map::new();
        projected.insert("$.context.artifacts".into(), json!(["real"]));

        repair_outputs_against_snippet(&snippet_outputs, &output_paths, &mut projected);
        assert_eq!(projected.get("$.context.artifacts"), Some(&json!(["real"])));
    }

    #[test]
    fn repair_then_validate_passes_for_the_bug_class() {
        // End to end: null array output → repair → validation now passes.
        let snippet_outputs = json!({
            "plan":      { "type": "object" },
            "artifacts": { "type": "array" }
        });
        let output_paths = json!({
            "$.context.plan":      "plan",
            "$.context.artifacts": "artifacts",
        });
        let mut projected = Map::new();
        projected.insert("$.context.plan".into(), json!({ "deliverables": [] }));
        projected.insert("$.context.artifacts".into(), Value::Null);

        repair_outputs_against_snippet(&snippet_outputs, &output_paths, &mut projected);
        validate_outputs_against_snippet(&snippet_outputs, &output_paths, &projected)
            .expect("after repair, the null-array output validates");
    }
}
