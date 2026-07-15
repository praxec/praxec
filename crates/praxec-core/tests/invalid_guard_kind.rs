//! SPEC §9 — guard `kind:` is a closed set. A typo (e.g. `permissoin`)
//! must be caught at `praxec check` time, and the runtime evaluator
//! must surface `INVALID_GUARD_KIND` as defense-in-depth if a
//! pre-validated definition somehow reaches it.

use chrono::Utc;
use praxec_core::guards::{DefaultGuardEvaluator, GuardKind};
use praxec_core::model::{Principal, WorkflowInstance};
use praxec_core::ports::GuardEvaluator;
use praxec_core::validate::validate_workflows;
use serde_json::json;

fn instance() -> WorkflowInstance {
    WorkflowInstance {
        id: "wf_test".into(),
        definition_id: "demo".into(),
        definition_version: "0".into(),
        definition: json!({}),
        state: "s".into(),
        version: 0,
        input: json!({}),
        context: json!({}),
        started_at: Utc::now(),
        run_env: praxec_core::RunEnv::for_test(),
        cancelled_at: None,
        cancelled_reason: None,
        depth: 0,
        parent: None,
    }
}

// ── Runtime backstop ────────────────────────────────────────────────────────

#[tokio::test]
async fn runtime_evaluator_returns_invalid_guard_kind_on_typo() {
    let evaluator = DefaultGuardEvaluator::new();
    let guard = json!({ "kind": "permissoin", "permission": "act" }); // typo
    let err = evaluator
        .evaluate(&guard, &instance(), &json!({}), &Principal::anonymous())
        .await
        .expect_err("invalid guard kind must surface as an error, not silent false");
    let msg = err.to_string();
    assert!(
        msg.contains("INVALID_GUARD_KIND"),
        "error should carry INVALID_GUARD_KIND code, got: {msg}"
    );
    assert!(
        msg.contains("permissoin"),
        "error should echo the offending kind for diagnosis, got: {msg}"
    );
}

#[tokio::test]
async fn runtime_evaluator_treats_missing_kind_as_invalid() {
    let evaluator = DefaultGuardEvaluator::new();
    let guard = json!({ "permission": "act" }); // no `kind:` at all
    let err = evaluator
        .evaluate(&guard, &instance(), &json!({}), &Principal::anonymous())
        .await
        .expect_err("missing guard kind must surface as an error");
    assert!(err.to_string().contains("INVALID_GUARD_KIND"), "got: {err}");
}

// ── Load-time validation ────────────────────────────────────────────────────

#[test]
fn check_rejects_invalid_guard_kind_at_load_time() {
    let cfg = json!({
        "version": "1.0.0",
        "workflows": {
            "demo": {
                "initialState": "draft",
                "states": {
                    "draft": {
                        "transitions": {
                            "submit": {
                                "target": "done",
                                "actor": "agent",
                                "guards": [
                                    { "kind": "permissoin", "permission": "act" }
                                ]
                            }
                        }
                    },
                    "done": { "terminal": true }
                }
            }
        }
    });
    let diags = validate_workflows(&cfg);
    let errors: Vec<_> = diags
        .iter()
        .filter(|d| d.is_error() && d.message().contains("permissoin"))
        .collect();
    assert!(
        !errors.is_empty(),
        "validator must reject 'permissoin' typo at load; got: {diags:?}"
    );
    assert!(
        errors[0].message().contains("invalid kind"),
        "diagnostic should flag the typo as an invalid kind, got: {}",
        errors[0].message()
    );
}

#[test]
fn check_rejects_invalid_kind_nested_inside_all_of() {
    let cfg = json!({
        "version": "1.0.0",
        "workflows": {
            "demo": {
                "initialState": "draft",
                "states": {
                    "draft": {
                        "transitions": {
                            "submit": {
                                "target": "done",
                                "actor": "agent",
                                "guards": [{
                                    "kind": "all_of",
                                    "guards": [
                                        { "kind": "permission", "permission": "p" },
                                        { "kind": "rol", "role": "admin" }
                                    ]
                                }]
                            }
                        }
                    },
                    "done": { "terminal": true }
                }
            }
        }
    });
    let diags = validate_workflows(&cfg);
    assert!(
        diags
            .iter()
            .any(|d| d.is_error() && d.message().contains("'rol'")),
        "validator must recurse into all_of and flag nested invalid kind; got: {diags:?}"
    );
}

