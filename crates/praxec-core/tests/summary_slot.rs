//! Guarantee tests for SPEC §6.3 — the optional `summary` slot.
//!
//! `workflow.submit` accepts an optional top-level `summary`. When present,
//! the runtime stores it to `context.summary` on commit and surfaces it at
//! the top level of every response and `workflow.get` — letting an LLM
//! resume a workflow cold without digging through the context.

use std::sync::Arc;

use async_trait::async_trait;
use praxec_core::WorkflowRuntime;
use praxec_core::audit::{AuditSink, MemoryAuditSink};
use praxec_core::guards::DefaultGuardEvaluator;
use praxec_core::model::{GetWorkflow, Principal, StartWorkflow, SubmitTransition};
use praxec_core::ports::{Executor, ExecutorRegistry};
use praxec_core::store::{ConfigDefinitionStore, InMemoryWorkflowStore};
use serde_json::{Value, json};

// ── harness ──────────────────────────────────────────────────────────────────

struct NoopExecutor;

#[async_trait]
impl Executor for NoopExecutor {
    async fn execute(
        &self,
        _: praxec_core::model::ExecuteRequest,
    ) -> Result<praxec_core::model::ExecuteResult, praxec_core::error::ExecutorError> {
        Ok(praxec_core::model::ExecuteResult::default())
    }
}

struct AnyKindRegistry;
impl ExecutorRegistry for AnyKindRegistry {
    fn get(&self, _kind: &str) -> Option<Arc<dyn Executor>> {
        Some(Arc::new(NoopExecutor))
    }
}

fn build_runtime() -> (WorkflowRuntime, Arc<MemoryAuditSink>) {
    let cfg = json!({
        "version": "1.0.0",
        "workflows": {
            "wf": {
                "initialState": "draft",
                "states": {
                    "draft": {
                        "transitions": {
                            "submit": { "target": "review", "actor": "agent" }
                        }
                    },
                    "review": {
                        "transitions": {
                            "approve": { "target": "done", "actor": "agent" }
                        }
                    },
                    "done": { "terminal": true }
                }
            }
        }
    });
    let definitions = Arc::new(ConfigDefinitionStore::from_config(&cfg));
    let store = Arc::new(InMemoryWorkflowStore::new());
    let executors = Arc::new(AnyKindRegistry);
    let guards = Arc::new(DefaultGuardEvaluator::new());
    let audit = Arc::new(MemoryAuditSink::new());
    let runtime = WorkflowRuntime::new(
        definitions,
        store,
        executors,
        guards,
        audit.clone() as Arc<dyn AuditSink>,
    )
    .with_writable_repo_roots(vec![praxec_core::RepoRoot::for_test()]);
    (runtime, audit)
}

// ── test 1 ────────────────────────────────────────────────────────────────────
// A submitted `summary` is stored to `context.summary` and surfaced at the
// top level of the response.

#[tokio::test]
async fn submit_with_summary_stores_and_surfaces() {
    let (runtime, _) = build_runtime();
    let start = runtime
        .start(StartWorkflow {
            definition_id: "wf".into(),
            input: json!({}),
            principal: Principal::anonymous(),
            run_env: praxec_core::RunEnv::for_test(),
            depth: 0,
            parent: None,
        })
        .await
        .unwrap();
    let workflow_id = start["workflow"]["id"].as_str().unwrap().to_string();
    let pre_version = start["workflow"]["version"].as_u64().unwrap();

    let resp = runtime
        .submit(SubmitTransition {
            workflow_id: workflow_id.clone(),
            expected_version: pre_version,
            transition: "submit".into(),
            arguments: json!({}),
            principal: Principal::anonymous(),
            summary: Some("Drafted with house voice; awaiting editor review.".into()),
            trace_id: None,
            run_id: None,
        })
        .await
        .unwrap();

    assert_eq!(
        resp["summary"].as_str(),
        Some("Drafted with house voice; awaiting editor review."),
        "summary must be surfaced at top level of the response"
    );
    assert_eq!(
        resp["context"]["summary"].as_str(),
        Some("Drafted with house voice; awaiting editor review."),
        "summary must also be stored to context.summary"
    );
}

// ── test 2 ────────────────────────────────────────────────────────────────────
// `workflow.get` returns the same summary so a cold resume sees it.

