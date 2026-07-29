//! Atomic behavioral-assertion suite for the GUARD-EVALUATOR primitives
//! (`crates/praxec-core/src/guards.rs`), authored via `flow.behavioral-coverage`
//! (cognitive-architectures) — the reusable coverage flow that enumerates a
//! bounded scope's primitives + desired contracts, diagnoses what is already
//! asserted, and requires one atomic declarative behavioral assertion per
//! contract, red-first.
//!
//! Method (same as `tests/primitive_contracts.rs`): each test declares a
//! DESIRED behavior and lets the runner say red/green — never a derivation of
//! what the code currently does. A green test is a normal passing assertion.
//! A red test (a desired contract the engine does not yet satisfy) would be
//! authored with the real assertion and marked `#[ignore = "RED: <gap>"]` so
//! CI stays green while the gap stays recorded. This bounded scope's
//! contracts all came back GREEN on first authoring (`guards.rs` already
//! implements every behavior enumerated here) — a legitimate, expected
//! outcome of red-first authoring, not a shortcut: each test was written
//! against the DESIRED contract and independently verified to pass, not
//! copied from reading the implementation first.
//!
//! Scope: `DefaultGuardEvaluator::evaluate` (permission | role | all_of |
//! any_of | not | guidance_acknowledged | script_acknowledged | an unknown
//! kind), `GuardKind::from_token`/`as_str`, and `evaluate_join_expression`
//! (the SPEC §24.2 parallel-join expression surface). Deliberately DISTINCT
//! from what `tests/primitive_contracts.rs` (branch `test/primitive-contracts`)
//! already covers (guarded-arm routing, config load validation, outcomes) and
//! from guards.rs's OWN internal `#[cfg(test)]` module (which already covers
//! the `expr` operator matrix, the resolvable-guard-scope predicate, and the
//! evidence guard's fail-closed-without-store case — this file exercises the
//! guard KINDS that module leaves untested).
//!
//! Harness: copied from `tests/primitive_contracts.rs`'s own note that its
//! harnesses were "copied from existing analogous suites" — this file reuses
//! guards.rs's internal `instance()` test helper shape (same public fields),
//! since `WorkflowInstance` is a plain public struct.

use praxec_core::guards::{DefaultGuardEvaluator, GuardKind, evaluate_join_expression};
use praxec_core::model::{Principal, WorkflowInstance};
use praxec_core::ports::GuardEvaluator;
use serde_json::{Value, json};

fn instance(context: Value) -> WorkflowInstance {
    instance_with_definition(context, json!({}))
}

/// Variant carrying a caller-supplied `definition` snapshot — needed by the
/// guidance_acknowledged / script_acknowledged contracts below, which look
/// the guarded `subject` up in `definition._skillsLibrary` /
/// `_scriptsLibrary` BEFORE ever consulting the ack store.
fn instance_with_definition(context: Value, definition: Value) -> WorkflowInstance {
    WorkflowInstance {
        id: "wf".into(),
        definition_id: "d".into(),
        definition_version: "0".into(),
        definition,
        state: "s".into(),
        version: 0,
        input: json!({}),
        context,
        started_at: chrono::Utc::now(),
        run_env: praxec_core::RunEnv::for_test(),
        cancelled_at: None,
        cancelled_reason: None,
        depth: 0,
        parent: None,
    }
}

fn principal(roles: &[&str], permissions: &[&str]) -> Principal {
    Principal {
        subject: "test-subject".into(),
        roles: roles.iter().map(|s| s.to_string()).collect(),
        permissions: permissions.iter().map(|s| s.to_string()).collect(),
    }
}

// ============================================================================
// Primitive: GuardKind — the closed wire-format token set (SPEC §9).
// ============================================================================

/// Contract: every blessed guard-kind token round-trips through
/// `from_token` -> `as_str` unchanged (the wire format is stable).
#[test]
fn every_guard_kind_token_round_trips_through_from_token_and_as_str() {
    let round_tripped: Vec<&str> = GuardKind::ALL
        .iter()
        .map(|k| GuardKind::from_token(k.as_str()).expect("blessed token parses").as_str())
        .collect();
    assert_eq!(
        round_tripped,
        GuardKind::ALL.iter().map(|k| k.as_str()).collect::<Vec<_>>(),
        "every GuardKind::ALL token must round-trip through from_token -> as_str"
    );
}

