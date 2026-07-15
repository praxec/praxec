//! Enforcement + resolution tests for the `hop_slot:` primitive (Spec A / A.1).
//!
//! STRICT model: a `hop_slot: <slot>` transition declares NO executor. At config
//! load the engine (`inject_hop_slots`):
//!   - injects the canonical `<slot>In` as the transition `inputSchema` and
//!     `<slot>Out` as the `$.context.<slot>` typed blackboard slot;
//!   - RESOLVES the marker to a concrete `cap.<slot>.<stack>` (stack from the
//!     workflow's `stack:` field; `generic` fallback; repo-priority tie-break)
//!     and wires it as a `kind: workflow` executor with a `use:` block.
//!
//! Enforcement is then pure reuse of the existing seams: `validate_schema`
//! (input) and `validate_blackboard_writes` (output). These tests prove the
//! contract is UNBYPASSABLE end-to-end — a malformed `verifyOut` produced by the
//! resolved cap is rejected before the transition advances — AND that resolution
//! picks the right cap.

use std::path::PathBuf;
use std::sync::Arc;

use async_trait::async_trait;
use praxec_core::WorkflowRuntime;
use praxec_core::audit::{AuditSink, MemoryAuditSink};
use praxec_core::config::{load_resolved_with_repos, resolve};
use praxec_core::guards::DefaultGuardEvaluator;
use praxec_core::model::{Principal, StartWorkflow, SubmitTransition};
use praxec_core::ports::{Executor, ExecutorRegistry};
use praxec_core::store::{ConfigDefinitionStore, InMemoryWorkflowStore};
use serde_json::{Map, Value, json};
use tempfile::TempDir;

// ── harness ───────────────────────────────────────────────────────────────────

/// Executor substituted for `kind: workflow` so a test controls the value the
/// resolved cap "produces" — the parent projects it into `$.context.<slot>`,
/// where `validate_blackboard_writes` enforces `<slot>Out`. (This bypasses real
/// sub-workflow launch; the enforcement seam under test is identical.)
struct FixedOutputExecutor {
    output: Value,
}

#[async_trait]
impl Executor for FixedOutputExecutor {
    async fn execute(
        &self,
        _: praxec_core::model::ExecuteRequest,
    ) -> Result<praxec_core::model::ExecuteResult, praxec_core::error::ExecutorError> {
        Ok(praxec_core::model::ExecuteResult {
            output: self.output.clone(),
            evidence: vec![],
            child_workflow_id: None,
            next_transition: None,
            suspend: None,
            telemetry: None,
        })
    }
}

struct SingleExecRegistry {
    inner: Arc<dyn Executor>,
}

impl ExecutorRegistry for SingleExecRegistry {
    fn get(&self, _kind: &str) -> Option<Arc<dyn Executor>> {
        Some(self.inner.clone())
    }
}

fn build_runtime(config: Value, executor_output: Value) -> WorkflowRuntime {
    let definitions = Arc::new(ConfigDefinitionStore::from_config(&config));
    let store = Arc::new(InMemoryWorkflowStore::new());
    let executors = Arc::new(SingleExecRegistry {
        inner: Arc::new(FixedOutputExecutor {
            output: executor_output,
        }),
    });
    let guards = Arc::new(DefaultGuardEvaluator::new());
    let audit = Arc::new(MemoryAuditSink::new());
    WorkflowRuntime::new(
        definitions,
        store,
        executors,
        guards,
        audit as Arc<dyn AuditSink>,
    )
    .with_writable_repo_roots(vec![praxec_core::RepoRoot::for_test()])
}

/// A minimal `cap.verify.<stack>` workflow: a terminal state + the HOP-typed
/// `snippet.outputs.verify` the resolver maps into `$.context.verify`.
fn verify_cap() -> Value {
    json!({
        "initialState": "ready",
        "states": { "ready": { "terminal": true } },
        "snippet": {
            "inputs": { "cwd": { "type": "string" } },
            "outputs": { "verify": { "$ref": "praxec://hop#/$defs/verifyOut" } }
        }
    })
}

