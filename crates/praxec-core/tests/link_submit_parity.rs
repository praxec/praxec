//! #1 (v0.0.22 hardening) — HATEOAS link ↔ submit parity.
//!
//! The gateway must never do two things: surface an action that a submit would
//! reject, or hide an action a submit would accept. Under `linkFilter: byGuards`
//! with argument-independent guards, the surfaced links MUST equal the set of
//! transitions a submit accepts. This test pins that, plus the two DELIBERATE
//! asymmetries the runtime documents:
//!   - the link filter checks guards but not the actor gate, so an `actor: human`
//!     transition is linked yet rejected for a non-human principal;
//!   - the default (`linkFilter: all`) surfaces guard-failing transitions on
//!     purpose — links are discovery, submit is enforcement.

mod common;

use std::sync::Arc;

use common::{AnyKind, FixedExecutor, Scenario, anon, human};
use serde_json::{Value, json};

fn scenario(yaml: &str) -> Scenario {
    let exec = Arc::new(FixedExecutor::new(json!({})));
    Scenario::build(yaml, Arc::new(AnyKind(exec)))
}

fn rejected_code(resp: &Value) -> Option<String> {
    resp.get("error")
        .and_then(|e| e.get("code"))
        .and_then(Value::as_str)
        .map(str::to_string)
}

const BYGUARDS: &str = r#"
version: "1.0.0"
workflows:
  demo:
    linkFilter: byGuards
    initialState: gate
    states:
      gate:
        transitions:
          allow:
            target: done
            executor: { kind: noop }
            guards: [{ kind: expr, expr: "$.workflow.input.ok == 1" }]
          deny:
            target: done
            executor: { kind: noop }
            guards: [{ kind: expr, expr: "$.workflow.input.ok == 0" }]
      done: { terminal: true }
"#;

/// Under byGuards, a guard-passing transition is BOTH linked and accepted.
#[tokio::test]
async fn linked_transition_is_accepted_by_submit() {
    let mut s = scenario(BYGUARDS);
    s.start("demo", json!({ "ok": 1 }), anon()).await;
    let links = s.link_rels();
    assert!(links.contains(&"allow".to_string()), "links: {links:?}");

    s.start("demo", json!({ "ok": 1 }), anon()).await;
    let resp = s.submit("allow", json!({}), anon()).await.clone();
    assert_eq!(
        rejected_code(&resp),
        None,
        "a linked transition must be accepted: {resp:?}"
    );
}

/// Under byGuards, a guard-failing transition is BOTH hidden and rejected.
#[tokio::test]
async fn unlinked_transition_is_rejected_by_submit() {
    let mut s = scenario(BYGUARDS);
    s.start("demo", json!({ "ok": 1 }), anon()).await;
    let links = s.link_rels();
    assert!(!links.contains(&"deny".to_string()), "links: {links:?}");

    s.start("demo", json!({ "ok": 1 }), anon()).await;
    let resp = s.submit("deny", json!({}), anon()).await.clone();
    assert!(
        rejected_code(&resp).is_some_and(|c| c.contains("GUARD")),
        "a hidden (guard-failing) transition must be rejected: {resp:?}"
    );
}

const HUMAN_GATE: &str = r#"
version: "1.0.0"
workflows:
  demo:
    linkFilter: byGuards
    initialState: gate
    states:
      gate:
        transitions:
          approve:
            target: done
            actor: human
            executor: { kind: noop }
            guards: [{ kind: expr, expr: "1 == 1" }]
      done: { terminal: true }
"#;

/// Documented asymmetry: the link filter checks guards, not the actor gate. An
/// `actor: human` transition whose guard passes IS linked, yet a non-human
/// principal's submit is rejected `ACTOR_MISMATCH`. Pinned so a future change to
/// either side is a conscious decision, not a silent drift.
#[tokio::test]
async fn human_transition_is_linked_but_rejected_for_a_non_human_principal() {
    let mut s = scenario(HUMAN_GATE);
    s.start("demo", json!({}), anon()).await;
    assert!(
        s.link_rels().contains(&"approve".to_string()),
        "a guard-passing human transition is still surfaced"
    );

    s.start("demo", json!({}), anon()).await;
    let resp = s.submit("approve", json!({}), anon()).await.clone();
    assert!(
        rejected_code(&resp).is_some_and(|c| c.contains("ACTOR")),
        "a non-human submit of a human transition must be rejected: {resp:?}"
    );
}

/// The same human transition IS accepted for a human principal — proving the
/// rejection above is the actor gate, not a broken guard.
#[tokio::test]
async fn human_transition_is_accepted_for_a_human_principal() {
    let mut s = scenario(HUMAN_GATE);
    s.start("demo", json!({}), human()).await;
    let resp = s.submit("approve", json!({}), human()).await.clone();
    assert_eq!(
        rejected_code(&resp),
        None,
        "human submit must be accepted: {resp:?}"
    );
}

const DISCOVERY: &str = r#"
version: "1.0.0"
workflows:
  demo:
    initialState: gate
    states:
      gate:
        transitions:
          blocked:
            target: done
            executor: { kind: noop }
            guards: [{ kind: expr, expr: "$.workflow.input.ok == 1" }]
      done: { terminal: true }
"#;

/// Default (`linkFilter: all`): a guard-FAILING transition is surfaced on purpose
/// (discovery), but submit still enforces the guard. Pins that links != legality
/// unless byGuards is set — so the parity property above is scoped correctly.
#[tokio::test]
async fn default_links_are_discovery_and_submit_still_enforces() {
    let mut s = scenario(DISCOVERY);
    s.start("demo", json!({ "ok": 0 }), anon()).await;
    assert!(
        s.link_rels().contains(&"blocked".to_string()),
        "default linkFilter surfaces guard-failing transitions (discovery)"
    );

    s.start("demo", json!({ "ok": 0 }), anon()).await;
    let resp = s.submit("blocked", json!({}), anon()).await.clone();
    assert!(
        rejected_code(&resp).is_some_and(|c| c.contains("GUARD")),
        "submit must enforce the guard even though the link was surfaced: {resp:?}"
    );
}
