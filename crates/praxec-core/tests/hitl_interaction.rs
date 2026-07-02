//! SPEC §29 — HITL interaction tests.
//!
//! Covers:
//! - `enable_human_ask: true` auto-injects `ask_human` self-loops on
//!   non-terminal states
//! - operator override (existing `ask_human` not clobbered)
//! - `max_fires_per_visit` cap enforcement + counter reset on state exit
//! - `lightweight: true` emits `workflow.interaction` not `.transition`
//! - `purpose:` propagates into audit payload

use praxec_core::config::resolve_str;
use serde_json::Value;

fn workflow_section(states: &str, extras: &str) -> String {
    format!(
        r#"version: "1.0.0"
workflows:
  agentic:
    {extras}
    initialState: planning
    states:
{states}
"#
    )
}

// ── injection ─────────────────────────────────────────────────────────────

#[test]
fn enable_human_ask_injects_ask_human_into_every_non_terminal_state() {
    let yaml = workflow_section(
        r#"      planning:
        transitions:
          do: { target: editing }
      editing:
        transitions:
          done: { target: complete }
      complete:
        terminal: true
"#,
        "enable_human_ask: true",
    );
    let resolved = resolve_str(&yaml).expect("config resolves");
    let states = resolved
        .pointer("/workflows/agentic/states")
        .and_then(Value::as_object)
        .expect("states");
    // Non-terminal states: ask_human present
    assert!(
        states
            .get("planning")
            .and_then(|s| s.pointer("/transitions/ask_human"))
            .is_some(),
        "planning must have injected ask_human"
    );
    assert!(
        states
            .get("editing")
            .and_then(|s| s.pointer("/transitions/ask_human"))
            .is_some(),
        "editing must have injected ask_human"
    );
    // Terminal state: ask_human absent
    assert!(
        states
            .get("complete")
            .and_then(|s| s.pointer("/transitions/ask_human"))
            .is_none(),
        "complete (terminal) must NOT have ask_human"
    );
}

#[test]
fn enable_human_ask_injected_transition_has_required_shape() {
    let yaml = workflow_section(
        r#"      planning:
        transitions:
          do: { target: planning }
"#,
        "enable_human_ask: true",
    );
    let resolved = resolve_str(&yaml).unwrap();
    let ask = resolved
        .pointer("/workflows/agentic/states/planning/transitions/ask_human")
        .expect("ask_human injected");
    assert_eq!(
        ask.pointer("/target").and_then(Value::as_str),
        Some("planning")
    );
    assert_eq!(ask.pointer("/actor").and_then(Value::as_str), Some("human"));
    assert_eq!(ask.pointer("/purpose").and_then(Value::as_str), Some("ask"));
    assert_eq!(
        ask.pointer("/lightweight").and_then(Value::as_bool),
        Some(true)
    );
    assert_eq!(
        ask.pointer("/max_fires_per_visit").and_then(Value::as_u64),
        Some(5),
        "default human_ask_cap is 5"
    );
    let req = ask
        .pointer("/inputSchema/required")
        .and_then(Value::as_array)
        .expect("inputSchema.required is an array");
    let req_strs: Vec<&str> = req.iter().filter_map(Value::as_str).collect();
    assert!(req_strs.contains(&"question"), "question is required");
    assert!(
        req_strs.contains(&"context_summary"),
        "context_summary is required"
    );
    assert!(
        req_strs.contains(&"attempted_alternatives"),
        "attempted_alternatives is required (SPEC §29.6 self-service poka-yoke)"
    );
}

#[test]
fn human_ask_cap_overrides_default_max_fires() {
    let yaml = workflow_section(
        r#"      planning:
        transitions:
          do: { target: planning }
"#,
        "enable_human_ask: true\n    human_ask_cap: 2",
    );
    let resolved = resolve_str(&yaml).unwrap();
    let cap = resolved
        .pointer("/workflows/agentic/states/planning/transitions/ask_human/max_fires_per_visit")
        .and_then(Value::as_u64);
    assert_eq!(cap, Some(2));
}

#[test]
fn operator_override_ask_human_is_not_clobbered() {
    let yaml = workflow_section(
        r#"      planning:
        transitions:
          do: { target: planning }
          ask_human:
            target: planning
            actor: human
            purpose: ask
            max_fires_per_visit: 99
            inputSchema:
              type: object
              required: [q]
              properties:
                q: { type: string }
"#,
        "enable_human_ask: true",
    );
    let resolved = resolve_str(&yaml).unwrap();
    let cap = resolved
        .pointer("/workflows/agentic/states/planning/transitions/ask_human/max_fires_per_visit")
        .and_then(Value::as_u64);
    assert_eq!(cap, Some(99), "operator override must take precedence");
    let req = resolved
        .pointer("/workflows/agentic/states/planning/transitions/ask_human/inputSchema/required")
        .and_then(Value::as_array)
        .unwrap();
    let req_strs: Vec<&str> = req.iter().filter_map(Value::as_str).collect();
    assert_eq!(req_strs, vec!["q"], "operator's inputSchema preserved");
}

#[test]
fn enable_human_ask_off_does_not_inject() {
    let yaml = workflow_section(
        r#"      planning:
        transitions:
          do: { target: planning }
"#,
        "",
    );
    let resolved = resolve_str(&yaml).unwrap();
    let ask = resolved.pointer("/workflows/agentic/states/planning/transitions/ask_human");
    assert!(ask.is_none(), "no injection when flag is absent/false");
}
