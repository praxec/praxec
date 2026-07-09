//! Spec A.1 §4.2/§4.4 — SchemaBound L2 inner-value validation.
//!
//! `finding.fix` is a `SchemaBound { schema_ref, value }`. The HOP vocabulary
//! enforces the ENVELOPE (schema_ref + value present) but NOT the inner `value`
//! against the pack schema named by `schema_ref`. This suite proves:
//!   (a) a top-level `schemas:` block registers namespaced inner JSON-Schemas,
//!       every statically-referenced `schema_ref` must resolve at load
//!       (closed-world, mirrors `validate_workflow_refs_resolve`), and every
//!       registered entry must compile;
//!   (b) at the blackboard-write boundary, a present `finding.fix` has its
//!       `value` validated against the resolved schema — a non-conforming value
//!       is rejected at runtime.

use std::sync::Arc;

use async_trait::async_trait;
use praxec_core::WorkflowRuntime;
use praxec_core::audit::{AuditSink, MemoryAuditSink};
use praxec_core::config::resolve;
use praxec_core::guards::DefaultGuardEvaluator;
use praxec_core::model::{Principal, StartWorkflow, SubmitTransition};
use praxec_core::ports::{Executor, ExecutorRegistry};
use praxec_core::store::{ConfigDefinitionStore, InMemoryWorkflowStore};
use serde_json::{Value, json};

// ── harness (mirrors tests/hop_slot.rs) ────────────────────────────────────────

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
}

/// The registered inner schema for a `finding.fix` payload (Spec A.1 §2.3).
fn fix_schema() -> Value {
    json!({
        "type": "object",
        "required": ["kind"],
        "additionalProperties": false,
        "properties": {
            "kind": { "enum": ["codemod", "manual"] },
            "recipe": { "type": "string" }
        }
    })
}

/// A `hop_slot: verify` flow with a registered `schemas:` block and a
/// `cap.verify.generic` producer.
fn sdlc_with_schemas(schemas: Value) -> Value {
    json!({
        "version": "1.0.0",
        "schemas": schemas,
        "workflows": {
            "sdlc": {
                "stack": "generic",
                "initialState": "gate",
                "states": {
                    "gate": { "transitions": { "run": {
                        "target": "done",
                        "actor": "agent",
                        "hop_slot": "verify"
                    } } },
                    "done": { "terminal": true }
                }
            },
            "cap.verify.generic": {
                "initialState": "ready",
                "states": { "ready": { "terminal": true } },
                "snippet": {
                    "inputs": { "cwd": { "type": "string" } },
                    "outputs": { "verify": { "$ref": "praxec://hop#/$defs/verifyOut" } }
                }
            }
        }
    })
}

fn verify_out_with_fix(fix_value: Value) -> Value {
    json!({
        "status": "fail",
        "summary": "one finding with a fix",
        "criteria": [],
        "findings": [{
            "file": "src/lib.rs",
            "line": 10,
            "rule_id": "r1",
            "severity": "error",
            "message": "bad",
            "fix": { "schema_ref": "fix.codemod", "value": fix_value }
        }],
        "provenance": { "stack": "generic", "source": "generic" }
    })
}

async fn start_and_submit(runtime: &WorkflowRuntime) -> Value {
    let start = runtime
        .start(StartWorkflow {
            definition_id: "sdlc".into(),
            input: json!({}),
            principal: Principal::anonymous(),
            trace_id: None,
            run_id: None,
            depth: 0,
            parent: None,
        })
        .await
        .unwrap();
    let workflow_id = start["workflow"]["id"].as_str().unwrap().to_string();
    let version = start["workflow"]["version"].as_u64().unwrap();
    runtime
        .submit(SubmitTransition {
            workflow_id,
            expected_version: version,
            transition: "run".into(),
            arguments: json!({ "cwd": "." }),
            principal: Principal::anonymous(),
            summary: None,
            trace_id: None,
            run_id: None,
        })
        .await
        .unwrap()
}

// ── (a) load-time registry + closed-world ──────────────────────────────────────

