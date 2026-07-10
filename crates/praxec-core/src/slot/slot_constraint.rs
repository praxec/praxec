//! SPEC §28 — declarative slot constraints, evaluated at write time.
//!
//! Slot declarations can carry a `constraint:` field that describes a
//! predicate over the slot's value. The runtime evaluates the predicate
//! WHEN the slot is written (executor output → blackboard merge) so
//! violations surface at the precise point of harm — not later via a
//! downstream guard read.
//!
//! ## Why this exists
//!
//! Without slot constraints, "this slot may only ever hold X" turns
//! into an external verifier script the operator writes per-slot. That
//! is: (a) procedural rather than declarative, (b) invisible at
//! authoring time, (c) reports failure via script exit code rather than
//! a structured `SLOT_CONSTRAINT_VIOLATED` event.
//!
//! ## What this is NOT
//!
//! JSON Schema overlap — `pattern`, `minimum`, `maximum`, `minLength`,
//! `maxLength`, `enum` — is handled by the existing typed-slot schema
//! validator (see `validate_blackboard_writes` in `runtime_records.rs`).
//! Constraint kinds in this module are deliberately scoped to things
//! JSON Schema CANNOT express:
//! - file-path allowlist with glob patterns (`path_allowlist`)
//! - subset-of dynamic-path reference (`subset_of`)
//!
//! ## Evaluation
//!
//! [`evaluate_constraints`] is called by `runtime_submit::submit` after
//! schema validation passes. The first failed constraint short-circuits
//! with `SLOT_CONSTRAINT_VIOLATED` naming the slot, constraint kind, and
//! offending value.

use anyhow::{Result, anyhow};
use globset::{Glob, GlobBuilder, GlobSet, GlobSetBuilder};
use serde_json::{Map, Value};

/// One constraint failure for a single slot.
#[derive(Debug)]
pub struct ConstraintViolation {
    pub slot: String,
    pub constraint_kind: String,
    pub message: String,
}

/// SPEC §28 — evaluate every constraint declared on every blackboard
/// slot written by this transition. Returns the FIRST violation
/// encountered so the caller surfaces a single, precise rejection.
///
/// `definition` is the workflow snapshot (carries the `blackboard:` and
/// per-state `slots:` blocks).
/// `state` is the from-state — state-local slot decls are checked too.
/// `context` is the POST-merge context (the values being written).
pub fn evaluate_constraints(
    definition: &Value,
    state: &str,
    context: &Value,
) -> Result<(), ConstraintViolation> {
    let context_obj = match context.as_object() {
        Some(o) => o,
        None => return Ok(()),
    };

    // Collect slot declarations from workflow-level + state-local.
    // We don't dedupe: a single slot name should only appear in ONE
    // place (cross-scope collision is INVALID_SLOT_REDECLARATION,
    // caught by the validator). At runtime we just check both.
    let workflow_slots = definition.get("blackboard").and_then(Value::as_object);
    let state_slots = definition
        .pointer(&format!(
            "/states/{}/slots",
            crate::runtime::runtime_links::pointer_escape(state)
        ))
        .and_then(Value::as_object);

    for (slot_name, value) in context_obj {
        let decl = workflow_slots
            .and_then(|m| m.get(slot_name))
            .or_else(|| state_slots.and_then(|m| m.get(slot_name)));
        let Some(decl) = decl else {
            continue;
        };
        let Some(constraint) = decl.get("constraint").and_then(Value::as_object) else {
            continue;
        };
        check_constraint(slot_name, constraint, value, context)?;
    }
    Ok(())
}

/// Check every constraint kind declared on one slot. Multiple kinds in
/// the same constraint block ALL must pass — they compose conjunctively.
fn check_constraint(
    slot: &str,
    constraint: &Map<String, Value>,
    value: &Value,
    context: &Value,
) -> Result<(), ConstraintViolation> {
    for (kind, params) in constraint {
        match kind.as_str() {
            "path_allowlist" => check_path_allowlist(slot, params, value)?,
            "subset_of" => check_subset_of(slot, params, value, context)?,
            other => {
                return Err(ConstraintViolation {
                    slot: slot.to_string(),
                    constraint_kind: other.to_string(),
                    message: format!(
                        "SLOT_CONSTRAINT_VIOLATED: unknown constraint kind '{other}' on slot \
                         '{slot}'. Supported: path_allowlist, subset_of. (Use slot's existing \
                         `type:` JSON Schema for regex / min / max / length / enum.)"
                    ),
                });
            }
        }
    }
    Ok(())
}

