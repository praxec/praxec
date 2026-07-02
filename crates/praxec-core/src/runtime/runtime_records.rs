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
    let Some(blackboard) = definition.get("blackboard").and_then(Value::as_object) else {
        return Ok(());
    };
    let context_obj = context.as_object();
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
        let validator = match jsonschema::validator_for(slot_schema) {
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
    Ok(())
}
