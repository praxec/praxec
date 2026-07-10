//! D1 drift detection — assert that `schemas/tool-descriptor.schema.json`
//! and the hand-authored types in `praxec_core::tool_descriptor` stay in
//! lockstep (mirrors `spec_enum_drift.rs`).
//!
//! The types are hand-authored because the schema cross-`$ref`s
//! `gateway-config.schema.json#/$defs/connection`, which typify cannot
//! resolve (same reason `hop.schema.json` is runtime-validated). Without
//! this test, schema and Rust would drift silently whenever someone updates
//! one and forgets the other.
//!
//! Guards, in both directions:
//! - every closed enum in the schema equals the Rust `ALL_TOKENS` set;
//! - the embedded schema const equals the shipped file byte-for-byte;
//! - a maximally-populated descriptor serializes to exactly the schema's
//!   property sets (schema-side additions surface here; Rust-side additions
//!   surface here too) and passes the canonical loader;
//! - the `reach.connection` `$ref` still points at the gateway config's
//!   `$defs/connection`, and that def still exists with the three kinds.

use std::collections::HashSet;
use std::path::PathBuf;

use serde_json::{Value, json};

use praxec_core::discovery::ScriptVerb;
use praxec_core::tool_descriptor::{
    AuthScheme, ProvisionProvider, TOOL_DESCRIPTOR_SCHEMA, TOOL_DESCRIPTOR_SCHEMA_VERSION,
    ToolDescriptor, ToolKind,
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

fn descriptor_schema() -> Value {
    read_schema("tool-descriptor.schema.json")
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
    let path = workspace_root().join("schemas/tool-descriptor.schema.json");
    let on_disk = std::fs::read_to_string(&path)
        .unwrap_or_else(|e| panic!("schema must exist at {}: {e}", path.display()));
    assert_eq!(
        TOOL_DESCRIPTOR_SCHEMA, on_disk,
        "TOOL_DESCRIPTOR_SCHEMA (include_str!) diverged from the shipped file — rebuild"
    );
}

// ── closed enums ──────────────────────────────────────────────────────────

#[test]
fn schema_kind_enum_matches_tool_kind_all_tokens() {
    let schema = descriptor_schema();
    let tokens = enum_tokens(&schema, "/properties/kind/enum");
    assert_set_eq("kind enum", ToolKind::ALL_TOKENS, &tokens);
}

#[test]
fn schema_operation_verb_enum_matches_script_verb_all_tokens() {
    // The design reuses the closed ScriptVerb vocabulary (SPEC §22.3) — no
    // parallel taxonomy. If ScriptVerb gains a token, the schema must too.
    let schema = descriptor_schema();
    let tokens = enum_tokens(&schema, "/$defs/operation/properties/verb/enum");
    assert_set_eq("operation.verb enum", ScriptVerb::ALL_TOKENS, &tokens);
}

#[test]
fn schema_auth_scheme_enum_matches_auth_scheme_all_tokens() {
    let schema = descriptor_schema();
    let tokens = enum_tokens(&schema, "/$defs/authRequirement/properties/scheme/enum");
    assert_set_eq(
        "authRequirement.scheme enum",
        AuthScheme::ALL_TOKENS,
        &tokens,
    );
}

#[test]
fn schema_provider_enum_matches_provision_provider_all_tokens() {
    let schema = descriptor_schema();
    let tokens = enum_tokens(&schema, "/$defs/provision/properties/providers/items/enum");
    assert_set_eq(
        "provision.providers enum",
        ProvisionProvider::ALL_TOKENS,
        &tokens,
    );
}

#[test]
fn schema_version_const_matches_rust_const() {
    let schema = descriptor_schema();
    let const_value = schema
        .pointer("/properties/schema_version/const")
        .and_then(Value::as_str)
        .expect("schema_version must be a const string");
    assert_eq!(const_value, TOOL_DESCRIPTOR_SCHEMA_VERSION);
}

// ── the load-bearing $ref: reach.connection ≡ gateway connection ─────────

#[test]
fn reach_connection_refs_gateway_config_connection_def() {
    let schema = descriptor_schema();
    let reference = schema
        .pointer("/$defs/reach/properties/connection/$ref")
        .and_then(Value::as_str)
        .expect("reach.connection must be a $ref");
    assert_eq!(
        reference, "gateway-config.schema.json#/$defs/connection",
        "reach.connection must $ref the EXISTING gateway connection shape \
         (copy, never transform) — do not fork the connection format"
    );

    // And the target still exists with the three kind branches the
    // descriptor's closed ToolKind mirrors.
    let gateway = read_schema("gateway-config.schema.json");
    let one_of = gateway
        .pointer("/$defs/connection/oneOf")
        .and_then(Value::as_array)
        .expect("gateway config must define $defs/connection as a oneOf");
    let branch_kinds: Vec<String> = one_of
        .iter()
        .map(|branch| {
            let reference = branch
                .get("$ref")
                .and_then(Value::as_str)
                .expect("connection oneOf branches are $refs");
            let pointer = reference.strip_prefix('#').expect("internal ref");
            gateway
                .pointer(&format!("{pointer}/properties/kind/const"))
                .and_then(Value::as_str)
                .expect("each connection branch declares a kind const")
                .to_string()
        })
        .collect();
    assert_set_eq(
        "gateway connection kinds vs ToolKind",
        ToolKind::ALL_TOKENS,
        &branch_kinds,
    );
}

// ── property-set lockstep: schema ↔ hand-authored serde types ─────────────

/// A descriptor with EVERY field populated (including the reserved
/// forward-compat slots). Serializing it and comparing key sets against the
/// schema's `properties` catches drift in either direction:
/// - a field added to the schema but not the Rust type → missing key here
///   (and `deny_unknown_fields` fails loads at runtime);
/// - a field added to the Rust type but not the schema → extra key here
///   (and the canonical loader rejects it as `additionalProperties`).
fn maximal_descriptor() -> Value {
    json!({
        "schema_version": TOOL_DESCRIPTOR_SCHEMA_VERSION,
        "name": "maximal",
        "version": "9.9.9",
        "source_repo": "https://example.invalid/maximal",
        "description": "every field populated",
        "tags": ["a"],
        "aliases": ["max"],
        "kind": "mcp",
        "reach": {
            "connection_name": "maximal",
            "grant_as": "maximal",
            "connection": { "kind": "mcp", "command": "maximal-server", "args": [], "env": {} },
            "auth": { "scheme": "env", "env": ["MAXIMAL_TOKEN"], "headers": ["X-Maximal"] }
        },
        "provision": {
            "mcp_registry_id": "dev.praxec/maximal",
            "version": "9.9.9",
            "providers": ["docker", "release", "cargo", "npx", "uvx"]
        },
        "operations": [
            {
                "id": "do",
                "verb": "run",
                "input_schema": { "type": "object" },
                "output_schema": { "type": "object" },
                "mcp_tool": "do"
            }
        ],
        "suggested_workflows": ["maximal/flow.max"],
        "embedding": [0.1, 0.2],
        "structural_fingerprint": "sha256:maximal"
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
fn maximal_descriptor_loads_and_round_trips_all_schema_properties() {
    // The maximal fixture must pass the canonical loader (schema + serde +
    // cross-field validate) — proving every schema property deserializes.
    let descriptor =
        ToolDescriptor::load_str(&maximal_descriptor().to_string()).expect("maximal loads");

    // Re-serialize and compare key sets against the schema, level by level.
    let serialized = serde_json::to_value(&descriptor).expect("descriptor serializes");
    let schema = descriptor_schema();

    let cases: &[(&str, &str, &str)] = &[
        ("top-level", "", "/properties"),
        ("reach", "/reach", "/$defs/reach/properties"),
        ("auth", "/reach/auth", "/$defs/authRequirement/properties"),
        ("provision", "/provision", "/$defs/provision/properties"),
        ("operation", "/operations/0", "/$defs/operation/properties"),
    ];
    for (label, value_ptr, schema_ptr) in cases {
        let value_keys: HashSet<String> = serialized
            .pointer(value_ptr)
            .and_then(Value::as_object)
            .unwrap_or_else(|| panic!("serialized descriptor must have an object at {value_ptr}"))
            .keys()
            .cloned()
            .collect();
        let mut schema_keys = schema_property_keys(&schema, schema_ptr);
        // The operation's dispatch coordinates are kind-exclusive: a valid
        // descriptor carries exactly ONE of the three. The maximal fixture
        // is kind: mcp, so `rest` / `cli` are legitimately absent.
        if *label == "operation" {
            schema_keys.remove("rest");
            schema_keys.remove("cli");
        }
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
}

#[test]
fn schema_required_fields_match_non_optional_rust_fields() {
    let schema = descriptor_schema();
    let required: Vec<String> = schema
        .pointer("/required")
        .and_then(Value::as_array)
        .expect("schema declares required")
        .iter()
        .map(|v| {
            v.as_str()
                .expect("required entries are strings")
                .to_string()
        })
        .collect();
    assert_set_eq(
        "top-level required",
        &[
            "schema_version",
            "name",
            "version",
            "kind",
            "reach",
            "operations",
        ],
        &required,
    );

    // Dropping any required field must fail the loader — proving the Rust
    // side treats them as non-optional too (no silent defaults).
    for field in &required {
        let mut doc = maximal_descriptor();
        doc.as_object_mut()
            .expect("descriptor is an object")
            .remove(field.as_str());
        assert!(
            ToolDescriptor::load_str(&doc.to_string()).is_err(),
            "descriptor missing required `{field}` must fail to load"
        );
    }
}