/// `path_allowlist: { allow: [<glob>...], deny: [<glob>...] }` — value
/// must be a JSON array of strings; every element must match at least
/// one `allow:` glob AND no `deny:` glob.
///
/// Empty `allow:` is rejected at config-load (a constraint that allows
/// everything is misconfiguration, not a feature).
fn check_path_allowlist(
    slot: &str,
    params: &Value,
    value: &Value,
) -> Result<(), ConstraintViolation> {
    let params_obj = params.as_object().ok_or_else(|| ConstraintViolation {
        slot: slot.to_string(),
        constraint_kind: "path_allowlist".into(),
        message: format!(
            "SLOT_CONSTRAINT_VIOLATED: constraint.path_allowlist on slot '{slot}' must be an \
             object with `allow: [<glob>...]` (required) and optional `deny: [<glob>...]`."
        ),
    })?;
    let arr = match value.as_array() {
        Some(a) => a,
        None => {
            return Err(ConstraintViolation {
                slot: slot.to_string(),
                constraint_kind: "path_allowlist".into(),
                message: format!(
                    "SLOT_CONSTRAINT_VIOLATED: slot '{slot}' has path_allowlist constraint but \
                     its value is not an array (got: {}); declare `type: array` on the slot.",
                    short_kind(value)
                ),
            });
        }
    };
    let allow_set = build_glob_set(params_obj.get("allow"), slot, "allow")?;
    if allow_set.is_empty() {
        return Err(ConstraintViolation {
            slot: slot.to_string(),
            constraint_kind: "path_allowlist".into(),
            message: format!(
                "SLOT_CONSTRAINT_VIOLATED: constraint.path_allowlist on slot '{slot}' has \
                 empty `allow:` — an allow-everything constraint is misconfiguration. \
                 Declare at least one glob, or remove the constraint."
            ),
        });
    }
    let deny_set = if params_obj.contains_key("deny") {
        Some(build_glob_set(params_obj.get("deny"), slot, "deny")?)
    } else {
        None
    };

    for (idx, item) in arr.iter().enumerate() {
        let s = match item.as_str() {
            Some(s) => s,
            None => {
                return Err(ConstraintViolation {
                    slot: slot.to_string(),
                    constraint_kind: "path_allowlist".into(),
                    message: format!(
                        "SLOT_CONSTRAINT_VIOLATED: slot '{slot}' element at index {idx} is \
                         not a string (got: {}); path_allowlist requires array of path strings.",
                        short_kind(item)
                    ),
                });
            }
        };
        if !allow_set.is_match(s) {
            return Err(ConstraintViolation {
                slot: slot.to_string(),
                constraint_kind: "path_allowlist".into(),
                message: format!(
                    "SLOT_CONSTRAINT_VIOLATED: slot '{slot}' element '{s}' (index {idx}) does \
                     not match any `allow:` glob. Allowed: {:?}",
                    params_obj
                        .get("allow")
                        .cloned()
                        .unwrap_or(Value::Array(vec![]))
                ),
            });
        }
        if let Some(deny) = &deny_set {
            if deny.is_match(s) {
                return Err(ConstraintViolation {
                    slot: slot.to_string(),
                    constraint_kind: "path_allowlist".into(),
                    message: format!(
                        "SLOT_CONSTRAINT_VIOLATED: slot '{slot}' element '{s}' (index {idx}) \
                         matched a `deny:` glob. Denied: {:?}",
                        params_obj
                            .get("deny")
                            .cloned()
                            .unwrap_or(Value::Array(vec![]))
                    ),
                });
            }
        }
    }
    Ok(())
}

