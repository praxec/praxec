//! D4b drift detection — assert that `schemas/registry.schema.json` and the
//! hand-authored types in `praxec_core::registry_v3` stay in lockstep
//! (mirrors `tool_descriptor_schema_snapshot.rs`).
//!
//! The types are hand-authored because the schema cross-`$ref`s
//! `tool-descriptor.schema.json` (which itself refs the gateway config),
//! which typify cannot resolve — the schema is registered for runtime
//! jsonschema validation instead. Without this test, schema and Rust would
//! drift silently whenever someone updates one and forgets the other.
//!
//! Guards, in both directions:
//! - every closed enum in the schema equals the Rust `ALL_TOKENS` set;
//! - the embedded schema const equals the shipped file byte-for-byte;
//! - a maximally-populated registry serializes to exactly the schema's
//!   property sets (schema-side additions surface here; Rust-side additions
//!   surface here too) and passes the canonical loader;
//! - the tool `descriptor` `$ref` still points at the D1 schema — the
//!   registry's descriptors ARE D1 [`ToolDescriptor`]s, never a fork.

use std::collections::HashSet;
use std::path::PathBuf;

use serde_json::{Value, json};

use praxec_core::registry_v3::{
    CrossmatrixRole, PackTier, REGISTRY_SCHEMA, Registry, RegistrySchema,
};
use praxec_core::tool_descriptor::{
    ProvisionProvider, TOOL_DESCRIPTOR_SCHEMA_VERSION, ToolDescriptor,
};

fn workspace_root() -> PathBuf {
    let mut p = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    p.pop();
    p.pop();
    p
}

fn read_schema(name: &str) -> Value {
    let path = workspace_root().join("schemas").join(name);
    let text = std::fs::read_to_string(&path)
        .unwrap_or_else(|e| panic!("schema must exist at {}: {e}", path.display()));
    serde_json::from_str(&text)
        .unwrap_or_else(|e| panic!("{} must parse as JSON: {e}", path.display()))
}

fn registry_schema() -> Value {
    read_schema("registry.schema.json")
}

/// Resolve a JSON pointer to an enum array and return its string tokens.
fn enum_tokens(schema: &Value, pointer: &str) -> Vec<String> {
    schema
        .pointer(pointer)
        .unwrap_or_else(|| panic!("schema must define an enum at {pointer}"))
        .as_array()
        .unwrap_or_else(|| panic!("{pointer} must be an array"))
        .iter()
        .map(|v| {
            v.as_str()
                .unwrap_or_else(|| panic!("{pointer} entries must be strings"))
                .to_string()
        })
        .collect()
}

fn assert_set_eq(label: &str, expected: &[&str], actual: &[String]) {
    let expected_set: HashSet<&str> = expected.iter().copied().collect();
    let actual_set: HashSet<&str> = actual.iter().map(String::as_str).collect();
    let missing: Vec<&str> = expected_set.difference(&actual_set).copied().collect();
    let extra: Vec<&str> = actual_set.difference(&expected_set).copied().collect();
    assert!(
        missing.is_empty() && extra.is_empty(),
        "{label}: drift between Rust enum and schema.\n  \
         missing from schema (present in Rust): {missing:?}\n  \
         extra in schema (absent from Rust):   {extra:?}\n\
         Reconcile by updating the source that's wrong."
    );
}

// ── embedded const ↔ shipped file ─────────────────────────────────────────

#[test]
fn embedded_schema_const_matches_shipped_file() {
    let path = workspace_root().join("schemas/registry.schema.json");
    let on_disk = std::fs::read_to_string(&path)
        .unwrap_or_else(|e| panic!("schema must exist at {}: {e}", path.display()));
    assert_eq!(
        REGISTRY_SCHEMA, on_disk,
        "REGISTRY_SCHEMA (include_str!) diverged from the shipped file — rebuild"
    );
}

// ── closed enums ──────────────────────────────────────────────────────────

#[test]
fn schema_marker_const_matches_v3_token() {
    let schema = registry_schema();
    let const_value = schema
        .pointer("/properties/schema/const")
        .and_then(Value::as_str)
        .expect("schema marker must be a const string");
    assert_eq!(const_value, RegistrySchema::V3.as_token());
    // And the closed Rust set still spans exactly {v2, v3} — the loader's
    // v2 acceptance rides on v3 being a compatible superset.
    assert_eq!(
        RegistrySchema::ALL_TOKENS,
        &["praxec.packs/v2", "praxec.packs/v3"],
    );
}