/// Host config: a `stack`-typed flow with one `hop_slot: verify` transition
/// (NO executor — the engine resolves + wires the cap), plus the named caps.
fn sdlc_config(stack: Option<&str>, caps: &[(&str, Value)]) -> Value {
    let mut sdlc = json!({
        "initialState": "gate",
        "states": {
            "gate": { "transitions": { "run": {
                "target": "done",
                "actor": "agent",
                "hop_slot": "verify"
            } } },
            "done": { "terminal": true }
        }
    });
    if let Some(s) = stack {
        sdlc.as_object_mut()
            .unwrap()
            .insert("stack".into(), json!(s));
    }
    let mut workflows = Map::new();
    workflows.insert("sdlc".into(), sdlc);
    for (id, def) in caps {
        workflows.insert((*id).to_string(), def.clone());
    }
    json!({ "version": "1.0.0", "workflows": Value::Object(workflows) })
}

/// The resolved `hop_slot: verify` transition after `resolve`.
fn run_transition(resolved: &Value) -> Value {
    resolved
        .pointer("/workflows/sdlc/states/gate/transitions/run")
        .cloned()
        .expect("run transition present")
}

fn valid_verify_out() -> Value {
    json!({
        "status": "pass",
        "summary": "criteria met",
        "criteria": [{ "id": "c1", "met": true, "evidence": "green" }],
        "findings": [],
        "provenance": { "stack": "language:rust", "source": "pack" }
    })
}

async fn start_and_submit(runtime: &WorkflowRuntime, valid_input: bool) -> Value {
    let start = runtime
        .start(StartWorkflow {
            definition_id: "sdlc".into(),
            input: json!({}),
            principal: Principal::anonymous(),
            run_env: praxec_core::RunEnv::for_test(),
            depth: 0,
            parent: None,
        })
        .await
        .unwrap();
    let workflow_id = start["workflow"]["id"].as_str().unwrap().to_string();
    let version = start["workflow"]["version"].as_u64().unwrap();
    // `arguments` must satisfy the injected `verifyIn` (requires `cwd`).
    let arguments = if valid_input {
        json!({ "cwd": "." })
    } else {
        json!({ "not_a_verify_in_field": true })
    };
    runtime
        .submit(SubmitTransition {
            workflow_id,
            expected_version: version,
            transition: "run".into(),
            arguments,
            principal: Principal::anonymous(),
            summary: None,
            trace_id: None,
            run_id: None,
        })
        .await
        .unwrap()
}

// ── resolution (load time) ────────────────────────────────────────────────────

#[test]
fn stack_specific_cap_resolves() {
    let resolved = resolve(sdlc_config(
        Some("rust"),
        &[
            ("cap.verify.rust", verify_cap()),
            ("cap.verify.generic", verify_cap()),
        ],
    ))
    .expect("resolves");
    let t = run_transition(&resolved);
    let exec = t.pointer("/executor").expect("executor injected");

    assert_eq!(exec.pointer("/kind"), Some(&json!("workflow")));
    assert_eq!(
        exec.pointer("/definitionId"),
        Some(&json!("cap.verify.rust")),
        "stack-specific cap must win over generic"
    );
    // The `use:` block the engine wires: outputs land the cap's `verify` output
    // at $.context.verify; inputs forward the required verifyIn field.
    assert_eq!(
        exec.pointer("/use/outputs"),
        Some(&json!({ "$.context.verify": "verify" }))
    );
    assert_eq!(
        exec.pointer("/use/inputs"),
        Some(&json!({ "cwd": "$.arguments.cwd" }))
    );
    // In/Out contracts still injected.
    assert_eq!(
        t.pointer("/inputSchema"),
        Some(&json!({ "$ref": "praxec://hop#/$defs/verifyIn" }))
    );
    assert_eq!(
        resolved.pointer("/workflows/sdlc/blackboard/verify"),
        Some(&json!({ "$ref": "praxec://hop#/$defs/verifyOut" }))
    );
    // expand_use_bindings (runs after) synthesized the output mapping + embedded
    // the cap's snippet outputs, so the resolved cap's `verify` lands correctly.
    assert_eq!(
        t.pointer("/output/verify"),
        Some(&json!("$.output.verify")),
        "use.outputs must expand into the transition output mapping"
    );
    assert!(
        t.pointer("/executor/_snippetOutputs/verify").is_some(),
        "cap snippet outputs must be embedded by expand_use_bindings"
    );
}

#[test]
fn generic_fallback_when_no_stack_cap() {
    let resolved = resolve(sdlc_config(
        Some("rust"),
        &[("cap.verify.generic", verify_cap())],
    ))
    .expect("resolves");
    assert_eq!(
        run_transition(&resolved).pointer("/executor/definitionId"),
        Some(&json!("cap.verify.generic")),
        "with no cap.verify.rust, must fall back to cap.verify.generic"
    );
}

