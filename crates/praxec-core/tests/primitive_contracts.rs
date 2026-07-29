//! Atomic behavioral-assertion suite for engine PRIMITIVES (A1-A6).
//!
//! Method: each test declares a DESIRED behavior and lets the runner say
//! red/green — this is not a derivation of what the code currently does.
//! A green test is a normal passing assertion. A red test (a desired
//! contract the engine does not yet satisfy) is authored with the real
//! assertion and marked `#[ignore = "RED: <gap>"]` so CI stays green while
//! the gap stays recorded; deleting the `#[ignore]` later is the fix-lands
//! signal.
//!
//! Harnesses below are copied from existing analogous suites (never
//! inferred from source reading):
//!   - A1/A6: the self-contained `NoopExec`/`NoopReg` + `WorkflowRuntime`
//!     harness from `tests/outcome_evaluation.rs`.
//!   - A4: `load_resolved_with_repos` + tempdir host file, from
//!     `tests/multi_repo_loading.rs`.
//!   - A5: `resolve_str` + `validate::validate_workflows`, from
//!     `tests/use_binding.rs` / `tests/validation_rules.rs`.

use std::sync::Arc;

use praxec_core::audit::{AuditSink, MemoryAuditSink};
use praxec_core::error::ExecutorError;
use praxec_core::guards::DefaultGuardEvaluator;
use praxec_core::model::{
    ExecuteRequest, ExecuteResult, Principal, StartWorkflow, SubmitTransition,
};
use praxec_core::ports::{Executor, ExecutorRegistry, WorkflowStore};
use praxec_core::runtime::WorkflowRuntime;
use praxec_core::store::{ConfigDefinitionStore, InMemoryEvidenceStore, InMemoryWorkflowStore};
use serde_json::{Value, json};

// ── shared no-op executor harness (mirrors outcome_evaluation.rs) ──────────

struct NoopExec;
#[async_trait::async_trait]
impl Executor for NoopExec {
    async fn execute(&self, _r: ExecuteRequest) -> Result<ExecuteResult, ExecutorError> {
        Ok(ExecuteResult::default())
    }
}
struct NoopReg;
impl ExecutorRegistry for NoopReg {
    fn get(&self, _kind: &str) -> Option<Arc<dyn Executor>> {
        Some(Arc::new(NoopExec))
    }
}

fn runtime_for(config: Value) -> WorkflowRuntime {
    let evidence = Arc::new(InMemoryEvidenceStore::new());
    WorkflowRuntime::new(
        Arc::new(ConfigDefinitionStore::from_config(&config)),
        Arc::new(InMemoryWorkflowStore::new()) as Arc<dyn WorkflowStore>,
        Arc::new(NoopReg),
        Arc::new(DefaultGuardEvaluator::with_evidence(evidence.clone())),
        Arc::new(MemoryAuditSink::new()) as Arc<dyn AuditSink>,
    )
    .with_writable_repo_roots(vec![praxec_core::RepoRoot::for_test()])
    .with_evidence(evidence)
}

async fn start_with_input(rt: &WorkflowRuntime, definition_id: &str, input: Value) -> Value {
    rt.start(StartWorkflow {
        definition_id: definition_id.into(),
        input,
        principal: Principal::anonymous(),
        run_env: praxec_core::RunEnv::for_test(),
        depth: 0,
        parent: None,
    })
    .await
    .expect("start succeeds")
}

async fn submit(rt: &WorkflowRuntime, id: &str, version: u64, transition: &str) -> Value {
    rt.submit(SubmitTransition {
        workflow_id: id.into(),
        expected_version: version,
        transition: transition.into(),
        arguments: json!({}),
        principal: Principal::anonymous(),
        summary: None,
        trace_id: None,
        run_id: None,
    })
    .await
    .expect("submit succeeds")
}

// ============================================================================
// A1 — Guarded enum routing: a guarded deterministic arm is taken when its
// guard passes; a different discriminant value falls through to the single
// UNGUARDED default arm instead (SPEC §9 deterministic-selection: V23).
// ============================================================================

/// Two deterministic candidates in state `route`: `to_ts` is guarded on
/// `$.context.mode == 'ts'`, `to_default` carries no guard (the default).
/// `mode` is seeded into context automatically from `input` at start
/// (runtime.rs input->context seeding), so the discriminant is set purely
/// by what the caller passes as `input.mode`.
fn switch_config() -> Value {
    json!({
        "workflows": { "switcher": {
            "initialState": "route",
            "states": {
                "route": {
                    "transitions": {
                        "to_ts": {
                            "target": "ts_target",
                            "actor": "deterministic",
                            "executor": { "kind": "noop" },
                            "guards": [ { "kind": "expr", "expr": "$.context.mode == 'ts'" } ]
                        },
                        "to_default": {
                            "target": "default_target",
                            "actor": "deterministic",
                            "executor": { "kind": "noop" }
                        }
                    }
                },
                "ts_target": { "terminal": true },
                "default_target": { "terminal": true }
            }
        }}
    })
}

