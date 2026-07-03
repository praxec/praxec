//! Per-`GuardKind` accept+reject matrix for `DefaultGuardEvaluator`, plus
//! composite (`all_of` / `any_of` / `not`) behavior including a deep nest.
//! The acknowledgment/evidence kinds have their own dedicated suites
//! (`guidance_acknowledged.rs`, `script_acknowledged_guard.rs`,
//! `evidence_guard*.rs`); this file covers the principal/expr/composite arms.

use praxec_core::guards::DefaultGuardEvaluator;
use praxec_core::model::{Principal, WorkflowInstance};
use praxec_core::ports::GuardEvaluator;
use serde_json::{Value, json};

fn instance(context: Value) -> WorkflowInstance {
    WorkflowInstance {
        id: "wf_guard".into(),
        definition_id: "demo".into(),
        definition_version: "1.0.0".into(),
        definition: json!({ "initialState": "s", "states": { "s": {} } }),
        state: "s".into(),
        version: 0,
        input: json!({}),
        context,
        started_at: chrono::Utc::now(),
        trace_id: None,
        run_id: None,
        cancelled_at: None,
        cancelled_reason: None,
        depth: 0,
        parent: None,
    }
}

fn principal() -> Principal {
    Principal {
        subject: "u1".into(),
        roles: vec!["editor".into()],
        permissions: vec!["publish".into()],
    }
}

async fn eval(guard: Value, context: Value) -> bool {
    DefaultGuardEvaluator::new()
        .evaluate(&guard, &instance(context), &json!({}), &principal())
        .await
        .expect("guard evaluates")
}

#[tokio::test]
async fn permission_guard_accepts_held_and_rejects_absent() {
    assert!(
        eval(
            json!({ "kind": "permission", "permission": "publish" }),
            json!({})
        )
        .await
    );
    assert!(
        !eval(
            json!({ "kind": "permission", "permission": "delete" }),
            json!({})
        )
        .await
    );
}

#[tokio::test]
async fn role_guard_accepts_held_and_rejects_absent() {
    assert!(eval(json!({ "kind": "role", "role": "editor" }), json!({})).await);
    assert!(!eval(json!({ "kind": "role", "role": "admin" }), json!({})).await);
}

#[tokio::test]
async fn expr_guard_accepts_and_rejects_on_context() {
    let ctx = json!({ "count": 1 });
    assert!(
        eval(
            json!({ "kind": "expr", "expr": "$.context.count == 1" }),
            ctx.clone()
        )
        .await
    );
    assert!(
        !eval(
            json!({ "kind": "expr", "expr": "$.context.count == 2" }),
            ctx
        )
        .await
    );
}

#[tokio::test]
async fn all_of_requires_every_clause() {
    let both_true = json!({
        "kind": "all_of",
        "guards": [
            { "kind": "role", "role": "editor" },
            { "kind": "permission", "permission": "publish" }
        ]
    });
    assert!(eval(both_true, json!({})).await);

    let one_false = json!({
        "kind": "all_of",
        "guards": [
            { "kind": "role", "role": "editor" },
            { "kind": "permission", "permission": "delete" }
        ]
    });
    assert!(!eval(one_false, json!({})).await);
}

#[tokio::test]
async fn any_of_requires_one_clause_and_empty_is_false() {
    let one_true = json!({
        "kind": "any_of",
        "guards": [
            { "kind": "role", "role": "admin" },
            { "kind": "role", "role": "editor" }
        ]
    });
    assert!(eval(one_true, json!({})).await);

    let none_true = json!({
        "kind": "any_of",
        "guards": [{ "kind": "role", "role": "admin" }]
    });
    assert!(!eval(none_true, json!({})).await);

    // Vacuous any_of → false (consistent with `requires` semantics).
    assert!(!eval(json!({ "kind": "any_of", "guards": [] }), json!({})).await);
}

#[tokio::test]
async fn not_inverts_inner() {
    assert!(
        !eval(
            json!({ "kind": "not", "guard": { "kind": "role", "role": "editor" } }),
            json!({})
        )
        .await
    );
    assert!(
        eval(
            json!({ "kind": "not", "guard": { "kind": "role", "role": "admin" } }),
            json!({})
        )
        .await
    );
}

#[tokio::test]
async fn deep_nested_composite_evaluates() {
    // all_of[ any_of[role:admin, role:editor], not[permission:delete] ] → true:
    // editor matches the any_of; the principal lacks `delete` so not[...] is true.
    let guard = json!({
        "kind": "all_of",
        "guards": [
            { "kind": "any_of", "guards": [
                { "kind": "role", "role": "admin" },
                { "kind": "role", "role": "editor" }
            ]},
            { "kind": "not", "guard": { "kind": "permission", "permission": "delete" } }
        ]
    });
    assert!(eval(guard, json!({})).await);

    // Flip the not's inner to a held permission → not[...] false → all_of false.
    let guard_false = json!({
        "kind": "all_of",
        "guards": [
            { "kind": "any_of", "guards": [{ "kind": "role", "role": "editor" }] },
            { "kind": "not", "guard": { "kind": "permission", "permission": "publish" } }
        ]
    });
    assert!(!eval(guard_false, json!({})).await);
}
