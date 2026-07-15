//! PR3 validation-rule accepts/rejects pairs for V1, V2, V6, V7, V8,
//! V9, V10, V11, V15, V16. Naming convention matches the PR3 parity
//! scanner: `fn v<N>_(accepts|rejects)_<topic>`.
//!
//! V3/V4/V5 live in tests/snippet_contract.rs (PR2).
//! V12 lives in tests/use_binding.rs (PR2).
//! V13/V14 are exercised in src/slot_table.rs unit tests.
//! V17/V18 live in tests/cap_output_violation.rs / cap_terminated.rs.
//! V19-V23 live in tests/multi_repo_loading.rs (PR1).

use praxec_core::config::resolve_str;
use praxec_core::validate::validate_workflows;

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

// ---------- V1 — verb in 24-token cloud ----------

#[test]
fn v1_accepts_capability_with_blessed_verb() {
    let yaml = r#"
version: "1.0.0"
workflows:
  cap.plan.draft:
    verb: plan
    initialState: ready
    snippet:
      inputs:  {}
      outputs: {}
    states:
      ready:
        transitions:
          t:
            target: done
            executor:
              kind: mcp
              connection: any
      done: { terminal: true }
"#;
    let d = diagnostics_for(yaml);
    assert!(!has_error_containing(&d, "INVALID_VERB"), "{d:?}");
    assert!(!has_error_containing(&d, "MISSING_VERB"), "{d:?}");
}

#[test]
fn v1_rejects_capability_with_unknown_verb() {
    let yaml = r#"
version: "1.0.0"
workflows:
  cap.bogus.thing:
    verb: destroy
    initialState: ready
    snippet:
      inputs:  {}
      outputs: {}
    states:
      ready: { terminal: true }
"#;
    let d = diagnostics_for(yaml);
    assert!(has_error_containing(&d, "INVALID_VERB"), "{d:?}");
}

// ---------- V2 — id stem matches cap.<verb>.<name> ----------

#[test]
fn v2_accepts_when_id_verb_matches_declared_verb() {
    let yaml = r#"
version: "1.0.0"
workflows:
  cap.plan.draft:
    verb: plan
    initialState: ready
    snippet:
      inputs:  {}
      outputs: {}
    states:
      ready:
        transitions:
          t:
            target: done
            executor: { kind: mcp, connection: any }
      done: { terminal: true }
"#;
    let d = diagnostics_for(yaml);
    assert!(!has_error_containing(&d, "ID_VERB_MISMATCH"), "{d:?}");
    assert!(!has_error_containing(&d, "INVALID_ID_SHAPE"), "{d:?}");
}

#[test]
fn v2_rejects_when_id_verb_differs_from_declared_verb() {
    let yaml = r#"
version: "1.0.0"
workflows:
  cap.plan.draft:
    verb: review
    initialState: ready
    snippet:
      inputs:  {}
      outputs: {}
    states:
      ready:
        transitions:
          t:
            target: done
            executor: { kind: mcp, connection: any }
      done: { terminal: true }
"#;
    let d = diagnostics_for(yaml);
    assert!(has_error_containing(&d, "ID_VERB_MISMATCH"), "{d:?}");
}

// ---------- V6 — primary executor verb-shape ----------

#[test]
fn v6_accepts_cognitive_cap_with_mcp_executor() {
    let yaml = r#"
version: "1.0.0"
workflows:
  cap.plan.draft:
    verb: plan
    initialState: drafting
    snippet:
      inputs:  {}
      outputs: {}
    states:
      drafting:
        transitions:
          t:
            target: done
            executor: { kind: mcp, connection: any }
      done: { terminal: true }
"#;
    let d = diagnostics_for(yaml);
    assert!(
        !has_error_containing(&d, "INVALID_PRIMARY_EXECUTOR"),
        "{d:?}"
    );
}

#[test]
fn v6_rejects_cognitive_cap_with_script_executor() {
    let yaml = r#"
version: "1.0.0"
workflows:
  cap.plan.draft:
    verb: plan
    initialState: drafting
    snippet:
      inputs:  {}
      outputs: {}
    states:
      drafting:
        transitions:
          t:
            target: done
            executor: { kind: script, subject: build.thing }
      done: { terminal: true }
"#;
    let d = diagnostics_for(yaml);
    assert!(
        has_error_containing(&d, "INVALID_PRIMARY_EXECUTOR"),
        "cognitive verb 'plan' must use kind: mcp or noop, not script: {d:?}"
    );
}