#[test]
fn schema_pack_tier_enum_matches_pack_tier_all_tokens() {
    let schema = registry_schema();
    let tokens = enum_tokens(&schema, "/$defs/pack/properties/tier/enum");
    assert_set_eq("pack.tier enum", PackTier::ALL_TOKENS, &tokens);
}

#[test]
fn schema_crossmatrix_role_enum_matches_crossmatrix_role_all_tokens() {
    let schema = registry_schema();
    let tokens = enum_tokens(&schema, "/$defs/crossmatrixRow/properties/role/enum");
    assert_set_eq(
        "crossmatrix.role enum",
        CrossmatrixRole::ALL_TOKENS,
        &tokens,
    );
}

#[test]
fn schema_provider_keys_enum_matches_provision_provider_all_tokens() {
    // The v2 provider map's keys and the D1 descriptor's provider chain are
    // the SAME closed set — no parallel provider vocabulary.
    let schema = registry_schema();
    let tokens = enum_tokens(
        &schema,
        "/$defs/registryTool/properties/providers/propertyNames/enum",
    );
    assert_set_eq(
        "registryTool.providers keys enum",
        ProvisionProvider::ALL_TOKENS,
        &tokens,
    );
}

// ── the load-bearing $ref: tool descriptor ≡ D1 ───────────────────────────

#[test]
fn tool_descriptor_field_refs_the_d1_schema() {
    let schema = registry_schema();
    let reference = schema
        .pointer("/$defs/registryTool/properties/descriptor/$ref")
        .and_then(Value::as_str)
        .expect("registryTool.descriptor must be a $ref");
    assert_eq!(
        reference, "tool-descriptor.schema.json",
        "the registry's tool descriptors ARE D1 ToolDescriptors — the field \
         must $ref the EXISTING descriptor schema, never fork it"
    );
}

// ── property-set lockstep: schema ↔ hand-authored serde types ─────────────

/// A registry with EVERY field populated. Serializing it and comparing key
/// sets against the schema's `properties` catches drift in either direction:
/// - a field added to the schema but not the Rust type → missing key here
///   (and `deny_unknown_fields` fails loads at runtime);
/// - a field added to the Rust type but not the schema → extra key here
///   (and the canonical loader rejects it as `additionalProperties`).
fn maximal_registry() -> Value {
    json!({
        "schema": "praxec.packs/v3",
        "packs": [
            {
                "id": "maximal-pack",
                "name": "Maximal Pack",
                "namespace": "maximal",
                "description": "every field populated",
                "repo": "https://example.invalid/maximal-pack",
                "tier": "open",
                "tags": ["a"],
                "requires": ["maximal-tool"],
                "external": ["outside-tool"],
                "extends": "base-pack"
            }
        ],
        "tools": [
            {
                "id": "maximal-tool",
                "name": "Maximal Tool",
                "description": "every field populated",
                "repo": "https://example.invalid/maximal-tool",
                "command": "maximal-tool",
                "version": "9.9.9",
                "mcp_registry_id": "dev.praxec/maximal-tool",
                "providers": {
                    "docker": "ghcr.io/example/maximal",
                    "release": "https://example.invalid/releases",
                    "cargo": "maximal-tool",
                    "npx": "@example/maximal",
                    "uvx": "maximal-tool"
                },
                "descriptor": {
                    "schema_version": TOOL_DESCRIPTOR_SCHEMA_VERSION,
                    "name": "maximal-tool",
                    "version": "9.9.9",
                    "kind": "mcp",
                    "reach": {
                        "connection_name": "maximal",
                        "grant_as": "maximal",
                        "connection": {
                            "kind": "mcp", "command": "maximal-tool", "args": [], "env": {}
                        }
                    },
                    "operations": [
                        {
                            "id": "do",
                            "verb": "run",
                            "input_schema": { "type": "object" },
                            "output_schema": { "type": "object" },
                            "mcp_tool": "do"
                        }
                    ]
                },
                "suggested_workflows": ["maximal/flow.max"]
            }
        ],
        "crossmatrix": [
            { "tool": "maximal-tool", "workflow": "maximal/flow.max", "role": "suggested" }
        ]
    })
}

fn schema_property_keys(schema: &Value, pointer: &str) -> HashSet<String> {
    schema
        .pointer(pointer)
        .and_then(Value::as_object)
        .unwrap_or_else(|| panic!("schema must have properties at {pointer}"))
        .keys()
        .cloned()
        .collect()
}

