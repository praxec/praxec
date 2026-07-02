use anyhow::anyhow;
use anyhow::bail;
use serde_json::Value;

/// Walk the schema's `properties` and fill in any `default` for keys missing
/// from `value`. Recurses into nested object properties so defaults apply at
/// any depth. No-ops if schema or value isn't an object — keeps the caller
/// free of pre-checks.
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
            Some(child) => apply_schema_defaults(Some(prop_schema), child),
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

    let validator = jsonschema::validator_for(schema)
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