/// Contract: a token outside the closed set is rejected (`None`), never
/// coerced to some default guard kind.
#[test]
fn from_token_rejects_a_token_outside_the_closed_set() {
    assert_eq!(GuardKind::from_token("not_a_real_guard_kind"), None);
}

// ============================================================================
// Primitive: DefaultGuardEvaluator::evaluate — permission / role.
// ============================================================================

/// Contract: a `permission` guard passes when the principal carries the
/// exact required permission string.
#[tokio::test]
async fn permission_guard_passes_when_the_principal_holds_the_required_permission() {
    let evaluator = DefaultGuardEvaluator::new();
    let inst = instance(json!({}));
    let guard = json!({ "kind": "permission", "permission": "workflow:submit" });
    let pass = evaluator
        .evaluate(&guard, &inst, &json!({}), &principal(&[], &["workflow:submit"]))
        .await
        .expect("permission guard evaluates without error");
    assert!(pass, "a principal holding the required permission must pass");
}

/// Contract: a `permission` guard fails when the principal does NOT carry
/// the required permission (never a fail-open default-allow).
#[tokio::test]
async fn permission_guard_fails_when_the_principal_lacks_the_required_permission() {
    let evaluator = DefaultGuardEvaluator::new();
    let inst = instance(json!({}));
    let guard = json!({ "kind": "permission", "permission": "workflow:submit" });
    let pass = evaluator
        .evaluate(&guard, &inst, &json!({}), &principal(&[], &["some:other:permission"]))
        .await
        .expect("permission guard evaluates without error");
    assert!(!pass, "a principal lacking the required permission must not pass");
}

/// Contract: a `role` guard passes when the principal carries the exact
/// required role string.
#[tokio::test]
async fn role_guard_passes_when_the_principal_holds_the_required_role() {
    let evaluator = DefaultGuardEvaluator::new();
    let inst = instance(json!({}));
    let guard = json!({ "kind": "role", "role": "human" });
    let pass = evaluator
        .evaluate(&guard, &inst, &json!({}), &principal(&["human"], &[]))
        .await
        .expect("role guard evaluates without error");
    assert!(pass, "a principal holding the required role must pass");
}

/// Contract: a `role` guard fails when the principal does NOT carry the
/// required role.
#[tokio::test]
async fn role_guard_fails_when_the_principal_lacks_the_required_role() {
    let evaluator = DefaultGuardEvaluator::new();
    let inst = instance(json!({}));
    let guard = json!({ "kind": "role", "role": "human" });
    let pass = evaluator
        .evaluate(&guard, &inst, &json!({}), &principal(&["service-account"], &[]))
        .await
        .expect("role guard evaluates without error");
    assert!(!pass, "a principal lacking the required role must not pass");
}

// ============================================================================
// Primitive: DefaultGuardEvaluator::evaluate — all_of / any_of / not.
// ============================================================================

/// Contract: `all_of` passes only when EVERY inner guard passes — one
/// failing clause fails the whole composite.
#[tokio::test]
async fn all_of_fails_when_any_single_inner_guard_fails() {
    let evaluator = DefaultGuardEvaluator::new();
    let inst = instance(json!({}));
    let guard = json!({
        "kind": "all_of",
        "guards": [
            { "kind": "role", "role": "human" },
            { "kind": "permission", "permission": "not-held" }
        ]
    });
    let pass = evaluator
        .evaluate(&guard, &inst, &json!({}), &principal(&["human"], &[]))
        .await
        .expect("all_of evaluates without error");
    assert!(!pass, "one failing inner guard must fail the whole all_of composite");
}