#[test]
fn maximal_registry_loads_and_round_trips_all_schema_properties() {
    // The maximal fixture must pass the canonical loader (schema + serde +
    // cross-field validate) — proving every schema property deserializes.
    let registry = Registry::load_value(maximal_registry()).expect("maximal loads");

    // Re-serialize and compare key sets against the schema, level by level.
    // The descriptor's own levels are covered by
    // `tool_descriptor_schema_snapshot.rs` — here we stop at the $ref seam.
    let serialized = serde_json::to_value(&registry).expect("registry serializes");
    let schema = registry_schema();

    let cases: &[(&str, &str, &str)] = &[
        ("top-level", "", "/properties"),
        ("pack", "/packs/0", "/$defs/pack/properties"),
        ("tool", "/tools/0", "/$defs/registryTool/properties"),
        (
            "crossmatrix row",
            "/crossmatrix/0",
            "/$defs/crossmatrixRow/properties",
        ),
    ];
    for (label, value_ptr, schema_ptr) in cases {
        let value_keys: HashSet<String> = serialized
            .pointer(value_ptr)
            .and_then(Value::as_object)
            .unwrap_or_else(|| panic!("serialized registry must have an object at {value_ptr}"))
            .keys()
            .cloned()
            .collect();
        let schema_keys = schema_property_keys(&schema, schema_ptr);
        let missing: Vec<&String> = schema_keys.difference(&value_keys).collect();
        let extra: Vec<&String> = value_keys.difference(&schema_keys).collect();
        assert!(
            missing.is_empty() && extra.is_empty(),
            "{label}: schema/type property drift.\n  \
             in schema but not serialized by Rust: {missing:?}\n  \
             serialized by Rust but not in schema: {extra:?}\n\
             Update the lagging side (and this fixture if a field was added)."
        );
    }

    // The $ref seam holds: the embedded descriptor deserialized as a real
    // D1 ToolDescriptor and round-trips through ITS canonical loader.
    let descriptor_value = serialized
        .pointer("/tools/0/descriptor")
        .expect("maximal tool carries a descriptor")
        .clone();
    ToolDescriptor::load_value(descriptor_value)
        .expect("the registry's descriptor passes the D1 canonical loader");
}

#[test]
fn schema_required_fields_match_non_optional_rust_fields() {
    let schema = registry_schema();

    let required_at = |pointer: &str| -> Vec<String> {
        schema
            .pointer(pointer)
            .and_then(Value::as_array)
            .unwrap_or_else(|| panic!("schema declares required at {pointer}"))
            .iter()
            .map(|v| {
                v.as_str()
                    .expect("required entries are strings")
                    .to_string()
            })
            .collect()
    };

    assert_set_eq("top-level required", &["schema"], &required_at("/required"));
    assert_set_eq(
        "pack required",
        &["id", "name", "namespace"],
        &required_at("/$defs/pack/required"),
    );
    assert_set_eq(
        "tool required",
        &["id", "name"],
        &required_at("/$defs/registryTool/required"),
    );
    assert_set_eq(
        "crossmatrix row required",
        &["tool", "workflow", "role"],
        &required_at("/$defs/crossmatrixRow/required"),
    );

    // Dropping any pack/tool/row required field must fail the loader —
    // proving the Rust side treats them as non-optional too (no silent
    // defaults). Removing `schema` is covered separately: it fails the
    // marker gate (REGISTRY_UNKNOWN_SCHEMA), before schema validation.
    let sites: &[(&str, &str, Vec<String>)] = &[
        ("pack", "/packs/0", required_at("/$defs/pack/required")),
        (
            "tool",
            "/tools/0",
            required_at("/$defs/registryTool/required"),
        ),
        (
            "crossmatrix row",
            "/crossmatrix/0",
            required_at("/$defs/crossmatrixRow/required"),
        ),
    ];
    for (label, pointer, required) in sites {
        for field in required {
            let mut doc = maximal_registry();
            doc.pointer_mut(pointer)
                .and_then(Value::as_object_mut)
                .unwrap_or_else(|| panic!("fixture has an object at {pointer}"))
                .remove(field.as_str());
            assert!(
                Registry::load_value(doc).is_err(),
                "{label} missing required `{field}` must fail to load"
            );
        }
    }
}
