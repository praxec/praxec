//! The default registry builder must actually wire every kind it advertises
//! in `REGISTERED_EXECUTOR_KINDS` — including the authoring executors added by
//! `with_authoring_executors`. This is the poka-yoke that keeps the advertised
//! set, the `check` oracle, and the runtime registry in lockstep: if a kind is
//! in the constant but not wired, `check` would pass and runtime would fail.

use praxec_executors::{default_registry, REGISTERED_EXECUTOR_KINDS};
use serde_json::json;

/// Kinds advertised in `REGISTERED_EXECUTOR_KINDS` but wired OUTSIDE the
/// base `default_registry` builder:
/// - `llm` is added by the binary as an overlay (`llm-executor`).
/// - `agent` is added by the binary as an overlay.
///
/// `workflow` is NO LONGER here: C1 wired it into the base registry runtime-less
/// (`default_registry_with_late_workflow` registers it; the binary late-binds
/// the runtime via `WorkflowExecutor::set_runtime`). It must therefore RESOLVE
/// from the base registry — that resolution is the C1 regression guard.
const WIRED_ELSEWHERE: &[&str] = &["llm", "agent"];

#[test]
fn default_registry_resolves_every_base_registered_kind() {
    let registry = default_registry(&json!({}));
    for kind in REGISTERED_EXECUTOR_KINDS {
        if WIRED_ELSEWHERE.contains(kind) {
            continue;
        }
        // The base runtime set, the authoring executors (dry_run,
        // structural_analysis, ingest, registry), AND `workflow` (runtime-less,
        // late-bound by the binary) must all resolve here.
        assert!(
            registry.get(kind).is_some(),
            "REGISTERED_EXECUTOR_KINDS advertises `{kind}` but the default registry does not wire it"
        );
    }
}

/// C1 regression: `kind: workflow` passes `praxec check` (it is in
/// REGISTERED_EXECUTOR_KINDS) and MUST also be present in the production
/// registry shape, so a config using it no longer validates-clean-then-dies. The
/// executor is registered runtime-less; `WorkflowExecutor::set_runtime` injects
/// the runtime at boot.
#[test]
fn workflow_kind_is_wired_into_the_default_registry() {
    let registry = default_registry(&json!({}));
    assert!(
        registry.get("workflow").is_some(),
        "C1: `workflow` is advertised in REGISTERED_EXECUTOR_KINDS and must resolve from the \
         production registry shape (runtime-less; late-bound via set_runtime)"
    );
}

#[test]
fn authoring_kinds_are_wired_into_the_default_registry() {
    // Regression guard for the lenient-oracle fix: these used to pass `check`
    // (in ALL_EXECUTOR_KINDS) but fail at runtime (unregistered).
    let registry = default_registry(&json!({}));
    for kind in ["dry_run", "structural_analysis", "ingest", "registry"] {
        assert!(
            registry.get(kind).is_some(),
            "authoring kind `{kind}` must be wired"
        );
    }
}

#[test]
fn default_registry_does_not_resolve_an_unknown_kind() {
    let registry = default_registry(&json!({}));
    assert!(registry.get("definitely-not-a-kind").is_none());
}
