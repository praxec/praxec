//! Synthesize a type-appropriate dummy value from a JSON Schema fragment, for
//! satisfying a capability's declared `snippet.outputs`.

use std::sync::LazyLock;

use serde_json::{Value, json};

use praxec_core::hop::HOP_SCHEMA;

/// The shipped HOP vocabulary's `$defs`, parsed once.
///
/// A slot capability spells its output contract as a `$ref` into this
/// vocabulary — `{ "$ref": "praxec://hop#/$defs/verifyOut" }` — so a synthesizer
/// that cannot follow the ref cannot produce a valid verdict for ANY slot cap in
/// the pack. It would emit `null` (no `type:` key to match on) and every
/// mock-driven verify would look like a contract violation.
static HOP_DEFS: LazyLock<Value> = LazyLock::new(|| {
    serde_json::from_str::<Value>(HOP_SCHEMA)
        .ok()
        .and_then(|doc| doc.get("$defs").cloned())
        .unwrap_or(Value::Null)
});

/// Resolve a HOP `$ref` to the schema it names — both the config-side alias
/// (`praxec://hop#/$defs/<def>`) and the document-internal form
/// (`#/$defs/<def>`), since a resolved def's own properties `$ref` each other
/// (e.g. `verifyOut.status` → `#/$defs/gateStatus`).
fn resolve_hop_ref(reference: &str) -> Option<&'static Value> {
    let name = reference
        .strip_prefix("praxec://hop#/$defs/")
        .or_else(|| reference.strip_prefix("#/$defs/"))?;
    HOP_DEFS.get(name)
}

