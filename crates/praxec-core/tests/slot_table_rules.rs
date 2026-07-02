//! PR3 V13/V14 — slot-table reachability + type consistency, exercised
//! end-to-end through `validate_workflows` so the rules participate in
//! the same code path the binary uses. Naming convention matches the
//! validation-parity scanner: `fn v<N>_(accepts|rejects)_<topic>`.

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

// ---------- V13 — reachability ----------

#[test]
fn v13_accepts_use_inputs_resolving_to_declared_input_slot() {
    let yaml = r#"
version: "1.0.0"
workflows:
  cap.plan.vet:
    verb: plan
    initialState: ready
    snippet:
      inputs:
        plan: { type: object }
      outputs: {}
    states:
      ready:
        transitions:
          t:
            target: done
            executor: { kind: mcp, connection: any }
      done: { terminal: true }
  flow.add-feature:
    inputs:
      draft_plan: { type: object }
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
                inputs:
                  plan: "$.context.draft_plan"
      done: { terminal: true }
"#;
    let d = diagnostics_for(yaml);
    assert!(!has_error_containing(&d, "UNREACHABLE_SLOT"), "{d:?}");
}

#[test]
fn v13_rejects_use_inputs_referencing_undeclared_slot() {
    let yaml = r#"
version: "1.0.0"
workflows:
  cap.plan.vet:
    verb: plan
    initialState: ready
    snippet:
      inputs:
        plan: { type: object }
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
              definitionId: cap.plan.vet
              use:
                inputs:
                  plan: "$.context.nonexistent"
      done: { terminal: true }
"#;
    let d = diagnostics_for(yaml);
    assert!(has_error_containing(&d, "UNREACHABLE_SLOT"), "{d:?}");
    assert!(has_error_containing(&d, "$.context.nonexistent"), "{d:?}");
}

// ---------- V14 — type consistency between states writing the same slot ----------

#[test]
fn v14_accepts_two_states_writing_compatible_types_to_same_slot() {
    let yaml = r#"
version: "1.0.0"
workflows:
  cap.plan.draft:
    verb: plan
    initialState: ready
    snippet:
      inputs:  {}
      outputs:
        verdict: { type: string, enum: [pass, fail] }
    states:
      ready:
        transitions:
          t:
            target: done
            executor: { kind: mcp, connection: any }
      done: { terminal: true }
  flow.add-feature:
    initialState: s1
    states:
      s1:
        transitions:
          t:
            target: s2
            executor:
              kind: workflow
              definitionId: cap.plan.draft
              use:
                outputs:
                  "$.context.verdict": verdict
      s2:
        transitions:
          t:
            target: done
            executor:
              kind: workflow
              definitionId: cap.plan.draft
              use:
                outputs:
                  "$.context.verdict": verdict
      done: { terminal: true }
"#;
    let d = diagnostics_for(yaml);
    assert!(!has_error_containing(&d, "SLOT_TYPE_CONFLICT"), "{d:?}");
}

#[test]
fn v14_rejects_two_states_writing_incompatible_types_to_same_slot() {
    let yaml = r#"
version: "1.0.0"
workflows:
  cap.plan.vetter-a:
    verb: plan
    initialState: ready
    snippet:
      inputs:  {}
      outputs:
        verdict: { type: string, enum: [pass, fail] }
    states:
      ready:
        transitions:
          t:
            target: done
            executor: { kind: mcp, connection: any }
      done: { terminal: true }
  cap.plan.vetter-b:
    verb: plan
    initialState: ready
    snippet:
      inputs:  {}
      outputs:
        verdict: { type: string, enum: [approved, rejected] }
    states:
      ready:
        transitions:
          t:
            target: done
            executor: { kind: mcp, connection: any }
      done: { terminal: true }
  flow.add-feature:
    initialState: s1
    states:
      s1:
        transitions:
          t:
            target: s2
            executor:
              kind: workflow
              definitionId: cap.plan.vetter-a
              use:
                outputs:
                  "$.context.verdict": verdict
      s2:
        transitions:
          t:
            target: done
            executor:
              kind: workflow
              definitionId: cap.plan.vetter-b
              use:
                outputs:
                  "$.context.verdict": verdict
      done: { terminal: true }
"#;
    let d = diagnostics_for(yaml);
    assert!(has_error_containing(&d, "SLOT_TYPE_CONFLICT"), "{d:?}");
}