#[tokio::test]
async fn get_after_summary_submit_returns_summary() {
    let (runtime, _) = build_runtime();
    let start = runtime
        .start(StartWorkflow {
            definition_id: "wf".into(),
            input: json!({}),
            principal: Principal::anonymous(),
            run_env: praxec_core::RunEnv::for_test(),
            depth: 0,
            parent: None,
        })
        .await
        .unwrap();
    let workflow_id = start["workflow"]["id"].as_str().unwrap().to_string();
    let pre_version = start["workflow"]["version"].as_u64().unwrap();

    runtime
        .submit(SubmitTransition {
            workflow_id: workflow_id.clone(),
            expected_version: pre_version,
            transition: "submit".into(),
            arguments: json!({}),
            principal: Principal::anonymous(),
            summary: Some("Cold-resumeable note.".into()),
            trace_id: None,
            run_id: None,
        })
        .await
        .unwrap();

    let get_resp = runtime
        .get(GetWorkflow {
            workflow_id,
            principal: Principal::anonymous(),
            trace_id: None,
            run_id: None,
        })
        .await
        .unwrap();

    assert_eq!(
        get_resp["summary"].as_str(),
        Some("Cold-resumeable note."),
        "praxec.query(workflowId) must surface the same summary as the prior submit; got: {get_resp}"
    );
}

// ── test 3 ────────────────────────────────────────────────────────────────────
// Later submits without a `summary` argument do NOT erase the previous
// summary. Summary persists across transitions until overwritten.

#[tokio::test]
async fn summary_persists_when_omitted_on_subsequent_submit() {
    let (runtime, _) = build_runtime();
    let start = runtime
        .start(StartWorkflow {
            definition_id: "wf".into(),
            input: json!({}),
            principal: Principal::anonymous(),
            run_env: praxec_core::RunEnv::for_test(),
            depth: 0,
            parent: None,
        })
        .await
        .unwrap();
    let workflow_id = start["workflow"]["id"].as_str().unwrap().to_string();
    let v0 = start["workflow"]["version"].as_u64().unwrap();

    // First submit carries a summary.
    let after_first = runtime
        .submit(SubmitTransition {
            workflow_id: workflow_id.clone(),
            expected_version: v0,
            transition: "submit".into(),
            arguments: json!({}),
            principal: Principal::anonymous(),
            summary: Some("first".into()),
            trace_id: None,
            run_id: None,
        })
        .await
        .unwrap();
    let v1 = after_first["workflow"]["version"].as_u64().unwrap();

    // Second submit omits summary — previous value must survive.
    let after_second = runtime
        .submit(SubmitTransition {
            workflow_id: workflow_id.clone(),
            expected_version: v1,
            transition: "approve".into(),
            arguments: json!({}),
            principal: Principal::anonymous(),
            summary: None,
            trace_id: None,
            run_id: None,
        })
        .await
        .unwrap();

    assert_eq!(
        after_second["summary"].as_str(),
        Some("first"),
        "summary from an earlier submit must persist when later submits omit it"
    );
}

// ── test 4 ────────────────────────────────────────────────────────────────────
// A submitted `summary` overwrites the previous value.

#[tokio::test]
async fn later_summary_overwrites_earlier() {
    let (runtime, _) = build_runtime();
    let start = runtime
        .start(StartWorkflow {
            definition_id: "wf".into(),
            input: json!({}),
            principal: Principal::anonymous(),
            run_env: praxec_core::RunEnv::for_test(),
            depth: 0,
            parent: None,
        })
        .await
        .unwrap();
    let workflow_id = start["workflow"]["id"].as_str().unwrap().to_string();
    let v0 = start["workflow"]["version"].as_u64().unwrap();

    let r1 = runtime
        .submit(SubmitTransition {
            workflow_id: workflow_id.clone(),
            expected_version: v0,
            transition: "submit".into(),
            arguments: json!({}),
            principal: Principal::anonymous(),
            summary: Some("first".into()),
            trace_id: None,
            run_id: None,
        })
        .await
        .unwrap();
    let v1 = r1["workflow"]["version"].as_u64().unwrap();

    let r2 = runtime
        .submit(SubmitTransition {
            workflow_id,
            expected_version: v1,
            transition: "approve".into(),
            arguments: json!({}),
            principal: Principal::anonymous(),
            summary: Some("second".into()),
            trace_id: None,
            run_id: None,
        })
        .await
        .unwrap();

    assert_eq!(r2["summary"].as_str(), Some("second"));
}

fn _unused() -> Value {
    // Silence the unused-import warning for `Value` when tests are filtered.
    json!({})
}