/// A minimal valid value for the given JSON Schema fragment.
pub fn dummy_for_schema(schema: &Value) -> Value {
    // Follow a HOP `$ref` before anything else — the ref IS the schema.
    if let Some(target) = schema
        .get("$ref")
        .and_then(Value::as_str)
        .and_then(resolve_hop_ref)
    {
        return dummy_for_schema(target);
    }

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
            pad_min_properties(&mut m, schema);
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

/// Pad an object dummy with filler keys until it meets `minProperties`.
///
/// A schema like `blast_radius: {type: object, minProperties: 1}` declares NO
/// `properties`, so the dummy is `{}` — which fails `minProperties: 1`. The
/// filler is arbitrary (the schema constrains only the COUNT), so any distinct
/// keys satisfy it.
fn pad_min_properties(m: &mut serde_json::Map<String, Value>, schema: &Value) {
    let min = schema
        .get("minProperties")
        .and_then(Value::as_u64)
        .unwrap_or(0) as usize;
    let mut i = 0;
    while m.len() < min {
        m.insert(format!("_fuzz_{i}"), Value::String("fuzz".to_owned()));
        i += 1;
    }
}

/// A dummy that satisfies `schema` AND includes every OPTIONAL property, all the
/// way down — for transition `arguments` and workflow `input`.
///
/// The distinction from [`dummy_for_schema`] (which emits only *required* object
/// properties) is deliberate and load-bearing in two directions:
///
/// - A `hop_slot` transition's `inputSchema` is a bare `{$ref: verifyIn}` with no
///   `properties`; this resolves the ref first, so the emitted arguments satisfy
///   the referenced contract instead of being an empty `{}`.
/// - A declared OUTPUT is often mapped from an OPTIONAL argument
///   (`summary: "$.arguments.summary"`). Emitting only required args would leave
///   that output null and fail the terminal contract — a per-edge probe must test
///   the edge as an agent that supplies everything, not one that omits optionals.
pub fn dummy_arguments(schema: &Value) -> Value {
    if let Some(target) = schema
        .get("$ref")
        .and_then(Value::as_str)
        .and_then(resolve_hop_ref)
    {
        return dummy_arguments(target);
    }
    match schema.get("type").and_then(|t| t.as_str()) {
        Some("object") => {
            let mut m = serde_json::Map::new();
            if let Some(props) = schema.get("properties").and_then(|p| p.as_object()) {
                for (name, psch) in props {
                    m.insert(name.clone(), dummy_arguments(psch));
                }
            }
            pad_min_properties(&mut m, schema);
            Value::Object(m)
        }
        Some("array") => {
            // At least `minItems` (default 1 so a downstream `.0` read resolves).
            let min = schema
                .get("minItems")
                .and_then(Value::as_u64)
                .unwrap_or(1)
                .max(1) as usize;
            let item = schema
                .get("items")
                .map(dummy_arguments)
                .unwrap_or(Value::Null);
            Value::Array(std::iter::repeat_n(item, min).collect())
        }
        _ => dummy_for_schema(schema),
    }
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

    /// Every slot capability in the pack spells its contract as a `$ref` into
    /// the shipped HOP vocabulary. A synthesizer that can't follow the ref emits
    /// `null` for all of them — so a mock-driven verify looks like a contract
    /// violation, and the fuzz reports a failure the definition doesn't have.
    #[test]
    fn a_hop_ref_resolves_to_a_contract_valid_dummy() {
        let verdict = dummy_for_schema(&json!({ "$ref": "praxec://hop#/$defs/verifyOut" }));

        // The value must actually satisfy verifyOut — checked through the same
        // registry-aware validator the runtime enforces the contract with, so
        // this test can't drift from the check it exists to keep honest.
        assert_eq!(
            praxec_core::hop::validate_against_schema(
                &json!({ "$ref": "praxec://hop#/$defs/verifyOut" }),
                &verdict,
                "verifyOut",
            ),
            Ok(()),
            "synthesized dummy must satisfy verifyOut, got: {verdict}"
        );

        // And the nested refs resolved too, not just the top level.
        assert!(
            verdict["provenance"]["stack"].is_string(),
            "nested $ref (provenance -> stackProvenance) must resolve: {verdict}"
        );
    }

    #[test]
    fn an_unresolvable_ref_still_degrades_to_null() {
        // Not a HOP ref — no registry to consult, so the old behavior stands
        // rather than panicking or inventing a value.
        assert_eq!(
            dummy_for_schema(&json!({ "$ref": "https://example.com/other#/x" })),
            Value::Null
        );
    }

    #[test]
    fn object_pads_to_min_properties() {
        // `{type: object, minProperties: 1}` with no declared properties: a bare
        // `{}` fails the constraint, so it must be padded. (The `blast_radius`
        // shape in cap.plan.build-graph's inputSchema.)
        let v = dummy_for_schema(&json!({ "type": "object", "minProperties": 1 }));
        assert!(
            v.as_object().is_some_and(|o| !o.is_empty()),
            "must have at least one property: {v}"
        );
    }

    #[test]
    fn dummy_arguments_resolves_a_ref_and_includes_optional_fields() {
        // A hop_slot inputSchema is a bare `{$ref: verifyIn}` — no `properties`.
        let args = dummy_arguments(&json!({ "$ref": "praxec://hop#/$defs/verifyIn" }));
        assert_eq!(
            praxec_core::hop::validate_against_schema(
                &json!({ "$ref": "praxec://hop#/$defs/verifyIn" }),
                &args,
                "verifyIn",
            ),
            Ok(()),
            "arguments must satisfy verifyIn, got: {args}"
        );

        // Optional properties are included (unlike dummy_for_schema) so an output
        // mapped from an optional argument does not land null.
        let schema = json!({
            "type": "object",
            "required": ["a"],
            "properties": { "a": { "type": "string" }, "b": { "type": "string" } }
        });
        let args = dummy_arguments(&schema);
        assert!(args.get("a").is_some());
        assert!(
            args.get("b").is_some(),
            "optional property must be present for the arguments probe: {args}"
        );
    }

    #[test]
    fn dummy_arguments_builds_a_deep_required_object() {
        // cap.plan.build-graph shape: nested required object + array minItems.
        let schema = json!({
            "type": "object",
            "required": ["graph"],
            "properties": { "graph": {
                "type": "object",
                "required": ["deliverables"],
                "properties": { "deliverables": {
                    "type": "array", "minItems": 1,
                    "items": { "type": "object", "required": ["id"],
                               "properties": { "id": { "type": "string" } } }
                }}
            }}
        });
        let args = dummy_arguments(&schema);
        assert!(
            args["graph"]["deliverables"][0]["id"].is_string(),
            "deep required path must be filled: {args}"
        );
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