// ── Poka-yoke: enum is the single source of truth ─────────────────────────

#[test]
fn guard_kind_round_trips_through_string() {
    // Every variant survives `as_str` -> `from_str`. If a developer adds
    // a variant without updating `from_str`, this test fires.
    for k in GuardKind::ALL {
        let parsed = GuardKind::from_token(k.as_str())
            .unwrap_or_else(|| panic!("from_str missing arm for {:?}", k));
        assert_eq!(*k, parsed, "round-trip mismatch for {:?}", k);
    }
}

#[tokio::test]
async fn runtime_evaluator_recognises_every_guard_kind_variant() {
    // Probe each variant with a minimal-shape guard. The assertion is
    // narrow: the runtime MUST NOT return INVALID_GUARD_KIND for any
    // variant. Other error paths (UNSET_SLOT, missing required field)
    // are fine — they indicate the variant was reached and dispatched.
    // This test is the structural sibling to the exhaustive match in
    // `DefaultGuardEvaluator::evaluate`; together they make it
    // impossible to add a variant without wiring the evaluator.
    let evaluator = DefaultGuardEvaluator::new();
    let principal = Principal::anonymous();
    let inst = instance();
    for kind in GuardKind::ALL {
        let guard = match kind {
            GuardKind::Permission => json!({ "kind": "permission", "permission": "x" }),
            GuardKind::Role => json!({ "kind": "role", "role": "x" }),
            GuardKind::Expr => json!({ "kind": "expr", "expr": "1 == 1" }),
            GuardKind::Jsonpath => json!({ "kind": "jsonpath", "expr": "1 == 1" }),
            GuardKind::AllOf => json!({ "kind": "all_of", "guards": [] }),
            GuardKind::AnyOf => json!({ "kind": "any_of", "guards": [] }),
            GuardKind::Not => {
                json!({ "kind": "not", "guard": { "kind": "permission", "permission": "x" } })
            }
            GuardKind::GuidanceAcknowledged => {
                json!({ "kind": "guidance_acknowledged", "subject": "x" })
            }
            GuardKind::ScriptAcknowledged => {
                json!({ "kind": "script_acknowledged", "subject": "x" })
            }
            GuardKind::Evidence => json!({ "kind": "evidence", "requires": [] }),
        };
        let result = evaluator
            .evaluate(&guard, &inst, &json!({}), &principal)
            .await;
        if let Err(e) = &result {
            assert!(
                !e.to_string().contains("INVALID_GUARD_KIND"),
                "evaluator returned INVALID_GUARD_KIND for known variant {:?}: {e}",
                kind
            );
        }
    }
}

#[test]
fn check_accepts_all_known_guard_kinds() {
    // Smoke-test: every documented kind should pass the kind check.
    // (Other validation may still flag unrelated issues; we only check
    // INVALID_GUARD_KIND diagnostics are absent.)
    let cfg = json!({
        "version": "1.0.0",
        "workflows": {
            "demo": {
                "initialState": "draft",
                "blackboard": { "flag": "boolean" },
                "states": {
                    "draft": {
                        "transitions": {
                            "submit": {
                                "target": "done",
                                "actor": "agent",
                                "guards": [
                                    { "kind": "permission", "permission": "p" },
                                    { "kind": "role", "role": "admin" },
                                    { "kind": "expr", "expr": "$.context.flag == true" },
                                    {
                                        "kind": "not",
                                        "guard": { "kind": "permission", "permission": "deny" }
                                    },
                                    {
                                        "kind": "any_of",
                                        "guards": [
                                            { "kind": "role", "role": "a" },
                                            { "kind": "role", "role": "b" }
                                        ]
                                    }
                                ]
                            }
                        }
                    },
                    "done": { "terminal": true }
                }
            }
        }
    });
    let diags = validate_workflows(&cfg);
    let invalid_kind_errs: Vec<_> = diags
        .iter()
        .filter(|d| d.message().contains("invalid kind"))
        .collect();
    assert!(
        invalid_kind_errs.is_empty(),
        "no INVALID_GUARD_KIND diagnostics expected; got: {invalid_kind_errs:?}"
    );
}
