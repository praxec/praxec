//! SPEC §24 (v0.4) — `[*]` bracket-wildcard array projection in path
//! expressions. Lets workflow `output:` mappings pluck per-element fields
//! from arrays returned by fan-out executors.
//!
//! Atomic FMECA-style assertions: one path-shape per test.

use praxec_core::mapping::read_in_scopes;
use serde_json::{Value, json};

fn no_args() -> Value {
    json!({})
}
fn no_input() -> Value {
    json!({})
}

// ── Basic [*] over a literal array ────────────────────────────────────────

#[test]
fn wildcard_projects_each_element_field() {
    let context = json!({
        "branches": [
            { "ok": true,  "index": 0 },
            { "ok": false, "index": 1 },
            { "ok": true,  "index": 2 },
        ]
    });
    let v = read_in_scopes(
        "$.context.branches[*].ok",
        &no_args(),
        &context,
        &no_input(),
        None,
    )
    .expect("path resolves");
    assert_eq!(v, json!([true, false, true]));
}

// ── [*] alias for the whole array ─────────────────────────────────────────

#[test]
fn wildcard_with_no_suffix_returns_array_clone() {
    let context = json!({ "items": [1, 2, 3] });
    let v = read_in_scopes(
        "$.context.items[*]",
        &no_args(),
        &context,
        &no_input(),
        None,
    )
    .expect("resolves");
    assert_eq!(v, json!([1, 2, 3]));
}

// ── Empty array → empty projection ────────────────────────────────────────

#[test]
fn wildcard_on_empty_array_returns_empty_array() {
    let context = json!({ "branches": [] });
    let v = read_in_scopes(
        "$.context.branches[*].ok",
        &no_args(),
        &context,
        &no_input(),
        None,
    )
    .expect("resolves");
    assert_eq!(v, json!([]));
}

// ── [*] on non-array → None (consistent with unresolved-path contract) ────

#[test]
fn wildcard_on_non_array_returns_none() {
    let context = json!({ "branches": { "not": "an array" } });
    let v = read_in_scopes(
        "$.context.branches[*].ok",
        &no_args(),
        &context,
        &no_input(),
        None,
    );
    assert_eq!(v, None);
}

// ── Nested projection: branches[*].nested.field ──────────────────────────

#[test]
fn wildcard_with_nested_suffix_projects_nested_field() {
    let context = json!({
        "results": [
            { "summary": { "score": 90 } },
            { "summary": { "score": 85 } },
            { "summary": { "score": 95 } },
        ]
    });
    let v = read_in_scopes(
        "$.context.results[*].summary.score",
        &no_args(),
        &context,
        &no_input(),
        None,
    )
    .expect("resolves");
    assert_eq!(v, json!([90, 85, 95]));
}

// ── Doubly-nested [*] (each result's items[*]) ────────────────────────────

#[test]
fn nested_wildcards_recurse_through_inner_array() {
    let context = json!({
        "groups": [
            { "items": [{ "name": "a" }, { "name": "b" }] },
            { "items": [{ "name": "c" }] },
        ]
    });
    let v = read_in_scopes(
        "$.context.groups[*].items[*].name",
        &no_args(),
        &context,
        &no_input(),
        None,
    )
    .expect("resolves");
    // Top-level [*] projects each group; for each group, the inner
    // [*].name projects to an inner array.
    assert_eq!(v, json!([["a", "b"], ["c"]]));
}

// ── Plain path (no [*]) still works (backward compatibility) ─────────────

#[test]
fn non_wildcard_path_unchanged_after_extension() {
    let context = json!({ "summary": { "ok_count": 5 } });
    let v = read_in_scopes(
        "$.context.summary.ok_count",
        &no_args(),
        &context,
        &no_input(),
        None,
    )
    .expect("resolves");
    assert_eq!(v, json!(5));
}

// ── Wildcard against $.output (the parallel-fan-out use case) ─────────────

#[test]
fn wildcard_works_against_output_scope() {
    let output = json!({
        "branches": [
            { "ok": true,  "output": { "verdict": "accept" } },
            { "ok": true,  "output": { "verdict": "retry"  } },
            { "ok": false, "output": null, "error": { "code": "timeout" } },
        ]
    });
    let v = read_in_scopes(
        "$.output.branches[*].ok",
        &no_args(),
        &json!({}),
        &no_input(),
        Some(&output),
    )
    .expect("resolves");
    assert_eq!(v, json!([true, true, false]));
}

// ── Wildcard against missing prefix returns None ─────────────────────────

#[test]
fn wildcard_on_missing_prefix_returns_none() {
    let context = json!({ "other": "field" });
    let v = read_in_scopes(
        "$.context.branches[*].ok",
        &no_args(),
        &context,
        &no_input(),
        None,
    );
    assert_eq!(v, None);
}

// ── Missing field per element → null in result array ─────────────────────

#[test]
fn missing_field_per_element_becomes_null_in_result() {
    let context = json!({
        "branches": [
            { "ok": true },                       // missing 'error'
            { "ok": false, "error": "boom" },
        ]
    });
    let v = read_in_scopes(
        "$.context.branches[*].error",
        &no_args(),
        &context,
        &no_input(),
        None,
    )
    .expect("resolves");
    assert_eq!(v, json!([null, "boom"]));
}