fn build_glob_set(
    list: Option<&Value>,
    slot: &str,
    field: &str,
) -> Result<GlobSet, ConstraintViolation> {
    let mut builder = GlobSetBuilder::new();
    if let Some(arr) = list.and_then(Value::as_array) {
        for (i, pat) in arr.iter().enumerate() {
            let s = pat.as_str().ok_or_else(|| ConstraintViolation {
                slot: slot.to_string(),
                constraint_kind: "path_allowlist".into(),
                message: format!(
                    "SLOT_CONSTRAINT_VIOLATED: slot '{slot}' constraint.path_allowlist.{field} \
                     element {i} is not a string (got: {}).",
                    short_kind(pat)
                ),
            })?;
            // SECURITY: `literal_separator(true)` makes `*` match within a
            // single path segment only — `allowed/*` matches `allowed/a` but
            // NOT `allowed/a/b`. Without it a path_allowlist is silently too
            // permissive (a single `*` spans `/` and escapes the intended
            // directory). Use `**` explicitly when segment-spanning is wanted.
            let glob = GlobBuilder::new(s)
                .literal_separator(true)
                .build()
                .map_err(|e| ConstraintViolation {
                    slot: slot.to_string(),
                    constraint_kind: "path_allowlist".into(),
                    message: format!(
                        "SLOT_CONSTRAINT_VIOLATED: slot '{slot}' \
                         constraint.path_allowlist.{field}[{i}] '{s}' is not a valid glob \
                         pattern: {e}"
                    ),
                })?;
            builder.add(glob);
        }
    }
    builder.build().map_err(|e| ConstraintViolation {
        slot: slot.to_string(),
        constraint_kind: "path_allowlist".into(),
        message: format!(
            "SLOT_CONSTRAINT_VIOLATED: slot '{slot}' constraint.path_allowlist.{field} could \
             not be compiled into a glob set: {e}"
        ),
    })
}

/// `subset_of: "$.context.declared_scope"` — value must be an array;
/// every element must appear in the array resolved from the dynamic
/// reference. Unset reference is fail-fast (poka-yoke vs silent pass).
fn check_subset_of(
    slot: &str,
    params: &Value,
    value: &Value,
    context: &Value,
) -> Result<(), ConstraintViolation> {
    let ref_path = params.as_str().ok_or_else(|| ConstraintViolation {
        slot: slot.to_string(),
        constraint_kind: "subset_of".into(),
        message: format!(
            "SLOT_CONSTRAINT_VIOLATED: constraint.subset_of on slot '{slot}' must be a string \
             path (e.g. \"$.context.declared_scope\"); got: {}",
            short_kind(params)
        ),
    })?;
    let arr = value.as_array().ok_or_else(|| ConstraintViolation {
        slot: slot.to_string(),
        constraint_kind: "subset_of".into(),
        message: format!(
            "SLOT_CONSTRAINT_VIOLATED: slot '{slot}' has subset_of constraint but its value is \
             not an array (got: {}).",
            short_kind(value)
        ),
    })?;

    let resolved = resolve_path(ref_path, context).ok_or_else(|| ConstraintViolation {
        slot: slot.to_string(),
        constraint_kind: "subset_of".into(),
        message: format!(
            "SLOT_CONSTRAINT_VIOLATED: slot '{slot}' subset_of references path '{ref_path}' but \
             that path is unset / unresolvable. Constraint must reference an already-populated \
             slot; check workflow ordering."
        ),
    })?;

    let ref_arr = resolved.as_array().ok_or_else(|| ConstraintViolation {
        slot: slot.to_string(),
        constraint_kind: "subset_of".into(),
        message: format!(
            "SLOT_CONSTRAINT_VIOLATED: slot '{slot}' subset_of references path '{ref_path}' \
             which resolved to a non-array value ({}).",
            short_kind(&resolved)
        ),
    })?;

    for (idx, element) in arr.iter().enumerate() {
        if !ref_arr.contains(element) {
            return Err(ConstraintViolation {
                slot: slot.to_string(),
                constraint_kind: "subset_of".into(),
                message: format!(
                    "SLOT_CONSTRAINT_VIOLATED: slot '{slot}' element at index {idx} (value: \
                     {element}) is not present in the reference array at '{ref_path}'."
                ),
            });
        }
    }
    Ok(())
}

fn resolve_path(path: &str, context: &Value) -> Option<Value> {
    // Minimal path resolver — same shape as guards.rs / mapping.rs.
    // Only `$.context.*` and `$.workflow.input.*` supported in
    // subset_of references; everything else is structural ambiguity.
    if let Some(p) = path.strip_prefix("$.context.") {
        let ptr = format!("/{}", p.replace('.', "/"));
        return context.pointer(&ptr).cloned();
    }
    None
}

