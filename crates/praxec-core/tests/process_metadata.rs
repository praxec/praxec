//! Phase 2 (intent-driven loom) — the optional `process`/`taskClass` flow tag
//! that marks a flow as an auto-selectable process for the intent index.
//! Validated at load: it must be a non-empty string, and — the R3 reward-gaming
//! guard — a process-tagged flow MUST declare ≥1 `outcome` (a 0-outcome process
//! "succeeds" vacuously and would otherwise top the intent index).

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

/// A flow with `process:` + the given `outcomes:` snippet spliced in.
fn flow(process_line: &str, outcomes_block: &str) -> String {
    format!(
        r#"
version: "1.0.0"
workflows:
  flow.ship:
    inputs: {{}}
{process_line}
{outcomes_block}
    initialState: s0
    states:
      s0:
        transitions:
          go:
            target: end
            executor: {{ kind: mcp, connection: any }}
      end: {{ terminal: true, outcome: success }}
"#
    )
}

const ONE_OUTCOME: &str =
    "    outcomes:\n      - { id: ok, statement: \"shipped\", check: \"$.context.x == true\" }";

#[test]
fn process_tag_accepts_a_named_process_with_an_outcome() {
    let d = diagnostics_for(&flow("    process: engineering", ONE_OUTCOME));
    assert!(!has_error_containing(&d, "INVALID_PROCESS"), "{d:?}");
    assert!(
        !has_error_containing(&d, "PROCESS_REQUIRES_OUTCOME"),
        "{d:?}"
    );
}

#[test]
fn process_tag_is_optional_untagged_flow_has_no_process_errors() {
    let d = diagnostics_for(&flow("", ""));
    assert!(!has_error_containing(&d, "INVALID_PROCESS"), "{d:?}");
    assert!(
        !has_error_containing(&d, "PROCESS_REQUIRES_OUTCOME"),
        "{d:?}"
    );
}

#[test]
fn process_tag_rejects_an_empty_string() {
    let d = diagnostics_for(&flow("    process: \"\"", ONE_OUTCOME));
    assert!(
        has_error_containing(&d, "INVALID_PROCESS"),
        "empty process must be rejected: {d:?}"
    );
}

#[test]
fn process_tag_rejects_a_non_string() {
    let d = diagnostics_for(&flow("    process: 42", ONE_OUTCOME));
    assert!(
        has_error_containing(&d, "INVALID_PROCESS"),
        "non-string process must be rejected: {d:?}"
    );
}

#[test]
fn process_tagged_flow_without_outcomes_is_rejected_r3() {
    // The reward-gaming guard: an auto-selectable process must declare what
    // "done" means, or the intent index would learn it as a vacuous winner.
    let d = diagnostics_for(&flow("    process: engineering", ""));
    assert!(
        has_error_containing(&d, "PROCESS_REQUIRES_OUTCOME"),
        "process-tagged flow with no outcomes must be rejected: {d:?}"
    );
}
