//! Load-time validation that every executor `kind` in the workflow
//! registry is one the runtime can actually dispatch (STUB-103).
//!
//! Before this, a typo'd or unsupported executor kind (`kind: lmm`,
//! `kind: scrpt`, or a kind no build registered) passed `praxec check`
//! cleanly and only failed when that transition FIRST fired at runtime —
//! with the workflow already mid-flight. This walks the registry the same
//! way [`crate::structural`-adjacent] `cost::doctor_check` /
//! `llm config_doctor::doctor_check` do, validating each executor block's
//! `kind` against [`crate::REGISTERED_EXECUTOR_KINDS`], and is wired into
//! the binary's `check` subcommand alongside them.
//!
//! Scope mirrors the sibling doctors: top-level executors at `onEnter` and
//! on each transition. Nested executors inside `parallel.branches` /
//! `pipeline.steps` dispatch through the same registry at runtime, which
//! rejects an unknown nested kind with an equally explicit error; the
//! parallel `aggregator` deliberately carries its own non-executor
//! `kind: expression`, so it is intentionally not walked here.

use praxec_core::validate::Diagnostic;
use serde_json::Value;

use crate::ALL_EXECUTOR_KINDS;

/// Walk every transition / `onEnter` executor in the workflow registry and
/// emit `UNKNOWN_EXECUTOR_KIND` for any block whose `kind` the runtime
/// can't dispatch.
pub fn doctor_check(workflow_registry: &Value) -> Vec<Diagnostic> {
    let mut out = Vec::new();

    let Some(workflows) = workflow_registry
        .pointer("/workflows")
        .and_then(Value::as_object)
    else {
        return out;
    };

    // CMP-046 — share core's single executor-site walker (which also visits
    // onEnter executors, CMP-003) rather than re-implementing the traversal.
    for (wf_id, wf_def) in workflows {
        praxec_core::validate::for_each_executor_site(wf_def, |site| {
            check_executor(wf_id, &site.location, site.executor, &mut out);
        });
    }

    out
}

