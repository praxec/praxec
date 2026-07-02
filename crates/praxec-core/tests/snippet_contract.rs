//! SPEC §5.1 / V3 / V4 / V5 — typed snippet contract for capability
//! workflows. Each test exercises one validation rule with an accepts +
//! rejects pair, named so the PR3 validation-parity scanner can find them.
//!
//! The validator under test is `praxec_core::validate::validate_workflows`
//! reached through `config::resolve_str` (the same path the binary uses).

use praxec_core::config::resolve_str;
use praxec_core::validate::validate_workflows;

/// Helper — produce the full diagnostic list for a YAML config fragment.
fn diagnostics_for(yaml: &str) -> Vec<String> {
    let config = resolve_str(yaml).expect("yaml resolves");
    validate_workflows(&config)
        .into_iter()
        .map(|d| d.message().to_string())
        .collect()
}

fn has_error_containing(diags: &[String], needle: &str) -> bool {
    diags.iter().any(|m| m.contains(needle))
}

// ---------- V3 — snippet block present ----------

#[test]
fn v3_accepts_capability_with_snippet_block() {
    let yaml = r#"
version: "1.0.0"
workflows:
  cap.plan.vet:
    initialState: ready
    snippet:
      inputs:
        plan: { type: object }
      outputs:
        verdict: { type: string }
    states:
      ready: { terminal: true }
"#;
    let diags = diagnostics_for(yaml);
    assert!(
        !has_error_containing(&diags, "MISSING_SNIPPET"),
        "should not error: {diags:?}"
    );
}

#[test]
fn v3_rejects_capability_without_snippet_block() {
    let yaml = r#"
version: "1.0.0"
workflows:
  cap.plan.vet:
    initialState: ready
    states:
      ready: { terminal: true }
"#;
    let diags = diagnostics_for(yaml);
    assert!(
        has_error_containing(&diags, "MISSING_SNIPPET"),
        "expected MISSING_SNIPPET: {diags:?}"
    );
}

// ---------- V4 — snippet has BOTH inputs: AND outputs: keys ----------

#[test]
fn v4_accepts_empty_inputs_and_outputs_objects() {
    // Empty `{}` for either side is valid per spec — the contract is
    // simply "this capability accepts/produces nothing typed".
    let yaml = r#"
version: "1.0.0"
workflows:
  cap.plan.vet:
    initialState: ready
    snippet:
      inputs:  {}
      outputs: {}
    states:
      ready: { terminal: true }
"#;
    let diags = diagnostics_for(yaml);
    assert!(
        !diags.iter().any(|m| m.contains("INVALID_SNIPPET")),
        "should accept empty inputs/outputs: {diags:?}"
    );
}

#[test]
fn v4_rejects_capability_with_snippet_missing_outputs() {
    let yaml = r#"
version: "1.0.0"
workflows:
  cap.plan.vet:
    initialState: ready
    snippet:
      inputs:
        plan: { type: object }
    states:
      ready: { terminal: true }
"#;
    let diags = diagnostics_for(yaml);
    assert!(
        has_error_containing(&diags, "INVALID_SNIPPET") && has_error_containing(&diags, "outputs"),
        "expected INVALID_SNIPPET about outputs: {diags:?}"
    );
}

// ---------- V5 — each input/output is JSON-schema-shaped ----------

#[test]
fn v5_accepts_each_entry_as_json_schema_object() {
    let yaml = r#"
version: "1.0.0"
workflows:
  cap.plan.vet:
    initialState: ready
    snippet:
      inputs:
        plan: { type: object, properties: { title: { type: string } } }
      outputs:
        verdict: { type: string, enum: [pass, fail, needs-revision] }
    states:
      ready: { terminal: true }
"#;
    let diags = diagnostics_for(yaml);
    assert!(
        !diags.iter().any(|m| m.contains("INVALID_SNIPPET")),
        "should accept well-formed schemas: {diags:?}"
    );
}

#[test]
fn v5_rejects_non_object_schema_entry() {
    // An entry value that isn't an object can't be a JSON schema.
    let yaml = r#"
version: "1.0.0"
workflows:
  cap.plan.vet:
    initialState: ready
    snippet:
      inputs:  {}
      outputs:
        verdict: "string"  # scalar, not a schema object
    states:
      ready: { terminal: true }
"#;
    let diags = diagnostics_for(yaml);
    assert!(
        has_error_containing(&diags, "INVALID_SNIPPET") && has_error_containing(&diags, "verdict"),
        "expected INVALID_SNIPPET naming verdict: {diags:?}"
    );
}

// ---------- Flows don't trigger V3 ----------

#[test]
fn v3_does_not_fire_on_flows() {
    // `flow.*` workflows are tier-flow; V3 is cap-only. V8 ("flow
    // MUST NOT declare snippet") is PR3 territory, not exercised here.
    let yaml = r#"
version: "1.0.0"
workflows:
  flow.add-feature:
    initialState: ready
    states:
      ready: { terminal: true }
"#;
    let diags = diagnostics_for(yaml);
    assert!(
        !has_error_containing(&diags, "MISSING_SNIPPET"),
        "flow should not trigger V3: {diags:?}"
    );
}
