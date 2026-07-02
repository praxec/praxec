//! SPEC §6.2 — content-identity hash for a capability's typed contract.
//!
//! The contract hash is computed at config-load from the capability's
//! `snippet:` block alone (inputs + outputs schemas, sorted-key
//! canonicalization). It is surfaced by `gateway.describe` and pinned via
//! `expects_contract_hash:` on `use:` blocks (V15/V16).
//!
//! ## Algorithm — stable across releases
//!
//! 1. Re-serialize the snippet Value with **sorted object keys** at every
//!    depth. The implementation walks the Value and emits a deterministic
//!    JSON byte stream via [`canonical_json_string`]. Array element order
//!    is preserved (arrays in JSON Schema are usually ordered:
//!    `required: [a, b]`, `enum: [pass, fail]`).
//! 2. Hash the resulting UTF-8 bytes with SHA-256.
//! 3. Format as `sha256:<64 lowercase hex chars>`.
//!
//! **The algorithm is part of the public contract.** Operators write
//! `expects_contract_hash: "sha256:..."` and the gateway compares against
//! the result of this function. Changing canonicalization (e.g. switching
//! to BLAKE3, normalizing whitespace differently, sorting array elements)
//! would silently break every pinning operator on upgrade. Don't.
//!
//! The pin to V1 (and only V1) of the algorithm is enforced via the
//! `tests/contract_hash_canonical.rs` golden-file test in the workspace.
//! Future algorithm versions, if they happen, will use a new prefix
//! (`sha256-v2:`) so old pins remain interpretable.

use serde_json::Value;
use sha2::{Digest, Sha256};

/// SPEC §6.2 — compute the contract hash for a `snippet:` block.
/// Returns `sha256:<hex>`. See module docs for the canonicalization
/// algorithm; the test `tests/contract_hash_canonical.rs` pins specific
/// inputs to specific outputs so refactors that change the encoding
/// surface as test failures.
pub fn compute_contract_hash(snippet: &Value) -> String {
    let canonical = canonical_json_string(snippet);
    let digest = Sha256::digest(canonical.as_bytes());
    format!("sha256:{:x}", digest)
}

/// Emit a deterministic JSON byte stream from `value`. Object keys are
/// sorted lexicographically at every depth; arrays preserve order;
/// strings use the standard JSON escape rules courtesy of
/// `serde_json::to_string`.
///
/// Public so tests + downstream tooling can build their own contract
/// fingerprints against the same canonicalization without copy-pasting
/// the algorithm. (Stability is the whole point.)
pub fn canonical_json_string(value: &Value) -> String {
    let mut out = String::new();
    write_canonical(&mut out, value);
    out
}

fn write_canonical(out: &mut String, value: &Value) {
    match value {
        Value::Null => out.push_str("null"),
        Value::Bool(b) => out.push_str(if *b { "true" } else { "false" }),
        Value::Number(n) => {
            // serde_json's Number Display preserves the original numeric
            // representation (int vs float); good for our purposes since
            // operator-written schemas come back through the YAML loader
            // with predictable shapes.
            out.push_str(&n.to_string());
        }
        Value::String(s) => {
            // Defer to serde_json's escape rules — same as every other
            // JSON consumer in the codebase.
            out.push_str(&serde_json::to_string(s).expect("string serializes"));
        }
        Value::Array(arr) => {
            out.push('[');
            for (i, item) in arr.iter().enumerate() {
                if i > 0 {
                    out.push(',');
                }
                write_canonical(out, item);
            }
            out.push(']');
        }
        Value::Object(map) => {
            // Sorted-key emission. BTreeMap-style ordering by allocating
            // a sorted Vec of keys; cheaper than building a fresh
            // BTreeMap for each level on the recursive walk.
            let mut keys: Vec<&String> = map.keys().collect();
            keys.sort();
            out.push('{');
            for (i, k) in keys.iter().enumerate() {
                if i > 0 {
                    out.push(',');
                }
                out.push_str(&serde_json::to_string(k).expect("key serializes"));
                out.push(':');
                write_canonical(out, &map[*k]);
            }
            out.push('}');
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn hash_format_is_sha256_prefixed_lowercase_hex_64() {
        let h = compute_contract_hash(&json!({}));
        assert!(h.starts_with("sha256:"));
        let hex = &h["sha256:".len()..];
        assert_eq!(hex.len(), 64);
        assert!(hex
            .chars()
            .all(|c| c.is_ascii_hexdigit() && !c.is_ascii_uppercase()));
    }

    #[test]
    fn key_order_does_not_affect_hash() {
        let a = json!({ "inputs": { "a": 1, "b": 2 }, "outputs": { "x": "y" } });
        let b = json!({ "outputs": { "x": "y" }, "inputs": { "b": 2, "a": 1 } });
        assert_eq!(compute_contract_hash(&a), compute_contract_hash(&b));
    }

    #[test]
    fn array_order_does_affect_hash() {
        let a = json!({ "required": ["a", "b"] });
        let b = json!({ "required": ["b", "a"] });
        assert_ne!(
            compute_contract_hash(&a),
            compute_contract_hash(&b),
            "JSON Schema array order is semantic; hashes must differ"
        );
    }

    #[test]
    fn schema_content_changes_change_hash() {
        let base = json!({
            "inputs":  {},
            "outputs": { "v": { "type": "string", "enum": ["pass", "fail"] } }
        });
        let mutated = json!({
            "inputs":  {},
            "outputs": { "v": { "type": "string", "enum": ["pass", "fail", "needs-revision"] } }
        });
        assert_ne!(
            compute_contract_hash(&base),
            compute_contract_hash(&mutated)
        );
    }

    #[test]
    fn canonical_json_string_emits_sorted_keys_at_every_depth() {
        let v = json!({ "b": { "y": 1, "x": 2 }, "a": 3 });
        let s = canonical_json_string(&v);
        // "a" before "b" at top level; "x" before "y" inside "b".
        assert_eq!(s, r#"{"a":3,"b":{"x":2,"y":1}}"#);
    }
}