#[test]
fn registered_schema_loads_clean() {
    let cfg = sdlc_with_schemas(json!({ "fix.codemod": fix_schema() }));
    resolve(cfg).expect("a config registering a valid inner schema must load");
}

#[test]
fn unregistered_schema_ref_is_a_load_error() {
    // A STATIC finding.fix (in initialContext) references a schema_ref that no
    // `schemas:` entry registers — the closed-world check must fail at load.
    let mut cfg = sdlc_with_schemas(json!({ "fix.codemod": fix_schema() }));
    cfg["workflows"]["sdlc"]["initialContext"] = json!({
        "pending_fix": { "schema_ref": "unregistered/thing", "value": { "kind": "codemod" } }
    });
    let err = resolve(cfg).expect_err("an unresolved schema_ref must fail load");
    let msg = err.to_string();
    assert!(
        msg.contains("SCHEMA_REF_UNRESOLVED") && msg.contains("unregistered/thing"),
        "error must name the code and the offending ref: {msg}"
    );
}

#[test]
fn invalid_schema_entry_is_a_load_error() {
    // A `schemas:` entry that is not a compilable JSON Schema fails at load.
    let cfg = sdlc_with_schemas(json!({
        "fix.broken": { "type": "not-a-real-type" }
    }));
    let err = resolve(cfg).expect_err("a non-compiling schema entry must fail load");
    assert!(err.to_string().contains("SCHEMA_INVALID"), "got: {err}");
}

// ── (b) runtime L2 inner-value validation ──────────────────────────────────────

#[tokio::test]
async fn conforming_fix_value_advances_the_transition() {
    let cfg = resolve(sdlc_with_schemas(json!({ "fix.codemod": fix_schema() }))).unwrap();
    let out = verify_out_with_fix(json!({ "kind": "codemod", "recipe": "swap-import" }));
    let runtime = build_runtime(cfg, json!({ "verify": out }));
    let resp = start_and_submit(&runtime).await;
    assert!(
        resp["error"].is_null(),
        "a conforming finding.fix.value must pass L2; got: {resp}"
    );
    assert_eq!(resp["workflow"]["state"], "done");
}

#[tokio::test]
async fn non_conforming_fix_value_is_rejected_at_runtime() {
    let cfg = resolve(sdlc_with_schemas(json!({ "fix.codemod": fix_schema() }))).unwrap();
    // `kind: "wrong"` is not in the registered enum [codemod, manual].
    let out = verify_out_with_fix(json!({ "kind": "wrong" }));
    let runtime = build_runtime(cfg, json!({ "verify": out }));
    let resp = start_and_submit(&runtime).await;
    assert_eq!(
        resp["error"]["code"].as_str(),
        Some("BLACKBOARD_TYPE_ERROR"),
        "a non-conforming finding.fix.value must be rejected; got: {}",
        resp["error"]
    );
    assert!(
        resp["error"]["message"]
            .as_str()
            .unwrap_or_default()
            .contains("fix.codemod"),
        "rejection must name the violated schema: {}",
        resp["error"]
    );
}

#[tokio::test]
async fn runtime_unregistered_fix_ref_is_rejected() {
    // Defense-in-depth: even when load-time closed-world passes (no static ref),
    // a producer emitting a fix whose schema_ref is unregistered is rejected at
    // the write boundary.
    let cfg = resolve(sdlc_with_schemas(json!({ "fix.codemod": fix_schema() }))).unwrap();
    let mut out = verify_out_with_fix(json!({ "kind": "codemod" }));
    out["findings"][0]["fix"]["schema_ref"] = json!("cogarch/nonexistent");
    let runtime = build_runtime(cfg, json!({ "verify": out }));
    let resp = start_and_submit(&runtime).await;
    assert_eq!(
        resp["error"]["code"].as_str(),
        Some("BLACKBOARD_TYPE_ERROR"),
        "an unregistered runtime schema_ref must be rejected; got: {}",
        resp["error"]
    );
    assert!(
        resp["error"]["message"]
            .as_str()
            .unwrap_or_default()
            .contains("cogarch/nonexistent"),
        "rejection must name the unregistered ref: {}",
        resp["error"]
    );
}
