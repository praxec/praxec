use serde_json::Value;

/// SPEC §7.2 — compute the per-transition delta of the workflow's
/// `context` so transition records can carry the structural diff. Cumulative
/// replay of these deltas reconstructs `context` at any past `seq` (§7.5).
///
/// Semantics: any key whose value differs between pre and post is included
/// (new keys → their post value; mutated keys → the new value; deleted keys
/// → explicit `null`). Returns an empty object when there is no diff or
/// when either side is not an object.
pub(crate) fn blackboard_delta(pre: &Value, post: &Value) -> Value {
    let pre_obj = pre.as_object();
    let post_obj = post.as_object();
    let mut delta = serde_json::Map::new();
    if let Some(post_obj) = post_obj {
        for (k, v) in post_obj {
            match pre_obj.and_then(|m| m.get(k)) {
                Some(prev) if prev == v => {}
                _ => {
                    delta.insert(k.clone(), v.clone());
                }
            }
        }
    }
    if let Some(pre_obj) = pre_obj {
        for k in pre_obj.keys() {
            if !post_obj.is_some_and(|m| m.contains_key(k)) {
                delta.insert(k.clone(), Value::Null);
            }
        }
    }
    Value::Object(delta)
}

/// SPEC §6.2 — validate `output:` writes against any *typed* blackboard slot.
///
/// Walks every key the transition's `output:` writes; if that key is declared
/// in the workflow's `blackboard:` map with a JSON-Schema fragment (object
/// form), the post-write value must conform. Returns the offending `(slot,
/// reason)` on the first violation so the caller can surface a
/// `BLACKBOARD_TYPE_ERROR` rejection BEFORE the transition advances.
///
/// Returns `Ok(())` when the blackboard is absent, in array form (no per-slot
/// schemas declared), or when the slot is undeclared / declared bare. Undeclared
/// writes are caught separately by `check` as a warning (SPEC §11) — this
/// check is purely the typed-slot guarantee.
pub(crate) fn validate_blackboard_writes(
    definition: &Value,
    output_mapping: Option<&Value>,
    context: &Value,
) -> Result<(), (String, String)> {
    let Some(mapping) = output_mapping.and_then(Value::as_object) else {
        return Ok(());
    };
    let context_obj = context.as_object();
    // L1 envelope: typed-slot schema validation (only when a `blackboard:` map
    // declares per-slot schemas).
    if let Some(blackboard) = definition.get("blackboard").and_then(Value::as_object) {
        for slot in mapping.keys() {
            let Some(slot_schema) = blackboard.get(slot) else {
                continue;
            };
            // Bare-name declarations in object form are an empty object — nothing
            // to validate. Only fragments that actually declare structure (a
            // `type`, properties, etc.) trigger validation.
            if !slot_schema.is_object() {
                continue;
            }
            let Some(schema_obj) = slot_schema.as_object() else {
                continue;
            };
            if schema_obj.is_empty() {
                continue;
            }
            let value = context_obj
                .and_then(|o| o.get(slot))
                .cloned()
                .unwrap_or(Value::Null);
            // Registry-aware (strictly widening): a `hop_slot:`-injected typed slot
            // carries `{ "$ref": "praxec://hop#/$defs/<name>Out" }`, so the shipped
            // HOP vocabulary must resolve here for the existing seam to enforce the
            // slot output. Plain blackboard schemas behave exactly as before.
            let validator = match crate::hop::compile_validator(slot_schema) {
                Ok(v) => v,
                Err(e) => {
                    return Err((slot.clone(), format!("invalid blackboard schema: {e}")));
                }
            };
            if !validator.is_valid(&value) {
                let errs: Vec<String> = validator
                    .iter_errors(&value)
                    .map(|e| e.to_string())
                    .collect();
                return Err((slot.clone(), errs.join("; ")));
            }
        }
    }
    // L2 SchemaBound inner-value validation (Spec A.1 §4.3), scoped to
    // `finding.fix`. Runs on every output write regardless of typed-slot
    // declaration — a `finding.fix` must resolve wherever it is produced.
    validate_schema_bound_values(definition, mapping, context)
}

/// Spec A.1 §4.3 — L2 validation of the ONE v1 SchemaBound extension point:
/// `finding.fix`. The envelope (`schema_ref` + `value` present) is enforced by
/// L1; L2 resolves each present `finding.fix.schema_ref` against the registered
/// `schemas:` map (stamped as `_schemasRegistry` at load) and validates the
/// otherwise-opaque inner `value`.
///
/// Fail-fast (returns the offending `(slot, reason)` for a `BLACKBOARD_TYPE_ERROR`):
/// - `schema_ref` not registered (defense-in-depth — the closed-world load check
///   makes this unreachable for honest configs, but the invariant must not
///   depend on producer discipline);
/// - inner `value` invalid against the registered schema.
///
/// Scope is strictly `finding.fix`: only values carrying a `findings` array are
/// inspected, and only each finding's `fix`.
fn validate_schema_bound_values(
    definition: &Value,
    mapping: &serde_json::Map<String, Value>,
    context: &Value,
) -> Result<(), (String, String)> {
    let registry = definition
        .get("_schemasRegistry")
        .and_then(Value::as_object);
    let context_obj = context.as_object();
    for slot in mapping.keys() {
        let Some(value) = context_obj.and_then(|o| o.get(slot)) else {
            continue;
        };
        let Some(findings) = value.get("findings").and_then(Value::as_array) else {
            continue;
        };
        for (i, finding) in findings.iter().enumerate() {
            let Some(fix) = finding.get("fix") else {
                continue;
            };
            // The envelope (L1) guarantees `schema_ref` is a present string on a
            // slot-out; guard anyway (L2 runs generically).
            let Some(schema_ref) = fix.get("schema_ref").and_then(Value::as_str) else {
                continue;
            };
            let inner_value = fix.get("value").cloned().unwrap_or(Value::Null);
            let Some(schema) = registry.and_then(|r| r.get(schema_ref)) else {
                let mut known: Vec<&str> = registry
                    .map(|r| r.keys().map(String::as_str).collect())
                    .unwrap_or_default();
                known.sort_unstable();
                return Err((
                    slot.clone(),
                    format!(
                        "SCHEMA_BOUND_VIOLATION: finding[{i}].fix.schema_ref '{schema_ref}' is not \
                         a registered schema; registered: [{}]",
                        known.join(", ")
                    ),
                ));
            };
            let validator = match crate::hop::compile_validator(schema) {
                Ok(v) => v,
                Err(e) => {
                    return Err((
                        slot.clone(),
                        format!(
                            "SCHEMA_BOUND_VIOLATION: registered schema '{schema_ref}' does not \
                             compile: {e}"
                        ),
                    ));
                }
            };
            if !validator.is_valid(&inner_value) {
                let errs: Vec<String> = validator
                    .iter_errors(&inner_value)
                    .map(|e| e.to_string())
                    .collect();
                return Err((
                    slot.clone(),
                    format!(
                        "SCHEMA_BOUND_VIOLATION: finding[{i}].fix.value violates registered schema \
                         '{schema_ref}': {}",
                        errs.join("; ")
                    ),
                ));
            }
        }
    }
    Ok(())
}