fn check_executor(wf_id: &str, location: &str, executor: &Value, out: &mut Vec<Diagnostic>) {
    // Only validate a `kind` that is actually present. A missing executor
    // kind is a separate structural error owned by core's validator; flagging
    // it here too would double-report.
    let Some(kind) = executor.get("kind").and_then(Value::as_str) else {
        return;
    };
    if ALL_EXECUTOR_KINDS.contains(&kind) {
        // The `kind: workflow` executor no longer polls a child to terminal:
        // a non-terminal child suspends durably (the runtime owns liveness).
        // `timeoutMs`/`noProgressTimeoutMs` only ever bounded that poll, so
        // they are now silently ignored. Flag them at load time rather than
        // let an author copy a stale example and wonder why the bound does
        // nothing. A warning (not an error) — the workflow still runs.
        if kind == "workflow" {
            for retired in ["timeoutMs", "noProgressTimeoutMs"] {
                if executor.get(retired).is_some() {
                    out.push(Diagnostic::Warning(format!(
                        "RETIRED_WORKFLOW_TIMEOUT: workflow '{wf_id}' {location} sets \
                         `{retired}` on a `kind: workflow` executor, which is no longer read \
                         — the executor suspends on a non-terminal child instead of polling \
                         it to terminal, so there is no poll for this knob to bound. Remove it; \
                         a child mission's own definition-level timeout still applies."
                    )));
                }
            }
        }
        return;
    }
    // `kind: skill` is a common authoring mistake: a skill is not a standalone
    // executor, it is the instruction layer of a `kind: llm` agent (SPEC
    // §33.12). Steer the author to the right shape instead of the generic
    // "unknown kind" list.
    if kind == "skill" {
        out.push(Diagnostic::Error(format!(
            "EXECUTOR_KIND_SKILL: workflow '{wf_id}' {location} declares `kind: skill`, but a skill \
             is not an executor — it is the instruction set of a `kind: llm` agent. Declare \
             `skills: [<subject>]` at the workflow/state/transition scope of a `kind: llm` step \
             instead; the skill body is injected as that agent's system message (SPEC §33.12)."
        )));
        return;
    }
    out.push(Diagnostic::Error(format!(
        "UNKNOWN_EXECUTOR_KIND: workflow '{wf_id}' {location} declares `kind: {kind}`, which no \
         executor handles. Known kinds: {known}. (Previously this passed `check` and only failed \
         when the transition first fired at runtime.)",
        known = ALL_EXECUTOR_KINDS.join(", ")
    )));
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn registry_with_executor(executor: Value) -> Value {
        json!({
            "workflows": {
                "wf_under_test": {
                    "states": {
                        "ready": {
                            "transitions": {
                                "go": { "target": "done", "executor": executor }
                            }
                        }
                    }
                }
            }
        })
    }

    #[test]
    fn known_kind_yields_no_diagnostics() {
        for kind in ALL_EXECUTOR_KINDS {
            let reg = registry_with_executor(json!({ "kind": kind }));
            assert!(
                doctor_check(&reg).is_empty(),
                "known kind `{kind}` must pass"
            );
        }
    }

    /// Poka-yoke — the default runtime set must always be a subset of the
    /// full set the doctor validates against, so a kind that ships in the
    /// gateway can never be reported as unknown.
    #[test]
    fn registered_kinds_are_a_subset_of_all_kinds() {
        for kind in crate::REGISTERED_EXECUTOR_KINDS {
            assert!(
                ALL_EXECUTOR_KINDS.contains(kind),
                "REGISTERED kind `{kind}` missing from ALL_EXECUTOR_KINDS"
            );
        }
    }

    /// The authoring-time executors that ship as standalone structs must be
    /// recognized, or `praxec check` would reject the shipped
    /// authoring-workflow example.
    #[test]
    fn authoring_time_kinds_are_recognized() {
        for kind in ["registry", "dry_run", "structural_analysis", "ingest"] {
            let reg = registry_with_executor(json!({ "kind": kind }));
            assert!(
                doctor_check(&reg).is_empty(),
                "authoring kind `{kind}` must pass"
            );
        }
    }

    #[test]
    fn unknown_kind_is_an_error() {
        let reg = registry_with_executor(json!({ "kind": "lmm" }));
        let diags = doctor_check(&reg);
        assert_eq!(diags.len(), 1);
        match &diags[0] {
            Diagnostic::Error(msg) => {
                assert!(msg.contains("UNKNOWN_EXECUTOR_KIND"), "got: {msg}");
                assert!(msg.contains("lmm"));
                assert!(msg.contains("wf_under_test"));
            }
            other => panic!("expected Error, got {other:?}"),
        }
    }

    #[test]
    fn kind_skill_gets_a_tailored_steering_message() {
        // `kind: skill` is not an executor (SPEC §33.12) — the author should
        // declare `skills:` at scope on a `kind: llm` step. The doctor must
        // say so rather than emit the generic unknown-kind list.
        let reg = registry_with_executor(json!({ "kind": "skill", "subject": "review.x" }));
        let diags = doctor_check(&reg);
        assert_eq!(diags.len(), 1);
        match &diags[0] {
            Diagnostic::Error(msg) => {
                assert!(msg.contains("EXECUTOR_KIND_SKILL"), "got: {msg}");
                assert!(msg.contains("kind: llm"), "must steer to llm: {msg}");
                assert!(msg.contains("skills:"), "must mention scoped skills: {msg}");
                assert!(
                    !msg.contains("UNKNOWN_EXECUTOR_KIND"),
                    "must not also emit the generic message: {msg}"
                );
            }
            other => panic!("expected Error, got {other:?}"),
        }
    }

    #[test]
    fn retired_workflow_timeout_knobs_warn_not_error() {
        // The `kind: workflow` executor no longer polls, so `timeoutMs` /
        // `noProgressTimeoutMs` are silently ignored. The doctor warns (the
        // workflow still runs) rather than failing the check.
        for retired in ["timeoutMs", "noProgressTimeoutMs"] {
            let reg = registry_with_executor(json!({
                "kind": "workflow",
                "definitionId": "child",
                retired: 60000,
            }));
            let diags = doctor_check(&reg);
            assert_eq!(
                diags.len(),
                1,
                "expected exactly one diagnostic for {retired}"
            );
            match &diags[0] {
                Diagnostic::Warning(msg) => {
                    assert!(msg.contains("RETIRED_WORKFLOW_TIMEOUT"), "got: {msg}");
                    assert!(msg.contains(retired), "must name the knob: {msg}");
                }
                other => panic!("expected Warning, got {other:?}"),
            }
        }
    }

    #[test]
    fn clean_workflow_executor_yields_no_diagnostics() {
        // A `kind: workflow` step without the retired knobs is fine.
        let reg = registry_with_executor(json!({
            "kind": "workflow",
            "definitionId": "child",
        }));
        assert!(doctor_check(&reg).is_empty());
    }

    #[test]
    fn onenter_executor_is_also_checked() {
        let reg = json!({
            "workflows": {
                "wf": {
                    "states": {
                        "s": { "onEnter": { "executor": { "kind": "bogus" } } }
                    }
                }
            }
        });
        let diags = doctor_check(&reg);
        assert_eq!(diags.len(), 1);
        assert!(matches!(&diags[0], Diagnostic::Error(m) if m.contains("onEnter")));
    }

    #[test]
    fn missing_kind_is_not_reported_here() {
        // Owned by core's structural validator — don't double-report.
        let reg = registry_with_executor(json!({ "subject": "x" }));
        assert!(doctor_check(&reg).is_empty());
    }

    #[test]
    fn guard_kinds_are_not_touched() {
        // Guards share the `kind` key but live outside executor positions;
        // a guard `kind: expr` must never be flagged as an executor kind.
        let reg = json!({
            "workflows": {
                "wf": {
                    "states": {
                        "s": {
                            "transitions": {
                                "go": {
                                    "target": "done",
                                    "guard": { "kind": "expr", "expr": "1 == 1" },
                                    "executor": { "kind": "noop" }
                                }
                            }
                        }
                    }
                }
            }
        });
        assert!(doctor_check(&reg).is_empty());
    }
}