#[test]
fn v6_accepts_research_cap_with_inventory_executor() {
    // `cap.research.tool-inventory` surveys the gateway deterministically — a
    // Cognitive verb backed by a deterministic executor is allowed (0.0.21 V6).
    let yaml = r#"
version: "1.0.0"
workflows:
  cap.research.tool-inventory:
    verb: research
    initialState: surveying
    snippet:
      inputs:  {}
      outputs: {}
    states:
      surveying:
        transitions:
          t:
            target: done
            actor: deterministic
            executor: { kind: inventory }
      done: { terminal: true }
"#;
    let d = diagnostics_for(yaml);
    assert!(
        !has_error_containing(&d, "INVALID_PRIMARY_EXECUTOR"),
        "research cap with kind: inventory must pass V6: {d:?}"
    );
}

#[test]
fn v6_accepts_verify_cap_with_path_grounding_executor() {
    let yaml = r#"
version: "1.0.0"
workflows:
  cap.verify.path-grounding:
    verb: verify
    initialState: grounding
    snippet:
      inputs:  {}
      outputs: {}
    states:
      grounding:
        transitions:
          t:
            target: done
            actor: deterministic
            executor: { kind: path_grounding, groundedPaths: ["$.context.files"] }
      done: { terminal: true }
"#;
    let d = diagnostics_for(yaml);
    assert!(
        !has_error_containing(&d, "INVALID_PRIMARY_EXECUTOR"),
        "verify cap with kind: path_grounding must pass V6: {d:?}"
    );
}

// ---------- V7 — flow id matches flow.<name> ----------

#[test]
fn v7_accepts_well_formed_flow_id() {
    let yaml = r#"
version: "1.0.0"
workflows:
  flow.add-feature:
    initialState: ready
    states:
      ready: { terminal: true }
"#;
    let d = diagnostics_for(yaml);
    assert!(!has_error_containing(&d, "INVALID_ID_SHAPE"), "{d:?}");
}

#[test]
fn v7_rejects_flow_id_missing_name_segment() {
    let yaml = r#"
version: "1.0.0"
workflows:
  flow:
    initialState: ready
    states:
      ready: { terminal: true }
"#;
    // `flow` (no dot) → tier Other, not Flow. To get V7 to fire we need
    // an id that LOOKS like flow.X but isn't. Use `flow.` (trailing dot).
    let d = diagnostics_for(yaml);
    // `flow` alone has tier Other so V7 doesn't fire — that's expected.
    assert!(d.iter().all(|m| !m.contains("INVALID_ID_SHAPE")), "{d:?}");

    // Now the actual V7 violation:
    let yaml2 = r#"
version: "1.0.0"
workflows:
  "flow.":
    initialState: ready
    states:
      ready: { terminal: true }
"#;
    let d2 = diagnostics_for(yaml2);
    assert!(
        has_error_containing(&d2, "INVALID_ID_SHAPE"),
        "trailing dot should error V7: {d2:?}"
    );
}

// ---------- V8 — flow has no snippet ----------

#[test]
fn v8_accepts_flow_without_snippet() {
    let yaml = r#"
version: "1.0.0"
workflows:
  flow.add-feature:
    initialState: ready
    states:
      ready: { terminal: true }
"#;
    let d = diagnostics_for(yaml);
    assert!(!has_error_containing(&d, "FLOW_HAS_SNIPPET"), "{d:?}");
}

#[test]
fn v8_rejects_flow_that_declares_snippet() {
    let yaml = r#"
version: "1.0.0"
workflows:
  flow.add-feature:
    initialState: ready
    snippet:
      inputs:  {}
      outputs: {}
    states:
      ready: { terminal: true }
"#;
    let d = diagnostics_for(yaml);
    assert!(has_error_containing(&d, "FLOW_HAS_SNIPPET"), "{d:?}");
}

// ---------- V9 — flow has no verb ----------

#[test]
fn v9_accepts_flow_without_verb() {
    let yaml = r#"
version: "1.0.0"
workflows:
  flow.add-feature:
    initialState: ready
    states:
      ready: { terminal: true }
"#;
    let d = diagnostics_for(yaml);
    assert!(!has_error_containing(&d, "FLOW_HAS_VERB"), "{d:?}");
}

#[test]
fn v9_rejects_flow_that_declares_verb() {
    let yaml = r#"
version: "1.0.0"
workflows:
  flow.add-feature:
    verb: plan
    initialState: ready
    states:
      ready: { terminal: true }
"#;
    let d = diagnostics_for(yaml);
    assert!(has_error_containing(&d, "FLOW_HAS_VERB"), "{d:?}");
}

// ---------- V10 — capability does not invoke another workflow ----------

