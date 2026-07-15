//! #5 (v0.0.22 hardening) — serde round-trip identity for persisted types.
//!
//! `deserialize(serialize(x))` must re-serialize to the same JSON. This is the
//! regression net for schema cutovers: the 0.0.21 `run_env` field replaced the
//! standalone `trace_id`/`run_id`, and a future rename/drop that silently loses
//! a field would change the re-serialized JSON and fail here. We compare the
//! twice-serialized JSON (not the struct) because `WorkflowInstance` has no
//! `PartialEq` — and this also proves no field is dropped on the way through.

use praxec_core::model::{ParentLink, WorkflowInstance};
use serde_json::json;

/// A maximally-populated instance so every optional/`skip_serializing_if` field
/// (parent, cancelled_*, run/trace correlation) is exercised by the round-trip.
fn fully_populated_instance() -> WorkflowInstance {
    WorkflowInstance {
        id: "wf-1".into(),
        definition_id: "def".into(),
        definition_version: "3".into(),
        definition: json!({ "initialState": "s", "states": {} }),
        state: "s".into(),
        version: 7,
        input: json!({ "feature": "x" }),
        context: json!({ "slot": 1, "nested": { "a": [1, 2] } }),
        started_at: chrono::Utc::now(),
        run_env: praxec_core::RunEnv::new(
            praxec_core::RepoRoot::for_test(),
            Some("run-9".into()),
            Some("trace-9".into()),
        ),
        cancelled_at: Some(chrono::Utc::now()),
        cancelled_reason: Some("operator abort".into()),
        depth: 2,
        parent: Some(ParentLink {
            workflow_id: "parent-wf".into(),
            transition: "spawn".into(),
        }),
    }
}

#[test]
fn workflow_instance_round_trips_through_serde() {
    let inst = fully_populated_instance();
    let once = serde_json::to_value(&inst).expect("serialize");
    let back: WorkflowInstance = serde_json::from_value(once.clone()).expect("deserialize");
    let twice = serde_json::to_value(&back).expect("re-serialize");
    assert_eq!(once, twice, "WorkflowInstance serde is not a round-trip");
}

/// Atomic: the run-ambient env (repo_root + run/trace ids) survives the round
/// trip — the exact fields the 0.0.21 cutover introduced.
#[test]
fn workflow_instance_round_trip_preserves_run_env() {
    let inst = fully_populated_instance();
    let json = serde_json::to_value(&inst).unwrap();
    let back: WorkflowInstance = serde_json::from_value(json).unwrap();
    assert_eq!(
        back.run_env.repo_root.as_str(),
        inst.run_env.repo_root.as_str()
    );
    assert_eq!(back.run_env.run_id.as_deref(), Some("run-9"));
    assert_eq!(back.run_env.trace_id.as_deref(), Some("trace-9"));
}

/// Adversarial: an instance snapshot WITHOUT `run_env` must fail to deserialize
/// (no serde default — the documented store-wipe cutover), not silently
/// materialize a bogus root.
#[test]
fn workflow_instance_without_run_env_is_rejected() {
    let inst = fully_populated_instance();
    let mut json = serde_json::to_value(&inst).unwrap();
    json.as_object_mut().unwrap().remove("run_env");
    let parsed: Result<WorkflowInstance, _> = serde_json::from_value(json);
    assert!(
        parsed.is_err(),
        "a snapshot lacking run_env must be rejected, not defaulted"
    );
}