#[tokio::test]
async fn guarded_arm_is_taken_when_its_guard_matches() {
    let rt = runtime_for(switch_config());
    let resp = start_with_input(&rt, "switcher", json!({ "mode": "ts" })).await;
    assert_eq!(
        resp["workflow"]["state"], "ts_target",
        "mode == 'ts' must route through the guarded arm, not the default; full resp: {resp:#}"
    );
}

#[tokio::test]
async fn unguarded_default_arm_is_taken_on_a_different_discriminant() {
    let rt = runtime_for(switch_config());
    let resp = start_with_input(&rt, "switcher", json!({ "mode": "rust" })).await;
    assert_eq!(
        resp["workflow"]["state"], "default_target",
        "mode == 'rust' does not match the guarded arm, so it must fall through to the \
         unguarded default; full resp: {resp:#}"
    );
}

// ============================================================================
// A4 — Unknown definitionId fails at LOAD, not deferred to runtime. This is
// V22 (`config::validate_workflow_refs_resolve`) — the check `praxec check`
// relies on via `load_resolved_with_repos`, the same loader used here.
//
// Empirically (this test, run without `#[ignore]` first): a HOST-ONLY
// config with no `repos:` block does NOT trip V22. `merge_declared_repos`
// (config.rs) returns `Ok(host)` immediately when `repos.is_empty()` —
// BEFORE ever reaching the `validate_workflow_refs_resolve(&merged)` call,
// which only sits on the repos-present branch further down the same
// function. `validate_workflows` (the separate `Vec<Diagnostic>` pass
// `praxec check` also runs) has no definitionId-existence check of its own
// either (`validate_use_bindings` / `validate_contract_hash_pins` both
// explicitly no-op on an unknown target, deferring to "V22's job"). So a
// single-file config's unknown `definitionId` sails through `praxec check`
// clean and is only ever discovered when that transition actually fires at
// runtime.
// ============================================================================

#[test]
fn a_kind_workflow_transition_referencing_an_unknown_definition_id_fails_at_load() {
    let td = tempfile::TempDir::new().unwrap();
    let host = r#"
version: "1.0.0"
workflows:
  flow.host:
    initialState: s
    states:
      s:
        transitions:
          go:
            target: done
            executor:
              kind: workflow
              definitionId: cap.does.not.exist
      done:
        terminal: true
"#;
    let path = td.path().join("praxec.yaml");
    std::fs::write(&path, host).unwrap();

    let err = praxec_core::config::load_resolved_with_repos(&path)
        .expect_err("an unresolved definitionId must fail the LOAD, not just warn/defer");
    let msg = format!("{err:#}");
    assert!(
        msg.contains("UNRESOLVED_WORKFLOW_REF"),
        "expected a V22 UNRESOLVED_WORKFLOW_REF load error, got: {msg}"
    );
}

// ============================================================================
// A5 — An unresolvable `$.`-path in a `skills:` entry must fail LOUD (load-time
// rejection or an explicit runtime error), never be silently treated as "no
// skills declared."
//
// Empirically (this test, run first without the loose OR-clause it now
// carries, to see the real diagnostic): `check_skills_refs` (validate.rs)
// treats every `skills:` array entry as a literal skill-subject string —
// there is no `$.`-path templating/resolution step for this field at all —
// so `"$.workflow.input.missing"` is checked against the top-level `skills:`
// library exactly like any other subject name, fails that membership check,
// and produces a genuinely LOUD load-time error naming the literal string:
// `references skills entry '$.workflow.input.missing' which is not declared
// in the top-level 'skills:' library (SPEC §11)`. So THIS contract already
// holds — GREEN — even though the diagnostic talks about an "undeclared
// skill" rather than an "unresolvable path" (there is no code path where a
// `$.`-shaped entry is silently dropped as long as it is a JSON *string*;
// see the doc-comment on `push_scope_subjects` / `check_scope` for the
// still-open non-string-entry gap this does NOT cover).
// ============================================================================