#[test]
fn v10_accepts_capability_with_no_nested_workflow_invocation() {
    let yaml = r#"
version: "1.0.0"
workflows:
  cap.plan.draft:
    verb: plan
    initialState: drafting
    snippet:
      inputs:  {}
      outputs: {}
    states:
      drafting:
        transitions:
          t:
            target: done
            executor: { kind: mcp, connection: any }
      done: { terminal: true }
"#;
    let d = diagnostics_for(yaml);
    assert!(!has_error_containing(&d, "CAPABILITY_NESTING"), "{d:?}");
}

#[test]
fn v10_rejects_capability_invoking_another_workflow() {
    let yaml = r#"
version: "1.0.0"
workflows:
  cap.plan.helper:
    verb: plan
    initialState: ready
    snippet:
      inputs:  {}
      outputs: {}
    states:
      ready:
        transitions:
          t:
            target: done
            executor: { kind: mcp, connection: any }
      done: { terminal: true }
  cap.plan.draft:
    verb: plan
    initialState: drafting
    snippet:
      inputs:  {}
      outputs: {}
    states:
      drafting:
        transitions:
          t:
            target: done
            executor:
              kind: workflow
              definitionId: cap.plan.helper
              use:
                outputs: {}
      done: { terminal: true }
"#;
    let d = diagnostics_for(yaml);
    assert!(has_error_containing(&d, "CAPABILITY_NESTING"), "{d:?}");
}

// ---------- V11 — flow does not invoke another flow ----------

#[test]
fn v11_accepts_flow_invoking_only_capabilities() {
    let yaml = r#"
version: "1.0.0"
workflows:
  cap.plan.draft:
    verb: plan
    initialState: ready
    snippet:
      inputs:  {}
      outputs: {}
    states:
      ready:
        transitions:
          t:
            target: done
            executor: { kind: mcp, connection: any }
      done: { terminal: true }
  flow.add-feature:
    initialState: planning
    states:
      planning:
        transitions:
          t:
            target: done
            executor:
              kind: workflow
              definitionId: cap.plan.draft
              use:
                outputs: {}
      done: { terminal: true }
"#;
    let d = diagnostics_for(yaml);
    assert!(!has_error_containing(&d, "FLOW_NESTING"), "{d:?}");
}

// V11 RELAXED — a flow MAY now invoke another flow (nested-flow composition,
// e.g. the loom). Recursion is bounded by the runtime sub-workflow depth cap,
// not a static load-time ban. A nested flow types its `use.outputs` from its
// top-level `outputs:` block.
#[test]
fn v11_now_allows_flow_invoking_another_flow() {
    let yaml = r#"
version: "1.0.0"
workflows:
  flow.sub:
    initialState: ready
    outputs:
      result: { type: object }
    states:
      ready: { terminal: true }
  flow.parent:
    initialState: working
    states:
      working:
        transitions:
          t:
            target: done
            executor:
              kind: workflow
              definitionId: flow.sub
              use:
                outputs:
                  "$.context.child_result": result
      done: { terminal: true }
"#;
    let d = diagnostics_for(yaml);
    assert!(!has_error_containing(&d, "FLOW_NESTING"), "{d:?}");
    // The nested flow's `outputs:` types the parent's use.outputs — no
    // UNKNOWN_CAP_OUTPUT_TARGET despite the child being a flow, not a cap.
    assert!(
        !has_error_containing(&d, "UNKNOWN_CAP_OUTPUT_TARGET"),
        "{d:?}"
    );
}

// ---------- V15 — expects_contract_hash matches actual ----------

#[test]
fn v15_accepts_correct_contract_hash_pin() {
    // Compute the hash dynamically since the snippet content drives it.
    let snippet_json = serde_json::json!({
        "inputs":  {},
        "outputs": { "verdict": { "type": "string", "enum": ["pass", "fail"] } }
    });
    let actual_hash = praxec_core::contract_hash::compute_contract_hash(&snippet_json);
    let yaml = format!(
        r#"
version: "1.0.0"
workflows:
  cap.plan.vet:
    verb: plan
    initialState: ready
    snippet:
      inputs:  {{}}
      outputs:
        verdict: {{ type: string, enum: [pass, fail] }}
    states:
      ready:
        transitions:
          t:
            target: done
            executor: {{ kind: mcp, connection: any }}
      done: {{ terminal: true }}
  flow.add-feature:
    initialState: planning
    states:
      planning:
        transitions:
          t:
            target: done
            executor:
              kind: workflow
              definitionId: cap.plan.vet
              expects_contract_hash: "{actual_hash}"
              use:
                outputs:
                  "$.context.verdict": verdict
      done: {{ terminal: true }}
"#
    );
    let d = diagnostics_for(&yaml);
    assert!(!has_error_containing(&d, "CONTRACT_HASH_MISMATCH"), "{d:?}");
}