fn short_kind(v: &Value) -> &'static str {
    match v {
        Value::Null => "null",
        Value::Bool(_) => "boolean",
        Value::Number(_) => "number",
        Value::String(_) => "string",
        Value::Array(_) => "array",
        Value::Object(_) => "object",
    }
}

/// SPEC §28 — load-time validation: every constraint declaration on a
/// slot must be well-formed (compilable globs, no duplicate kinds,
/// non-empty `allow:` for path_allowlist).
///
/// Called once per workflow at config load. The runtime evaluator
/// trusts that constraints have been well-formed already.
pub fn validate_constraints_in_definition(definition: &Value) -> Result<()> {
    if let Some(slots) = definition.get("blackboard").and_then(Value::as_object) {
        for (name, decl) in slots {
            validate_one_slot_constraint(name, decl)?;
        }
    } else if let Some(arr) = definition.get("blackboard").and_then(Value::as_array) {
        reject_constraint_in_array_blackboard("blackboard", arr)?;
    }
    if let Some(states) = definition.get("states").and_then(Value::as_object) {
        for (state_name, state_def) in states {
            if let Some(slots) = state_def.get("slots").and_then(Value::as_object) {
                for (name, decl) in slots {
                    validate_one_slot_constraint(&format!("{state_name}.{name}"), decl)?;
                }
            } else if let Some(arr) = state_def.get("slots").and_then(Value::as_array) {
                reject_constraint_in_array_blackboard(&format!("states.{state_name}.slots"), arr)?;
            }
        }
    }
    Ok(())
}

/// SPEC §28 — a `constraint:` can only be honored when the blackboard is in
/// OBJECT form (slot-name → decl), because [`evaluate_constraints`] resolves
/// declarations via `Value::as_object`. Under the ARRAY form there is no
/// per-slot decl the runtime can read, so a `constraint:` riding on an array
/// element would silently never evaluate. Reject it at load (poka-yoke) rather
/// than letting the author believe a constraint is enforced when it is not.
fn reject_constraint_in_array_blackboard(scope: &str, arr: &[Value]) -> Result<()> {
    for el in arr {
        let has_constraint = el
            .as_object()
            .map(|o| o.contains_key("constraint"))
            .unwrap_or(false);
        if has_constraint {
            let named = el
                .get("name")
                .and_then(Value::as_str)
                .map(|n| format!(" (slot '{n}')"))
                .unwrap_or_default();
            return Err(anyhow!(
                "CONSTRAINT_REQUIRES_OBJECT_BLACKBOARD: '{scope}' is declared in array form but \
                 carries a `constraint:`{named} — constraints cannot be honored under an \
                 array-form blackboard. Declare the blackboard in object form (slot-name → decl) \
                 to use constraints."
            ));
        }
    }
    Ok(())
}

