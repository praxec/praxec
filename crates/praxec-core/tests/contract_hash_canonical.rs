//! SPEC §6.2 — golden file for the contract-hash canonicalization
//! algorithm. Operators pin contract hashes in their flows via
//! `expects_contract_hash:`; the algorithm is part of the public
//! contract. Any change here is an externally observable break — those
//! pins now reject the same capability. This file pins specific inputs
//! to specific outputs so refactors that change the encoding surface
//! as test failures and force a deliberate decision (introduce
//! `sha256-v2:` rather than silently drift).

use praxec_core::contract_hash::{canonical_json_string, compute_contract_hash};
use serde_json::json;

#[test]
fn hash_of_empty_inputs_outputs_snippet_is_stable() {
    let snippet = json!({ "inputs": {}, "outputs": {} });
    let hash = compute_contract_hash(&snippet);
    // The canonical encoding `{"inputs":{},"outputs":{}}` SHA-256s to:
    // (computed once and pinned; regenerate via `compute_contract_hash`
    // if the algorithm is intentionally rev'd to `sha256-v2:`)
    assert_eq!(
        hash,
        "sha256:3e8b860b6c32dc75b859f3d59c56dfcc0410bacdc623eb3d0d90f36d8720efb0"
    );
}

#[test]
fn hash_of_realistic_plan_vet_snippet_is_stable() {
    let snippet = json!({
        "inputs": {
            "plan":           { "type": "object",  "required": true },
            "max_iterations": { "type": "integer", "default":  3 }
        },
        "outputs": {
            "verdict":  { "type": "string", "enum": ["pass", "fail", "needs-revision"] },
            "findings": { "type": "array",  "items": { "type": "object" } }
        }
    });
    let hash = compute_contract_hash(&snippet);
    assert_eq!(
        hash, "sha256:9ec470db3e22c27d653a2f7770444d2ac5919ec83f8da0210fdce40213811d71",
        "regenerate with `cargo test --test contract_hash_canonical` and update if algorithm change is intentional"
    );
}

#[test]
fn canonical_encoding_is_strictly_sorted_keys() {
    // Spec asserts sorted-key canonicalization; document the exact byte
    // stream so reviewers can reason about it without running the test.
    let v = json!({
        "outputs": { "z": 1, "a": 2 },
        "inputs":  { "y": 3, "b": 4 }
    });
    assert_eq!(
        canonical_json_string(&v),
        r#"{"inputs":{"b":4,"y":3},"outputs":{"a":2,"z":1}}"#
    );
}