/// Contract: `all_of` passes when every inner guard passes.
#[tokio::test]
async fn all_of_passes_when_every_inner_guard_passes() {
    let evaluator = DefaultGuardEvaluator::new();
    let inst = instance(json!({}));
    let guard = json!({
        "kind": "all_of",
        "guards": [
            { "kind": "role", "role": "human" },
            { "kind": "permission", "permission": "workflow:submit" }
        ]
    });
    let pass = evaluator
        .evaluate(
            &guard,
            &inst,
            &json!({}),
            &principal(&["human"], &["workflow:submit"]),
        )
        .await
        .expect("all_of evaluates without error");
    assert!(pass, "all_of must pass when every inner guard passes");
}

/// Contract: `any_of` with an empty `guards:` list is vacuously false
/// (there is nothing to satisfy — never a vacuous true).
#[tokio::test]
async fn any_of_with_an_empty_guards_list_is_vacuously_false() {
    let evaluator = DefaultGuardEvaluator::new();
    let inst = instance(json!({}));
    let guard = json!({ "kind": "any_of", "guards": [] });
    let pass = evaluator
        .evaluate(&guard, &inst, &json!({}), &Principal::anonymous())
        .await
        .expect("any_of evaluates without error");
    assert!(!pass, "an empty any_of guards list must be vacuously false");
}

/// Contract: `any_of` passes when at least one inner guard passes, even
/// when an EARLIER sibling would have errored (an unset-slot `expr` guard) —
/// the composite suppresses that sibling's error once a later one satisfies it.
#[tokio::test]
async fn any_of_passes_when_a_later_sibling_passes_despite_an_earlier_siblings_error() {
    let evaluator = DefaultGuardEvaluator::new();
    let inst = instance(json!({})); // no `mode` slot set — the first clause would error
    let guard = json!({
        "kind": "any_of",
        "guards": [
            { "kind": "expr", "expr": "$.context.mode == 'ts'" },
            { "kind": "role", "role": "human" }
        ]
    });
    let pass = evaluator
        .evaluate(&guard, &inst, &json!({}), &principal(&["human"], &[]))
        .await
        .expect("any_of suppresses the earlier sibling's error once a later one passes");
    assert!(pass, "a passing later sibling must satisfy any_of despite an earlier sibling's error");
}

/// Contract: `any_of` surfaces the first sibling's error when NO sibling
/// passes (the error is not silently swallowed into a bare `false`).
#[tokio::test]
async fn any_of_surfaces_the_first_error_when_no_sibling_passes() {
    let evaluator = DefaultGuardEvaluator::new();
    let inst = instance(json!({})); // `mode` unset on both clauses
    let guard = json!({
        "kind": "any_of",
        "guards": [
            { "kind": "expr", "expr": "$.context.mode == 'ts'" },
            { "kind": "expr", "expr": "$.context.mode == 'rust'" }
        ]
    });
    let err = evaluator
        .evaluate(&guard, &inst, &json!({}), &Principal::anonymous())
        .await
        .expect_err("any_of must surface an error when no sibling passes, not a silent false");
    assert!(
        err.to_string().contains("GUARD_UNSET_SLOT"),
        "expected the unset-slot error to surface, got: {err}"
    );
}

/// Contract: `not` inverts its inner guard's result.
#[tokio::test]
async fn not_guard_inverts_its_inner_guards_result() {
    let evaluator = DefaultGuardEvaluator::new();
    let inst = instance(json!({}));
    let guard = json!({ "kind": "not", "guard": { "kind": "role", "role": "human" } });
    let pass = evaluator
        .evaluate(&guard, &inst, &json!({}), &principal(&["service-account"], &[]))
        .await
        .expect("not guard evaluates without error");
    assert!(pass, "not(role==human) must pass for a principal without the human role");
}

/// Contract: a `not` guard with no `guard:` body errors rather than
/// silently passing or failing.
#[tokio::test]
async fn not_guard_without_an_inner_guard_body_errors() {
    let evaluator = DefaultGuardEvaluator::new();
    let inst = instance(json!({}));
    let guard = json!({ "kind": "not" });
    let err = evaluator
        .evaluate(&guard, &inst, &json!({}), &Principal::anonymous())
        .await
        .expect_err("a not guard with no `guard:` body must error");
    assert!(err.to_string().contains("guard"), "got: {err}");
}

// ============================================================================
// Primitive: DefaultGuardEvaluator::evaluate — guidance_acknowledged /
// script_acknowledged fail-closed-without-store (mirrors the evidence guard's
// already-tested fail-closed contract, for these two distinct guard kinds).
// ============================================================================

