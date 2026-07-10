//! Synthesize a type-appropriate dummy value from a JSON Schema fragment, for
//! satisfying a capability's declared `snippet.outputs`.

use serde_json::{Value, json};

/// A minimal valid value for the given JSON Schema fragment.
pub fn dummy_for_schema(schema: &Value) -> Value {
    // enum wins (first value)
    if let Some(first) = schema
        .get("enum")
        .and_then(|e| e.as_array())
        .and_then(|a| a.first())
    {
        return first.clone();
    }

    match schema.get("type").and_then(|t| t.as_str()) {
        Some("string") => {
            // Honor `minLength` so length-constrained inputs (e.g. a plan body
            // declared `minLength: 200`) produce a *satisfying* dummy rather than
            // a too-short value that trips INPUT_SCHEMA_VIOLATION.
            let min = schema.get("minLength").and_then(Value::as_u64).unwrap_or(0) as usize;
            let mut s = String::from("fuzz");
            while s.len() < min {
                s.push('x');
            }
            json!(s)
        }
        Some("number") | Some("integer") => {
            // Honor `minimum` so bound-constrained numerics satisfy their schema.
            let min = schema.get("minimum").and_then(Value::as_i64).unwrap_or(1);
            json!(min)
        }
        Some("boolean") => json!(true),
        Some("array") => {
            let min = schema.get("minItems").and_then(|m| m.as_u64()).unwrap_or(0) as usize;
            let item_schema = schema.get("items");
            let item = item_schema.map(dummy_for_schema).unwrap_or(Value::Null);
            Value::Array(std::iter::repeat_n(item, min).collect())
        }
        Some("object") => {
            let mut m = serde_json::Map::new();
            if let Some(props) = schema.get("properties").and_then(|p| p.as_object()) {
                let required: Vec<&str> = schema
                    .get("required")
                    .and_then(|r| r.as_array())
                    .map(|a| a.iter().filter_map(|v| v.as_str()).collect())
                    .unwrap_or_default();
                for (name, psch) in props {
                    // emit all properties if none are marked required; else only required ones
                    if required.is_empty() || required.contains(&name.as_str()) {
                        m.insert(name.clone(), dummy_for_schema(psch));
                    }
                }
            }
            Value::Object(m)
        }
        _ => Value::Null,
    }
}

/// Build an object populating EVERY declared property of an object schema
/// (optional ones included), each with a type-appropriate dummy.
///
/// Unlike [`dummy_for_schema`], which emits only *required* properties for an
/// `object`, this is for **input seeding** (workflow `inputSchema`, transition
/// `arguments`): a downstream output mapping may copy any property — required or
/// not — onto a typed blackboard slot, so all must carry a value. Returns `{}`
/// when the schema declares no `properties`.
pub fn dummy_all_properties(schema: &Value) -> Value {
    let Some(props) = schema.get("properties").and_then(|p| p.as_object()) else {
        return json!({});
    };
    let mut m = serde_json::Map::new();
    for (name, psch) in props {
        m.insert(name.clone(), dummy_for_schema(psch));
    }
    Value::Object(m)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn all_properties_includes_optional() {
        // `dummy_for_schema` would emit only `a` (required); this emits both.
        let schema = json!({
            "type": "object",
            "required": ["a"],
            "properties": { "a": { "type": "string" }, "b": { "type": "string" } }
        });
        let v = dummy_all_properties(&schema);
        assert!(v.get("a").is_some());
        assert!(v.get("b").is_some(), "optional property must be seeded too");
    }

    #[test]
    fn enum_picks_first() {
        assert_eq!(
            dummy_for_schema(&json!({ "type": "string", "enum": ["S1","S2"] })),
            json!("S1")
        );
    }

    #[test]
    fn primitives() {
        assert_eq!(
            dummy_for_schema(&json!({ "type": "string" })),
            json!("fuzz")
        );
        assert_eq!(dummy_for_schema(&json!({ "type": "number" })), json!(1));
        assert_eq!(dummy_for_schema(&json!({ "type": "integer" })), json!(1));
        assert_eq!(dummy_for_schema(&json!({ "type": "boolean" })), json!(true));
        assert_eq!(dummy_for_schema(&json!({ "type": "array" })), json!([]));
        assert_eq!(dummy_for_schema(&json!({ "type": "object" })), json!({}));
    }

    #[test]
    fn string_honors_min_length() {
        let s = dummy_for_schema(&json!({ "type": "string", "minLength": 200 }));
        assert!(s.as_str().unwrap().len() >= 200, "must satisfy minLength");
    }

    #[test]
    fn short_min_length_keeps_fuzz() {
        assert_eq!(
            dummy_for_schema(&json!({ "type": "string", "minLength": 2 })),
            json!("fuzz")
        );
    }

    #[test]
    fn integer_honors_minimum() {
        assert_eq!(
            dummy_for_schema(&json!({ "type": "integer", "minimum": 5 })),
            json!(5)
        );
    }

    #[test]
    fn unknown_is_null() {
        assert_eq!(dummy_for_schema(&json!({})), Value::Null);
    }

    #[test]
    fn array_min_items_with_item_schema() {
        let schema = json!({
            "type": "array",
            "minItems": 3,
            "items": { "type": "string" }
        });
        assert_eq!(dummy_for_schema(&schema), json!(["fuzz", "fuzz", "fuzz"]));
    }

    #[test]
    fn object_required_fields_only() {
        let schema = json!({
            "type": "object",
            "required": ["a"],
            "properties": {
                "a": { "type": "integer" },
                "b": { "type": "string" }
            }
        });
        let result = dummy_for_schema(&schema);
        // required field 'a' must be present with value 1
        assert_eq!(result.get("a"), Some(&json!(1)));
    }

    #[test]
    fn nested_array_of_objects_candidates() {
        let schema = json!({
            "type": "array",
            "minItems": 3,
            "items": {
                "type": "object",
                "required": ["id", "confidence"],
                "properties": {
                    "id": { "type": "string" },
                    "confidence": { "type": "string", "enum": ["low", "medium", "high"] }
                }
            }
        });
        let result = dummy_for_schema(&schema);
        let arr = result.as_array().expect("should be array");
        assert_eq!(arr.len(), 3);
        for elem in arr {
            assert_eq!(elem.get("id"), Some(&json!("fuzz")));
            assert_eq!(elem.get("confidence"), Some(&json!("low")));
        }
    }
}
