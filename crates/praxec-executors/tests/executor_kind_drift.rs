//! SPEC §24 GAP-E mitigation — drift test for executor kinds.
//!
//! Asserts that `praxec_executors::REGISTERED_EXECUTOR_KINDS`
//! matches the JSON schema's `executor.properties.kind.examples` array.
//!
//! When this test fails, the assertion error names the diverged tokens.
//! Fix by updating EITHER:
//!   - `REGISTERED_EXECUTOR_KINDS` in `crates/praxec-executors/src/lib.rs`
//!   - or `schemas/gateway-config.schema.json` `executor.properties.kind.examples`
//!
//! Adding a new executor kind = update BOTH in the same commit.

use std::collections::HashSet;
use std::path::PathBuf;

use praxec_executors::REGISTERED_EXECUTOR_KINDS;

fn workspace_root() -> PathBuf {
    let mut p = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    p.pop();
    p.pop();
    p
}

#[test]
fn schema_executor_kind_examples_match_registered_executor_kinds() {
    let schema_path = workspace_root().join("schemas/gateway-config.schema.json");
    let schema_src = std::fs::read_to_string(&schema_path)
        .unwrap_or_else(|e| panic!("schema must exist at {}: {e}", schema_path.display()));

    // Find the `executor` $def → `properties` → `kind` → `examples` array.
    // The schema is a single root JSON value; parse it and traverse via
    // pointer rather than ad-hoc string slicing.
    let schema: serde_json::Value =
        serde_json::from_str(&schema_src).expect("schema must parse as JSON");
    let examples = schema
        .pointer("/$defs/executor/properties/kind/examples")
        .and_then(|v| v.as_array())
        .expect("schema must have $defs/executor/properties/kind/examples array");

    let schema_kinds: HashSet<&str> = examples.iter().filter_map(|v| v.as_str()).collect();
    let registered: HashSet<&str> = REGISTERED_EXECUTOR_KINDS.iter().copied().collect();

    let missing_from_schema: Vec<&str> = registered.difference(&schema_kinds).copied().collect();
    let extra_in_schema: Vec<&str> = schema_kinds.difference(&registered).copied().collect();
    assert!(
        missing_from_schema.is_empty() && extra_in_schema.is_empty(),
        "executor kind drift between Rust registry and JSON schema.\n  \
         missing from schema (registered in Rust): {missing_from_schema:?}\n  \
         extra in schema (not registered in Rust): {extra_in_schema:?}\n\
         Fix: update both `REGISTERED_EXECUTOR_KINDS` and the schema's \
         `executor.properties.kind.examples` array in the same commit."
    );
}