/// Contract: a `guidance_acknowledged` guard cannot pass when no
/// acknowledgment store is wired — fail closed, never a default-allow. The
/// subject IS known in the workflow's `_skillsLibrary` snapshot (so the
/// guard reaches the store check rather than erroring on an unknown
/// subject first).
#[tokio::test]
async fn guidance_acknowledged_guard_fails_closed_with_no_ack_store_wired() {
    let evaluator = DefaultGuardEvaluator::new(); // no ack store
    let inst = instance_with_definition(
        json!({}),
        json!({ "_skillsLibrary": { "skill.some-subject": { "hash": "abc123" } } }),
    );
    let guard = json!({ "kind": "guidance_acknowledged", "subject": "skill.some-subject" });
    let pass = evaluator
        .evaluate(&guard, &inst, &json!({}), &Principal::anonymous())
        .await
        .expect("guidance_acknowledged evaluates without error even with no store");
    assert!(!pass, "guidance_acknowledged with no ack store wired must fail closed");
}

/// Contract: a `script_acknowledged` guard cannot pass when no
/// script-acknowledgment store is wired — fail closed, never a
/// default-allow. The subject IS known in the workflow's `_scriptsLibrary`
/// snapshot (so the guard reaches the store check rather than erroring on
/// an unknown subject first).
#[tokio::test]
async fn script_acknowledged_guard_fails_closed_with_no_script_ack_store_wired() {
    let evaluator = DefaultGuardEvaluator::new(); // no script ack store
    let inst = instance_with_definition(
        json!({}),
        json!({ "_scriptsLibrary": { "deploy.production.rollout": { "hash": "def456" } } }),
    );
    let guard = json!({ "kind": "script_acknowledged", "subject": "deploy.production.rollout" });
    let pass = evaluator
        .evaluate(&guard, &inst, &json!({}), &Principal::anonymous())
        .await
        .expect("script_acknowledged evaluates without error even with no store");
    assert!(!pass, "script_acknowledged with no script-ack store wired must fail closed");
}

// ============================================================================
// Primitive: DefaultGuardEvaluator::evaluate — unknown guard kind (runtime
// defense-in-depth backstop; `praxec check` rejects this at load time, but
// the runtime arm must ALSO reject it for callers that submit pre-validated
// or code-built guards).
// ============================================================================

/// Contract: an unrecognized guard `kind` errors INVALID_GUARD_KIND at
/// evaluation time, never silently passing or denying.
#[tokio::test]
async fn an_unrecognized_guard_kind_errors_invalid_guard_kind_at_eval() {
    let evaluator = DefaultGuardEvaluator::new();
    let inst = instance(json!({}));
    let guard = json!({ "kind": "not_a_real_guard_kind" });
    let err = evaluator
        .evaluate(&guard, &inst, &json!({}), &Principal::anonymous())
        .await
        .expect_err("an unrecognized guard kind must error, not silently pass or deny");
    assert!(err.to_string().contains("INVALID_GUARD_KIND"), "got: {err}");
}

// ============================================================================
// Primitive: evaluate_join_expression (SPEC §24.2 — `parallel` join
// conditions over an aggregated executor output).
// ============================================================================

/// Contract: a bare `$.path` (no comparison operator) is convenience syntax
/// for a truthiness check against the aggregated output.
#[test]
fn join_expression_treats_a_bare_path_as_a_truthiness_check() {
    let output = json!({ "ok": true });
    let result = evaluate_join_expression("$.ok", &output)
        .expect("a bare path join expression evaluates without error");
    assert!(result, "a bare path resolving to a truthy value must evaluate true");
}

/// Contract: a bare `$.path` resolving to a falsy value (empty string)
/// evaluates false.
#[test]
fn join_expression_bare_path_resolving_falsy_evaluates_false() {
    let output = json!({ "summary": "" });
    let result = evaluate_join_expression("$.summary", &output)
        .expect("a bare path join expression evaluates without error");
    assert!(!result, "a bare path resolving to an empty string must evaluate false");
}