#[test]
fn unresolvable_dollar_path_skills_entry_fails_loud_at_load() {
    let yaml = r#"
version: "1.0.0"
workflows:
  flow.host:
    initialState: s
    states:
      s:
        skills: ["$.workflow.input.missing"]
        transitions:
          go:
            target: done
            executor: { kind: noop }
      done:
        terminal: true
"#;
    let config = praxec_core::config::resolve_str(yaml).expect("yaml resolves");
    let diags = praxec_core::validate::validate_workflows(&config);
    let messages: Vec<String> = diags.iter().map(|d| d.to_string()).collect();
    assert!(
        messages
            .iter()
            .any(|m| m.contains("$.workflow.input.missing")),
        "an unresolvable `$.`-path skills entry must fail loud at load, naming the \
         entry, rather than being silently treated as no skills declared; got: {messages:?}"
    );
}

// ============================================================================
// A5b — A `skills:` entry that is a NON-STRING JSON value (e.g. a bare
// number, or a nested array/object) must be rejected LOUDLY at load, naming
// the offending entry — never silently skipped as if no skill were declared
// there. Both `check_skills_refs` (validate.rs) and the runtime
// `push_scope_subjects` (runtime_links.rs) walk `skills:` arrays with an
// `entry.as_str()` guard that silently `continue`s past any entry that
// isn't a JSON string, so a non-string entry currently produces NO
// diagnostic at all (as if the scope were empty).
// ============================================================================

#[test]
fn non_string_skills_entry_is_rejected_loudly_at_load() {
    let yaml = r#"
version: "1.0.0"
workflows:
  flow.host:
    initialState: s
    states:
      s:
        skills: [42]
        transitions:
          go:
            target: done
            executor: { kind: noop }
      done:
        terminal: true
"#;
    let config = praxec_core::config::resolve_str(yaml).expect("yaml resolves");
    let diags = praxec_core::validate::validate_workflows(&config);
    let messages: Vec<String> = diags.iter().map(|d| d.to_string()).collect();
    assert!(
        messages.iter().any(|m| m.contains("42")),
        "a non-string `skills:` entry must be rejected loudly at load, naming the \
         offending entry, rather than being silently skipped; got: {messages:?}"
    );
}

// ============================================================================
// A6 — DoD `outcomes[].check` is executor-agnostic: it evaluates purely off
// the evidence shape in context, regardless of which executor populated it.
// Set `$.context.ws_verify` via a plain `kind: noop` deterministic step (NOT
// a specific verify capability) and confirm the outcome flips met on that
// shape alone.
// ============================================================================

fn dod_config() -> Value {
    json!({
        "workflows": { "dod": {
            "initialState": "work",
            "outcomes": [
                { "id": "verified", "statement": "verification evidence recorded",
                  "check": "$.context.ws_verify.status == 'pass'" }
            ],
            "blackboard": { "ws_verify": { "type": "object" } },
            "states": {
                "work": {
                    "transitions": {
                        "record_pass": {
                            "target": "work",
                            "executor": { "kind": "noop" },
                            "output": { "ws_verify": { "status": "pass" } }
                        },
                        "record_fail": {
                            "target": "work",
                            "executor": { "kind": "noop" },
                            "output": { "ws_verify": { "status": "fail" } }
                        }
                    }
                }
            }
        }}
    })
}

fn met(response: &Value, id: &str) -> Option<bool> {
    response["outcomes"]
        .as_array()?
        .iter()
        .find(|o| o["id"] == id)
        .and_then(|o| o["met"].as_bool())
}

#[tokio::test]
async fn dod_check_is_unmet_before_any_evidence_is_recorded() {
    let rt = runtime_for(dod_config());
    let r = start_with_input(&rt, "dod", json!({})).await;
    assert_eq!(met(&r, "verified"), Some(false));
}

#[tokio::test]
async fn dod_check_flips_met_once_a_plain_deterministic_step_writes_the_evidence_shape() {
    // The step that sets `ws_verify` is `kind: noop` — a generic deterministic
    // step, not a dedicated verify capability. The outcome must key on the
    // evidence SHAPE (`$.context.ws_verify.status == 'pass'`), never on which
    // executor/capability produced it.
    let rt = runtime_for(dod_config());
    let s = start_with_input(&rt, "dod", json!({})).await;
    let id = s["workflow"]["id"].as_str().unwrap().to_string();
    let v = s["workflow"]["version"].as_u64().unwrap();
    let r = submit(&rt, &id, v, "record_pass").await;
    assert_eq!(met(&r, "verified"), Some(true));
}

#[tokio::test]
async fn dod_check_reads_not_met_when_the_recorded_status_is_fail() {
    let rt = runtime_for(dod_config());
    let s = start_with_input(&rt, "dod", json!({})).await;
    let id = s["workflow"]["id"].as_str().unwrap().to_string();
    let v = s["workflow"]["version"].as_u64().unwrap();
    let r = submit(&rt, &id, v, "record_fail").await;
    assert_eq!(met(&r, "verified"), Some(false));
}