fn validate_one_slot_constraint(slot_id: &str, decl: &Value) -> Result<()> {
    let Some(constraint) = decl.get("constraint").and_then(Value::as_object) else {
        return Ok(());
    };
    if constraint.is_empty() {
        return Err(anyhow!(
            "INVALID_CONSTRAINT_DECLARATION: slot '{slot_id}' declares an empty `constraint:` \
             block — either remove it or add a constraint kind"
        ));
    }
    for (kind, params) in constraint {
        match kind.as_str() {
            "path_allowlist" => {
                let obj = params.as_object().ok_or_else(|| {
                    anyhow!(
                        "INVALID_CONSTRAINT_DECLARATION: slot '{slot_id}' path_allowlist must be \
                         an object with `allow:` and optional `deny:`"
                    )
                })?;
                let allow = obj.get("allow").and_then(Value::as_array).ok_or_else(|| {
                    anyhow!(
                        "INVALID_CONSTRAINT_DECLARATION: slot '{slot_id}' path_allowlist requires \
                         a non-empty `allow: [<glob>...]` (empty/missing is misconfiguration)"
                    )
                })?;
                if allow.is_empty() {
                    return Err(anyhow!(
                        "INVALID_CONSTRAINT_DECLARATION: slot '{slot_id}' path_allowlist.allow is \
                         empty — an allow-everything constraint is misconfiguration, declare at \
                         least one glob or remove the constraint"
                    ));
                }
                // Validate all globs compile.
                for (i, pat) in allow.iter().enumerate() {
                    let s = pat.as_str().ok_or_else(|| {
                        anyhow!(
                            "INVALID_CONSTRAINT_DECLARATION: slot '{slot_id}' path_allowlist.allow[{i}] \
                             is not a string"
                        )
                    })?;
                    Glob::new(s).map_err(|e| {
                        anyhow!(
                            "INVALID_CONSTRAINT_DECLARATION: slot '{slot_id}' \
                             path_allowlist.allow[{i}] '{s}' is not a valid glob: {e}"
                        )
                    })?;
                }
                if let Some(deny) = obj.get("deny").and_then(Value::as_array) {
                    for (i, pat) in deny.iter().enumerate() {
                        let s = pat.as_str().ok_or_else(|| {
                            anyhow!(
                                "INVALID_CONSTRAINT_DECLARATION: slot '{slot_id}' \
                                 path_allowlist.deny[{i}] is not a string"
                            )
                        })?;
                        Glob::new(s).map_err(|e| {
                            anyhow!(
                                "INVALID_CONSTRAINT_DECLARATION: slot '{slot_id}' \
                                 path_allowlist.deny[{i}] '{s}' is not a valid glob: {e}"
                            )
                        })?;
                    }
                }
            }
            "subset_of" => {
                if !params.is_string() {
                    return Err(anyhow!(
                        "INVALID_CONSTRAINT_DECLARATION: slot '{slot_id}' subset_of must be a \
                         string path (e.g. \"$.context.declared_scope\")"
                    ));
                }
            }
            other => {
                return Err(anyhow!(
                    "INVALID_CONSTRAINT_DECLARATION: slot '{slot_id}' unknown constraint kind \
                     '{other}'. Supported: path_allowlist, subset_of. (Use the slot's existing \
                     `type:` JSON Schema for regex / min / max / length / enum.)"
                ));
            }
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    // GLOB-LITERAL-SEPARATOR — `allowed/*` must match a single segment only.
    #[test]
    fn path_allowlist_star_does_not_span_separator() {
        let value = json!(["allowed/a", "allowed/a/b"]);
        let params = json!({ "allow": ["allowed/*"] });
        let err = check_path_allowlist("paths", &params, &value)
            .expect_err("allowed/a/b must be rejected by allowed/*");
        assert!(
            err.message.contains("allowed/a/b"),
            "rejection should name the offending nested path: {}",
            err.message
        );
    }

    #[test]
    fn path_allowlist_single_segment_passes() {
        let value = json!(["allowed/a"]);
        let params = json!({ "allow": ["allowed/*"] });
        assert!(check_path_allowlist("paths", &params, &value).is_ok());
    }

    #[test]
    fn path_allowlist_double_star_spans_separator() {
        let value = json!(["allowed/a/b/c"]);
        let params = json!({ "allow": ["allowed/**"] });
        assert!(
            check_path_allowlist("paths", &params, &value).is_ok(),
            "`**` is the explicit opt-in for segment-spanning"
        );
    }

    // CMP-010 — a constraint riding on an array-form blackboard is rejected.
    #[test]
    fn constraint_under_array_blackboard_is_rejected() {
        let def = json!({
            "blackboard": [
                { "name": "files", "constraint": { "path_allowlist": { "allow": ["src/*"] } } }
            ]
        });
        let err = validate_constraints_in_definition(&def)
            .expect_err("array-form blackboard with constraint must be rejected");
        assert!(
            err.to_string()
                .contains("CONSTRAINT_REQUIRES_OBJECT_BLACKBOARD"),
            "{err}"
        );
    }

    #[test]
    fn array_blackboard_without_constraint_is_ok() {
        let def = json!({ "blackboard": ["files", "verdict"] });
        assert!(validate_constraints_in_definition(&def).is_ok());
    }

    #[test]
    fn constraint_under_array_state_slots_is_rejected() {
        let def = json!({
            "states": {
                "s1": {
                    "slots": [
                        { "name": "x", "constraint": { "subset_of": "$.context.scope" } }
                    ]
                }
            }
        });
        let err = validate_constraints_in_definition(&def)
            .expect_err("array-form state slots with constraint must be rejected");
        assert!(
            err.to_string()
                .contains("CONSTRAINT_REQUIRES_OBJECT_BLACKBOARD"),
            "{err}"
        );
    }
}
