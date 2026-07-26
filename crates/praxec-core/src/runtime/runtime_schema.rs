use anyhow::anyhow;
use anyhow::bail;
use serde_json::Value;

/// Walk the schema's `properties` and fill in any `default` for keys missing
/// from `value`. Recurses into nested object properties so defaults apply at
/// any depth. No-ops if schema or value isn't an object — keeps the caller
/// free of pre-checks.
///
/// A key present as an explicit `null` is treated the same as an absent key
/// (#71): when a caller maps an optional snippet input to a scope path that
/// resolves to `null` (the input was omitted), the injected `null` must not
/// defeat the schema `default` — otherwise a later `use.inputs` map-path read
/// fails as an unresolved-arg permanent error even though a default was
/// declared. `default`-over-`null` is the correct semantic for a value the
/// caller could not supply; without a `default`, a `null` is left untouched
/// (and the recursion no-ops on it).
pub(crate) fn apply_schema_defaults(schema: Option<&Value>, value: &mut Value) {
    let Some(schema) = schema else { return };
    let Some(props) = schema.get("properties").and_then(Value::as_object) else {
        return;
    };
    let Some(obj) = value.as_object_mut() else {
        return;
    };
    for (key, prop_schema) in props {
        match obj.get_mut(key) {
            None => {
                if let Some(default) = prop_schema.get("default") {
                    obj.insert(key.clone(), default.clone());
                }
            }
            Some(child) => {
                if child.is_null() {
                    if let Some(default) = prop_schema.get("default") {
                        *child = default.clone();
                        continue;
                    }
                }
                apply_schema_defaults(Some(prop_schema), child);
            }
        }
    }
}

pub(crate) fn validate_schema(
    schema: Option<&Value>,
    value: &Value,
    label: &str,
) -> anyhow::Result<()> {
    let Some(schema) = schema else {
        return Ok(());
    };

    // Registry-aware (strictly widening): resolves a `$ref` into the shipped
    // HOP vocabulary (praxec://hop) — e.g. a `hop_slot:`-injected transition
    // `inputSchema` `{ "$ref": "praxec://hop#/$defs/verifyIn" }`. Self-contained
    // schemas behave exactly as bare `validator_for`.
    let validator = crate::hop::compile_validator(schema)
        .map_err(|e| anyhow!("invalid {} schema: {}", label, e))?;
    if !validator.is_valid(value) {
        let errs: Vec<String> = validator
            .iter_errors(value)
            .map(|e| e.to_string())
            .collect();
        bail!("invalid {}: {}", label, errs.join("; "));
    }
    Ok(())
}

pub(crate) fn required_str<'a>(value: &'a Value, pointer: &str) -> anyhow::Result<&'a str> {
    value
        .pointer(pointer)
        .and_then(Value::as_str)
        .ok_or_else(|| anyhow!("required string missing at {}", pointer))
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn schema() -> Value {
        json!({
            "type": "object",
            "properties": {
                "cargo_scope": { "type": "string", "default": "--workspace" },
                "min_tests":   { "type": "integer", "default": 1 },
                "provided":    { "type": "string", "default": "d" },
                "no_default":  { "type": "string" }
            }
        })
    }

    #[test]
    fn absent_key_gets_default() {
        let mut v = json!({});
        apply_schema_defaults(Some(&schema()), &mut v);
        assert_eq!(v["cargo_scope"], json!("--workspace"));
        assert_eq!(v["min_tests"], json!(1));
    }

    #[test]
    fn present_non_null_value_is_preserved() {
        let mut v = json!({ "cargo_scope": "-p praxec-core" });
        apply_schema_defaults(Some(&schema()), &mut v);
        assert_eq!(v["cargo_scope"], json!("-p praxec-core"));
    }

    // #71 regression: an optional snippet input mapped to a scope path that
    // resolves to null (the caller omitted it) injects `null`; the schema
    // default must win over that null instead of surviving to defeat a later
    // map-path read.
    #[test]
    fn present_null_is_filled_by_default() {
        let mut v = json!({ "cargo_scope": Value::Null, "min_tests": Value::Null });
        apply_schema_defaults(Some(&schema()), &mut v);
        assert_eq!(v["cargo_scope"], json!("--workspace"));
        assert_eq!(v["min_tests"], json!(1));
    }

    #[test]
    fn present_null_without_default_is_untouched() {
        let mut v = json!({ "no_default": Value::Null });
        apply_schema_defaults(Some(&schema()), &mut v);
        assert_eq!(v["no_default"], Value::Null);
    }

    #[test]
    fn nested_object_defaults_still_apply() {
        let s = json!({
            "type": "object",
            "properties": {
                "opts": {
                    "type": "object",
                    "properties": { "retries": { "type": "integer", "default": 3 } }
                }
            }
        });
        let mut v = json!({ "opts": {} });
        apply_schema_defaults(Some(&s), &mut v);
        assert_eq!(v["opts"]["retries"], json!(3));
    }
}