#[test]
fn absent_stack_resolves_generic() {
    let resolved =
        resolve(sdlc_config(None, &[("cap.verify.generic", verify_cap())])).expect("resolves");
    assert_eq!(
        run_transition(&resolved).pointer("/executor/definitionId"),
        Some(&json!("cap.verify.generic")),
        "absent `stack:` means the generic stack"
    );
}

#[test]
fn unresolved_cap_is_a_load_error() {
    // stack rust, and NO cap.verify.rust or cap.verify.generic loaded.
    let err = resolve(sdlc_config(Some("rust"), &[])).expect_err("must fail load");
    let msg = err.to_string();
    assert!(
        msg.contains("HOP_SLOT_UNRESOLVED")
            && msg.contains("verify")
            && msg.contains("rust")
            && msg.contains("cap.verify.generic"),
        "error must name slot, stack, and the missing caps: {msg}"
    );
}

#[test]
fn author_executor_is_a_conflict() {
    let mut cfg = sdlc_config(Some("rust"), &[("cap.verify.rust", verify_cap())]);
    cfg.pointer_mut("/workflows/sdlc/states/gate/transitions/run")
        .unwrap()
        .as_object_mut()
        .unwrap()
        .insert("executor".into(), json!({ "kind": "noop" }));
    let err = resolve(cfg).expect_err("author executor must conflict");
    assert!(
        err.to_string().contains("HOP_SLOT_EXECUTOR_CONFLICT"),
        "got: {err}"
    );
}

#[test]
fn unknown_hop_slot_name_is_a_load_error() {
    let mut cfg = sdlc_config(Some("rust"), &[("cap.verify.rust", verify_cap())]);
    cfg.pointer_mut("/workflows/sdlc/states/gate/transitions/run")
        .unwrap()
        .as_object_mut()
        .unwrap()
        .insert("hop_slot".into(), json!("frobnicate"));
    let err = resolve(cfg).expect_err("unknown slot must fail load");
    let msg = err.to_string();
    assert!(
        msg.contains("HOP_SLOT_UNKNOWN") && msg.contains("frobnicate"),
        "error must name the offending slot: {msg}"
    );
    assert!(
        msg.contains("verify") && msg.contains("lint_format"),
        "error must list the valid slot names: {msg}"
    );
}

#[test]
fn explicit_input_schema_is_not_clobbered() {
    let mut cfg = sdlc_config(Some("rust"), &[("cap.verify.rust", verify_cap())]);
    cfg.pointer_mut("/workflows/sdlc/states/gate/transitions/run")
        .unwrap()
        .as_object_mut()
        .unwrap()
        .insert("inputSchema".into(), json!({ "type": "object" }));
    let resolved = resolve(cfg).expect("resolves");
    let t = run_transition(&resolved);
    assert_eq!(
        t.pointer("/inputSchema"),
        Some(&json!({ "type": "object" })),
        "an explicit inputSchema must be preserved"
    );
    // Out slot still engine-owned; executor still resolved.
    assert_eq!(
        resolved.pointer("/workflows/sdlc/blackboard/verify"),
        Some(&json!({ "$ref": "praxec://hop#/$defs/verifyOut" }))
    );
    assert_eq!(
        t.pointer("/executor/definitionId"),
        Some(&json!("cap.verify.rust"))
    );
}

// ── repo-priority (end-to-end through load_resolved_with_repos) ────────────────

fn fixtures_root() -> PathBuf {
    let mut p = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    p.push("tests");
    p.push("fixtures");
    p.push("repos");
    p
}

fn write_host(td: &TempDir, body: &str) -> PathBuf {
    let p = td.path().join("praxec.yaml");
    std::fs::write(&p, body).unwrap();
    p
}

/// Host with two repos (`va`, `vb`) both shipping `cap.verify.generic`, at the
/// given priorities, and an sdlc flow with a `hop_slot: verify` transition.
fn priority_host(pa: i64, pb: i64) -> String {
    format!(
        r#"
version: "1.0.0"
repos:
  - path: "{a}"
    priority: {pa}
  - path: "{b}"
    priority: {pb}
workflows:
  sdlc:
    initialState: gate
    states:
      gate:
        transitions:
          run:
            target: done
            actor: agent
            hop_slot: verify
      done:
        terminal: true
"#,
        a = fixtures_root().join("verify-a").display(),
        b = fixtures_root().join("verify-b").display(),
    )
}

