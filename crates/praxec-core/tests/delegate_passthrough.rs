//! SPEC §21 — `delegate` is a pass-through string from workflow state to
//! response. The gateway never reads or branches on the value; it only
//! validates shape (non-empty string) at config load and surfaces verbatim
//! at the top level of every workflow response.
//!
//! FMECA-style atomic assertions: one behavior per test.

mod common;

use common::{AnyKind, FixedExecutor, Scenario, anon};
use serde_json::{Value, json};
use std::sync::Arc;

fn delegated_workflow_yaml() -> &'static str {
    r#"
version: "1.0.0"
workflows:
  demo:
    initialState: planning
    states:
      planning:
        delegate: planning-agent
        goal: Plan the change
        guidance: Think then submit ready.
        transitions:
          ready:
            target: done
            executor: { kind: noop }
      done:
        terminal: true
"#
}

fn undelegated_workflow_yaml() -> &'static str {
    r#"
version: "1.0.0"
workflows:
  demo:
    initialState: working
    states:
      working:
        goal: Just work
        transitions:
          done_t:
            target: done
            executor: { kind: noop }
      done:
        terminal: true
"#
}

fn fixed_noop_registry() -> Arc<AnyKind> {
    Arc::new(AnyKind(Arc::new(FixedExecutor::new(json!({})))))
}

// ── Positive: delegate declared → surfaced at top-level of response ────────

#[tokio::test]
async fn delegate_surfaces_at_top_level_on_start() {
    let mut sc = Scenario::build(delegated_workflow_yaml(), fixed_noop_registry());
    let resp = sc.start("demo", json!({}), anon()).await;
    assert_eq!(
        resp["delegate"].as_str(),
        Some("planning-agent"),
        "delegate must appear at top level on start response; got: {resp}"
    );
}

#[tokio::test]
async fn delegate_surfaces_at_top_level_after_submit() {
    let mut sc = Scenario::build(delegated_workflow_yaml(), fixed_noop_registry());
    let _ = sc.start("demo", json!({}), anon()).await;
    // Workflow is at `planning` (delegated). After submitting `ready`, we
    // land at `done` which has no delegate — delegate field MUST be absent.
    let resp = sc.submit("ready", json!({}), anon()).await;
    assert_eq!(
        resp["delegate"],
        Value::Null,
        "delegate must NOT appear on response when current state has no delegate; \
         got top-level keys: {:?}",
        resp.as_object().map(|m| m.keys().collect::<Vec<_>>())
    );
}

// ── Negative: state without `delegate` → field fully absent ───────────────

#[tokio::test]
async fn delegate_absent_when_state_has_no_delegate_field() {
    let mut sc = Scenario::build(undelegated_workflow_yaml(), fixed_noop_registry());
    let resp = sc.start("demo", json!({}), anon()).await;
    // Strict absence — the runtime must not emit `delegate: null` either.
    // §21: "It is read at response-build time and surfaced verbatim ..." —
    // when the state has no delegate, the field MUST be fully absent.
    assert!(
        !resp.as_object().unwrap().contains_key("delegate"),
        "delegate key must be fully absent (not null); got: {resp}"
    );
}

// ── Negative: empty-string delegate → INVALID_DELEGATE at load ────────────

#[tokio::test]
async fn empty_delegate_string_rejects_at_config_load() {
    let bad = r#"
version: "1.0.0"
workflows:
  demo:
    initialState: planning
    states:
      planning:
        delegate: ""
        transitions:
          ready:
            target: done
            executor: { kind: noop }
      done:
        terminal: true
"#;
    let err = praxec_core::config::resolve_str(bad).expect_err("empty delegate must reject");
    let s = format!("{err:?}");
    assert!(
        s.contains("INVALID_DELEGATE"),
        "error must name INVALID_DELEGATE; got: {s}"
    );
    assert!(
        s.contains("planning"),
        "error must name the offending state; got: {s}"
    );
}

// ── Negative: non-string delegate → INVALID_DELEGATE at load ──────────────

#[tokio::test]
async fn numeric_delegate_rejects_at_config_load() {
    let bad = r#"
version: "1.0.0"
workflows:
  demo:
    initialState: planning
    states:
      planning:
        delegate: 42
        transitions:
          ready:
            target: done
            executor: { kind: noop }
      done:
        terminal: true
"#;
    let err = praxec_core::config::resolve_str(bad).expect_err("numeric delegate must reject");
    let s = format!("{err:?}");
    assert!(
        s.contains("INVALID_DELEGATE"),
        "error must name INVALID_DELEGATE; got: {s}"
    );
    assert!(
        s.contains("number"),
        "error must describe the wrong-kind shape ('number'); got: {s}"
    );
}

// ── Snapshot: delegate from start-time config persists across reloads ─────

#[tokio::test]
async fn delegate_value_comes_from_instance_snapshot_not_live_config() {
    // SPEC §8.2 — instances carry their definition snapshot. If the live
    // config were re-loaded between start and submit, the delegate value
    // returned on a `workflow.get` MUST still be the one that existed at
    // start time. We don't reload here (single in-memory store), but the
    // test pins the contract: response.delegate == state.delegate from
    // the snapshot, not from any external lookup.
    let mut sc = Scenario::build(delegated_workflow_yaml(), fixed_noop_registry());
    let resp = sc.start("demo", json!({}), anon()).await;
    assert_eq!(resp["delegate"].as_str(), Some("planning-agent"));
}

// ── ADR-0009: workflow-level `orchestrator` (the driving actor) surfaces ───

fn orchestrated_workflow_yaml() -> &'static str {
    r#"
version: "1.0.0"
workflows:
  demo:
    orchestrator: "anthropic:claude-sonnet-4-6"
    initialState: working
    states:
      working:
        goal: Work
        transitions:
          done_t:
            target: done
            executor: { kind: noop }
      done:
        terminal: true
"#
}

#[tokio::test]
async fn orchestrator_binding_surfaces_at_top_level() {
    let mut sc = Scenario::build(orchestrated_workflow_yaml(), fixed_noop_registry());
    let resp = sc.start("demo", json!({}), anon()).await;
    assert_eq!(
        resp["orchestrator"].as_str(),
        Some("anthropic:claude-sonnet-4-6"),
        "the orchestrator binding must surface at top level so a mediator can show \
         'driven by X'; got: {resp}"
    );
}

#[tokio::test]
async fn orchestrator_absent_when_workflow_declares_none() {
    let mut sc = Scenario::build(undelegated_workflow_yaml(), fixed_noop_registry());
    let resp = sc.start("demo", json!({}), anon()).await;
    assert_eq!(resp["orchestrator"], Value::Null);
}
