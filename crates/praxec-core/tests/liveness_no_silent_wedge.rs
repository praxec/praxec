//! #4 (v0.0.22 hardening) — no silent wedge (guard-aware liveness).
//!
//! The validator already proves the STRUCTURAL liveness invariant at load: every
//! reachable non-terminal state has a path to a terminal (validate.rs terminal-
//! reachability), and deterministic cycles are rejected. What it CANNOT prove
//! statically is guard-aware liveness — whether the guards on those paths are
//! ever satisfiable (that's undecidable in general, so a load-time rule would
//! either miss cases or false-positive on valid packs).
//!
//! So we pin the SAFETY property that actually matters at runtime instead: a run
//! that structurally can reach a terminal but whose only exit is permanently
//! guard-blocked must NEVER falsely report `succeeded`. It stays put, surfaces no
//! legal action, and rejects the blocked exit. A regression that let a wedged run
//! report success (or auto-advance past a false guard) fails here.

mod common;

use std::sync::Arc;

use common::{AnyKind, FixedExecutor, Scenario, anon};
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

// `gate` structurally CAN reach `done` (an edge exists, so terminal-reachability
// passes at load), but the edge's guard is permanently false, so the run can
// never actually advance — a guard-aware livelock the static checker can't see.
const WEDGED: &str = r#"
version: "1.0.0"
workflows:
  demo:
    linkFilter: byGuards
    initialState: gate
    states:
      gate:
        transitions:
          advance:
            target: done
            executor: { kind: noop }
            guards: [{ kind: expr, expr: "1 == 2" }]
      done: { terminal: true }
"#;

/// A guard-wedged run must NOT falsely report `succeeded`, and must stay in the
/// wedged state rather than silently completing.
#[tokio::test]
async fn a_guard_wedged_run_never_falsely_succeeds() {
    let mut s = scenario(WEDGED);
    let resp = s.start("demo", json!({}), anon()).await.clone();
    assert_eq!(
        resp["workflow"]["state"],
        json!("gate"),
        "a wedged run must stay in its state, not advance: {resp:?}"
    );
    assert_ne!(
        resp["result"]["status"],
        json!("succeeded"),
        "a wedged run must never report succeeded: {resp:?}"
    );
}

/// Under byGuards the wedged run surfaces NO legal action — it does not offer an
/// exit it would then reject.
#[tokio::test]
async fn a_guard_wedged_run_surfaces_no_legal_action() {
    let mut s = scenario(WEDGED);
    s.start("demo", json!({}), anon()).await;
    let links = s.link_rels();
    assert!(
        links.is_empty(),
        "no legal action should be surfaced: {links:?}"
    );
}

/// The only structural exit is genuinely blocked — a submit of it is rejected,
/// confirming the wedge is real (not a missing-executor artifact).
#[tokio::test]
async fn a_guard_wedged_run_rejects_its_only_exit() {
    let mut s = scenario(WEDGED);
    s.start("demo", json!({}), anon()).await;
    let resp = s.submit("advance", json!({}), anon()).await.clone();
    assert!(
        rejected_code(&resp).is_some_and(|c| c.contains("GUARD")),
        "the permanently-guarded exit must reject: {resp:?}"
    );
}