#[test]
fn repo_priority_higher_namespace_wins() {
    let td = TempDir::new().unwrap();
    let path = write_host(&td, &priority_host(5, 3));
    let (config, _diags) = load_resolved_with_repos(&path).expect("two-repo load");
    assert_eq!(
        config.pointer("/workflows/sdlc/states/gate/transitions/run/executor/definitionId"),
        Some(&json!("va/cap.verify.generic")),
        "the higher-priority repo (va=5 > vb=3) must win"
    );
}

#[test]
fn repo_priority_equal_is_ambiguous() {
    let td = TempDir::new().unwrap();
    let path = write_host(&td, &priority_host(5, 5));
    let err = load_resolved_with_repos(&path).expect_err("equal priority must be ambiguous");
    let msg = err.to_string();
    assert!(
        msg.contains("HOP_SLOT_AMBIGUOUS")
            && msg.contains("va/cap.verify.generic")
            && msg.contains("vb/cap.verify.generic"),
        "error must name the tied caps: {msg}"
    );
}

// ── runtime enforcement (the unbypassable guarantee, via resolution) ───────────

#[tokio::test]
async fn valid_verify_out_advances_the_transition() {
    let resolved = resolve(sdlc_config(
        Some("rust"),
        &[("cap.verify.rust", verify_cap())],
    ))
    .unwrap();
    let runtime = build_runtime(resolved, json!({ "verify": valid_verify_out() }));
    let resp = start_and_submit(&runtime, true).await;
    assert!(
        resp["error"].is_null(),
        "a conforming verifyOut must pass; got: {resp}"
    );
    assert_eq!(resp["workflow"]["state"], "done");
    assert_eq!(resp["context"]["verify"]["status"], "pass");
}

#[tokio::test]
async fn verify_out_with_bad_status_enum_is_rejected() {
    let mut bad = valid_verify_out();
    bad["status"] = json!("green"); // not a member of gateStatus
    let resolved = resolve(sdlc_config(
        Some("rust"),
        &[("cap.verify.rust", verify_cap())],
    ))
    .unwrap();
    let runtime = build_runtime(resolved, json!({ "verify": bad }));
    let resp = start_and_submit(&runtime, true).await;

    assert_eq!(
        resp["error"]["code"].as_str(),
        Some("BLACKBOARD_TYPE_ERROR"),
        "a bad status enum from the resolved cap must be rejected; got: {}",
        resp["error"]
    );
    assert!(
        resp["error"]["message"]
            .as_str()
            .unwrap_or_default()
            .contains("verify")
    );
}

#[tokio::test]
async fn verify_out_finding_fix_missing_schema_ref_is_rejected() {
    // A finding.fix (SchemaBound) missing its required `schema_ref` — proves the
    // nested $ref chain (verifyOut → finding → schemaBound) resolves at the seam.
    let mut bad = valid_verify_out();
    bad["status"] = json!("fail");
    bad["findings"] = json!([{
        "file": "src/lib.rs",
        "line": 10,
        "rule_id": "r1",
        "severity": "error",
        "message": "bad",
        "fix": { "value": { "kind": "manual" } }
    }]);
    let resolved = resolve(sdlc_config(
        Some("rust"),
        &[("cap.verify.rust", verify_cap())],
    ))
    .unwrap();
    let runtime = build_runtime(resolved, json!({ "verify": bad }));
    let resp = start_and_submit(&runtime, true).await;
    assert_eq!(
        resp["error"]["code"].as_str(),
        Some("BLACKBOARD_TYPE_ERROR"),
        "a finding.fix missing schema_ref must be rejected; got: {}",
        resp["error"]
    );
}

#[tokio::test]
async fn injected_verify_in_rejects_nonconforming_arguments() {
    // The input contract is enforced too: arguments violating verifyIn
    // (additionalProperties:false, missing required `cwd`) are rejected at submit.
    let resolved = resolve(sdlc_config(
        Some("rust"),
        &[("cap.verify.rust", verify_cap())],
    ))
    .unwrap();
    let runtime = build_runtime(resolved, json!({ "verify": valid_verify_out() }));
    let resp = start_and_submit(&runtime, false).await;
    assert_eq!(
        resp["error"]["code"].as_str(),
        Some("INPUT_SCHEMA_VIOLATION"),
        "arguments violating the injected verifyIn must be rejected; got: {}",
        resp["error"]
    );
}