#[test]
fn v15_rejects_mismatched_contract_hash_pin() {
    let yaml = r#"
version: "1.0.0"
workflows:
  cap.plan.vet:
    verb: plan
    initialState: ready
    snippet:
      inputs:  {}
      outputs:
        verdict: { type: string }
    states:
      ready:
        transitions:
          t:
            target: done
            executor: { kind: mcp, connection: any }
      done: { terminal: true }
  flow.add-feature:
    initialState: planning
    states:
      planning:
        transitions:
          t:
            target: done
            executor:
              kind: workflow
              definitionId: cap.plan.vet
              expects_contract_hash: "sha256:deadbeefdeadbeefdeadbeefdeadbeefdeadbeefdeadbeefdeadbeefdeadbeef"
              use:
                outputs:
                  "$.context.verdict": verdict
      done: { terminal: true }
"#;
    let d = diagnostics_for(yaml);
    assert!(has_error_containing(&d, "CONTRACT_HASH_MISMATCH"), "{d:?}");
}

// ---------- V16 — stable-lifecycle target requires expects_contract_hash ----------

#[test]
fn v16_accepts_stable_target_with_pin() {
    let snippet_json = serde_json::json!({
        "inputs":  {},
        "outputs": { "verdict": { "type": "string" } }
    });
    let actual_hash = praxec_core::contract_hash::compute_contract_hash(&snippet_json);
    let yaml = format!(
        r#"
version: "1.0.0"
workflows:
  cap.plan.vet:
    verb: plan
    lifecycle: stable
    initialState: ready
    snippet:
      inputs:  {{}}
      outputs:
        verdict: {{ type: string }}
    states:
      ready:
        transitions:
          t:
            target: done
            executor: {{ kind: mcp, connection: any }}
      done: {{ terminal: true }}
  flow.add-feature:
    initialState: planning
    states:
      planning:
        transitions:
          t:
            target: done
            executor:
              kind: workflow
              definitionId: cap.plan.vet
              expects_contract_hash: "{actual_hash}"
              use:
                outputs:
                  "$.context.verdict": verdict
      done: {{ terminal: true }}
"#
    );
    let d = diagnostics_for(&yaml);
    assert!(!has_error_containing(&d, "MISSING_CONTRACT_HASH"), "{d:?}");
}

#[test]
fn v16_rejects_stable_target_without_pin() {
    let yaml = r#"
version: "1.0.0"
workflows:
  cap.plan.vet:
    verb: plan
    lifecycle: stable
    initialState: ready
    snippet:
      inputs:  {}
      outputs:
        verdict: { type: string }
    states:
      ready:
        transitions:
          t:
            target: done
            executor: { kind: mcp, connection: any }
      done: { terminal: true }
  flow.add-feature:
    initialState: planning
    states:
      planning:
        transitions:
          t:
            target: done
            executor:
              kind: workflow
              definitionId: cap.plan.vet
              use:
                outputs:
                  "$.context.verdict": verdict
      done: { terminal: true }
"#;
    let d = diagnostics_for(yaml);
    assert!(has_error_containing(&d, "MISSING_CONTRACT_HASH"), "{d:?}");
}

// ---------- ADR-0007 — workflow flow shape ----------

#[test]
fn orchestrator_accepts_a_model_or_agent_name() {
    let yaml = r#"
version: "1.0.0"
workflows:
  demo:
    orchestrator: "anthropic:claude-sonnet-4-6"
    initialState: done
    states:
      done: { terminal: true }
"#;
    resolve_str(yaml).expect("a non-empty orchestrator ref resolves");
}

#[test]
fn orchestrator_rejects_empty_or_non_string() {
    let empty = r#"
version: "1.0.0"
workflows:
  demo:
    orchestrator: ""
    initialState: done
    states:
      done: { terminal: true }
"#;
    let err = resolve_str(empty).expect_err("empty orchestrator must fail fast");
    assert!(
        format!("{err:#}").contains("INVALID_ORCHESTRATOR"),
        "got: {err:#}"
    );

    let non_string = r#"
version: "1.0.0"
workflows:
  demo:
    orchestrator: 42
    initialState: done
    states:
      done: { terminal: true }
"#;
    let err2 = resolve_str(non_string).expect_err("non-string orchestrator must fail fast");
    assert!(
        format!("{err2:#}").contains("INVALID_ORCHESTRATOR"),
        "got: {err2:#}"
    );
}
