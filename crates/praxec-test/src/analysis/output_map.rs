//! Reads a transition's `output:` block and reports, per context slot, where
//! its value comes from. P1 only needs the passthrough case
//! (`slot: "$.output.<field>"`).

use serde_json::Value;

#[derive(Debug, PartialEq)]
pub enum OutputSource {
    /// `slot: "$.output.<field>"` — the executor output field that feeds slot.
    Field(String),
    /// A literal, an operator object, or a non-`$.output` path — not something
    /// the fuzzer can satisfy by emitting an output field.
    Other,
}

/// Returns (contextSlot, source) for each entry in `transition.output`.
/// `transition` is the resolved transition JSON object. Empty if no `output:`.
pub fn analyze_output(transition: &Value) -> Vec<(String, OutputSource)> {
    let Some(output_obj) = transition.get("output").and_then(|v| v.as_object()) else {
        return vec![];
    };

    output_obj
        .iter()
        .map(|(slot, val)| {
            let source = if let Some(s) = val.as_str() {
                if let Some(field) = s.strip_prefix("$.output.") {
                    OutputSource::Field(field.to_string())
                } else {
                    OutputSource::Other
                }
            } else {
                OutputSource::Other
            };
            (slot.clone(), source)
        })
        .collect()
}

/// Returns every context slot fed by the WHOLE executor result (`slot: "$.output"`).
///
/// This is the shape every `kind: mcp` leaf uses — the tool's result IS the
/// value. It is not a `Field`: there is no sub-path to plan, the mock's entire
/// output must be the slot's value. Callers that only understand `Field` will
/// emit `{}` here, which fails any slot declared as an array/string/number.
pub fn whole_output_slots(transition: &Value) -> Vec<String> {
    let Some(output_obj) = transition.get("output").and_then(|v| v.as_object()) else {
        return vec![];
    };
    output_obj
        .iter()
        .filter(|(_, val)| val.as_str() == Some("$.output"))
        .map(|(slot, _)| slot.clone())
        .collect()
}

/// Returns `(contextSlot, fullPathAfterOutput)` for every `slot: "$.output.<path>"` entry.
///
/// Unlike [`analyze_output`], this preserves the FULL dotted path after `$.output.`, so
/// callers can build properly nested mock output objects. For example, `"$.output.json.deployId"`
/// yields `("deployId", "json.deployId")`.
///
/// Non-`$.output.*` entries are omitted.
pub fn output_field_paths(transition: &Value) -> Vec<(String, String)> {
    let Some(output_obj) = transition.get("output").and_then(|v| v.as_object()) else {
        return vec![];
    };
    output_obj
        .iter()
        .filter_map(|(slot, val)| {
            let s = val.as_str()?;
            let full_path = s.strip_prefix("$.output.")?;
            Some((slot.clone(), full_path.to_string()))
        })
        .collect()
}

/// Insert `value` at the nested path `parts` inside `obj`, merging with any
/// existing sub-objects rather than overwriting them.
///
/// ```text
/// insert_nested({}, ["json", "deployId"], "fuzz")
///   => {"json": {"deployId": "fuzz"}}
///
/// insert_nested({"json": {"a": 1}}, ["json", "b"], 2)
///   => {"json": {"a": 1, "b": 2}}
/// ```
pub fn insert_nested(obj: &mut serde_json::Map<String, Value>, parts: &[&str], value: Value) {
    if parts.is_empty() {
        return;
    }
    if parts.len() == 1 {
        obj.entry(parts[0]).or_insert(value);
        return;
    }
    // Navigate / create the intermediate object.
    let entry = obj
        .entry(parts[0])
        .or_insert_with(|| Value::Object(serde_json::Map::new()));
    if let Some(sub) = entry.as_object_mut() {
        insert_nested(sub, &parts[1..], value);
    }
    // If the existing entry is not an object (e.g. a scalar from a prior path),
    // we leave it as-is to avoid clobbering it.
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn passthrough_field() {
        let t = json!({ "output": { "plan": "$.output.plan", "ok": true } });
        let m = analyze_output(&t);
        assert!(m.contains(&("plan".into(), OutputSource::Field("plan".into()))));
        assert!(m.contains(&("ok".into(), OutputSource::Other)));
    }

    #[test]
    fn renamed_field() {
        let t = json!({ "output": { "approved": "$.output.verdict" } });
        let m = analyze_output(&t);
        assert_eq!(
            m,
            vec![("approved".into(), OutputSource::Field("verdict".into()))]
        );
    }

    #[test]
    fn other_for_context_path_and_literal() {
        let t = json!({ "output": { "a": "$.context.x", "b": "literal", "c": 3 } });
        let m = analyze_output(&t);
        for (slot, src) in &m {
            assert_eq!(*src, OutputSource::Other, "slot {slot} should be Other");
        }
        assert_eq!(m.len(), 3);
    }

    #[test]
    fn no_output_block() {
        assert_eq!(analyze_output(&json!({})), vec![]);
    }

    #[test]
    fn output_field_paths_nested() {
        let t = json!({ "output": { "deployId": "$.output.json.deployId", "ok": true } });
        let paths = output_field_paths(&t);
        assert!(paths.contains(&("deployId".into(), "json.deployId".into())));
        assert_eq!(paths.len(), 1, "non-$.output entries are omitted");
    }

    #[test]
    fn output_field_paths_flat() {
        let t = json!({ "output": { "verdict": "$.output.verdict" } });
        let paths = output_field_paths(&t);
        assert_eq!(paths, vec![("verdict".into(), "verdict".into())]);
    }

    #[test]
    fn insert_nested_single_segment() {
        let mut obj = serde_json::Map::new();
        insert_nested(&mut obj, &["key"], json!("val"));
        assert_eq!(obj["key"], json!("val"));
    }

    #[test]
    fn insert_nested_multi_segment() {
        let mut obj = serde_json::Map::new();
        insert_nested(&mut obj, &["json", "deployId"], json!("fuzz"));
        assert_eq!(obj["json"]["deployId"], json!("fuzz"));
    }

    #[test]
    fn insert_nested_merges_sibling_paths() {
        let mut obj = serde_json::Map::new();
        insert_nested(&mut obj, &["json", "a"], json!(1));
        insert_nested(&mut obj, &["json", "b"], json!(2));
        assert_eq!(obj["json"]["a"], json!(1));
        assert_eq!(obj["json"]["b"], json!(2));
    }

    #[test]
    fn insert_nested_does_not_overwrite_existing() {
        let mut obj = serde_json::Map::new();
        insert_nested(&mut obj, &["x"], json!("first"));
        insert_nested(&mut obj, &["x"], json!("second"));
        assert_eq!(obj["x"], json!("first"), "or_insert should not overwrite");
    }
}
