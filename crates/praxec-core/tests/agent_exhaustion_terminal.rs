//! Finding #13 — a permanent agent-walk exhaustion must durably terminalize
//! the instance.
//!
//! `AGENT_CHAIN_EXHAUSTED` / `AGENT_STEP_BUDGET_EXHAUSTED` mean every model in
//! the fallback chain (or the shared step budget) is spent — terminal by
//! construction: a blind re-fire can only burn the identical walk again. The
//! failure used to be response-transient (nothing persisted), leaving the
//! instance `running`/`waiting` on re-query — a zombie that stranded parked
//! parents forever and let drivers re-fire the dead walk at full model burn.
//!
//! The fix cancels the instance (idempotent, guards further submits, wakes any
//! suspended parent — the same machinery the livelock quarantine uses) at all
//! three failure arms: start-chain, submit-chain, and direct submit.

mod common;
use common::chain::*;

use std::sync::Arc;

use async_trait::async_trait;
use praxec_core::error::ExecutorError;
use praxec_core::model::{ExecuteRequest, ExecuteResult, Principal, StartWorkflow};
use praxec_core::ports::Executor;
use praxec_core::{SubmitTransition, WorkflowRuntime};
use serde_json::{Value, json};

/// Always fails with the given message (stands in for an agent executor whose
/// whole model walk is spent).
struct ExhaustedExecutor(String);

#[async_trait]
impl Executor for ExhaustedExecutor {
    async fn execute(&self, _: ExecuteRequest) -> Result<ExecuteResult, ExecutorError> {
        Err(ExecutorError::Permanent(self.0.clone()))
    }
}

/// One agent-actor transition; start parks at `editing`, submit fires the
/// executor directly (the direct-submit failure arm).
fn single_agent_config() -> Value {
    json!({
        "version": "1.0.0",
        "workflows": { "wf": {
            "initialState": "editing",
            "states": {
                "editing": { "transitions": { "edit": {
                    "target": "done", "actor": "agent",
                    "executor": { "kind": "agent" }
                }}},
                "done": { "terminal": true }
            }
        }}
    })
}

async fn start(rt: &WorkflowRuntime, definition_id: &str) -> anyhow::Result<Value> {
    rt.start(StartWorkflow {
        definition_id: definition_id.into(),
        input: json!({}),
        principal: Principal::anonymous(),
        run_env: praxec_core::RunEnv::for_test(),
        depth: 0,
        parent: None,
    })
    .await
}

async fn submit(
    rt: &WorkflowRuntime,
    id: &str,
    ver: u64,
    transition: &str,
) -> anyhow::Result<Value> {
    rt.submit(SubmitTransition {
        workflow_id: id.into(),
        expected_version: ver,
        transition: transition.into(),
        arguments: json!({}),
        principal: Principal::anonymous(),
        summary: None,
        trace_id: None,
        run_id: None,
    })
    .await
}

// ── direct-submit arm ────────────────────────────────────────────────────────

#[tokio::test]
async fn submit_chain_exhaustion_durably_cancels_the_instance() {
    let exec = Arc::new(ExhaustedExecutor(
        "AGENT_CHAIN_EXHAUSTED: all 3 models failed (walk: m1 timeout, m2 no_result, m3 timeout)"
            .into(),
    ));
    let (rt, _audit) =
        build_runtime_with_executor(single_agent_config(), exec as Arc<dyn Executor>);
    let s = start(&rt, "wf").await.unwrap();
    let id = s["workflow"]["id"].as_str().unwrap().to_string();
    let ver = s["workflow"]["version"].as_u64().unwrap();

    let resp = submit(&rt, &id, ver, "edit").await.unwrap();
    assert_eq!(resp["error"]["code"], "EXECUTOR_FAILED", "{resp}");

    // The failure is DURABLE: the instance is cancelled, not a re-fireable zombie.
    let inst = rt.load_instance(&id).await.unwrap();
    assert!(
        inst.cancelled_at.is_some(),
        "exhaustion must durably cancel the instance (finding #13)"
    );
    assert!(
        inst.cancelled_reason
            .as_deref()
            .unwrap_or_default()
            .contains("AGENT_CHAIN_EXHAUSTED"),
        "cancelled_reason must carry the exhaustion evidence: {:?}",
        inst.cancelled_reason
    );

    // A blind re-fire is refused loudly instead of burning the dead walk again.
    let refire = submit(&rt, &id, inst.version, "edit").await;
    let err = refire.expect_err("re-submit after exhaustion-cancel must be rejected");
    assert!(err.to_string().contains("WORKFLOW_CANCELLED"), "{err}");
}

#[tokio::test]
async fn submit_step_budget_exhaustion_durably_cancels_the_instance() {
    let exec = Arc::new(ExhaustedExecutor(
        "AGENT_STEP_BUDGET_EXHAUSTED: 900s step budget spent across 3 attempts".into(),
    ));
    let (rt, _audit) =
        build_runtime_with_executor(single_agent_config(), exec as Arc<dyn Executor>);
    let s = start(&rt, "wf").await.unwrap();
    let id = s["workflow"]["id"].as_str().unwrap().to_string();
    let ver = s["workflow"]["version"].as_u64().unwrap();

    submit(&rt, &id, ver, "edit").await.unwrap();

    let inst = rt.load_instance(&id).await.unwrap();
    assert!(inst.cancelled_at.is_some());
    assert!(
        inst.cancelled_reason
            .as_deref()
            .unwrap_or_default()
            .contains("AGENT_STEP_BUDGET_EXHAUSTED")
    );
}

// ── start-chain arm ──────────────────────────────────────────────────────────

#[tokio::test]
async fn start_chain_exhaustion_durably_cancels_the_instance() {
    // The deterministic chain fires at start; its executor is exhausted, so the
    // start-path ChainOutcome::Failed arm handles it.
    let exec = Arc::new(ExhaustedExecutor(
        "AGENT_CHAIN_EXHAUSTED: all models failed".into(),
    ));
    let (rt, _audit) =
        build_runtime_with_executor(fully_deterministic_to_terminal(), exec as Arc<dyn Executor>);
    let s = start(&rt, "pipeline").await.unwrap();
    assert_eq!(s["error"]["code"], "CHAIN_FAILED", "{s}");

    let id = s["workflow"]["id"].as_str().unwrap();
    let inst = rt.load_instance(id).await.unwrap();
    assert!(
        inst.cancelled_at.is_some(),
        "start-chain exhaustion must durably cancel (finding #13)"
    );
}

// ── submit-chain arm ─────────────────────────────────────────────────────────

#[tokio::test]
async fn submit_continuation_chain_exhaustion_durably_cancels_the_instance() {
    // `a --agent ok--> b --deterministic exhausted--> c`: the submit succeeds at
    // `a` and the CONTINUATION chain fails at `b`, exercising the submit-path
    // ChainOutcome::Failed arm.
    struct Router;
    #[async_trait]
    impl Executor for Router {
        async fn execute(&self, req: ExecuteRequest) -> Result<ExecuteResult, ExecutorError> {
            if req.executor_config.get("kind").and_then(Value::as_str) == Some("boom") {
                return Err(ExecutorError::Permanent(
                    "AGENT_CHAIN_EXHAUSTED: all models failed".into(),
                ));
            }
            Ok(ExecuteResult {
                output: json!({}),
                evidence: vec![],
                child_workflow_id: None,
                next_transition: None,
                suspend: None,
                telemetry: None,
            })
        }
    }
    let cfg = json!({
        "version": "1.0.0",
        "workflows": { "wf": {
            "initialState": "a",
            "states": {
                "a": { "transitions": { "go": {
                    "target": "b", "actor": "agent",
                    "executor": { "kind": "ok" }
                }}},
                "b": { "transitions": { "burn": {
                    "target": "c", "actor": "deterministic",
                    "executor": { "kind": "boom" }
                }}},
                "c": { "terminal": true }
            }
        }}
    });
    let (rt, _audit) = build_runtime_with_executor(cfg, Arc::new(Router) as Arc<dyn Executor>);
    let s = start(&rt, "wf").await.unwrap();
    let id = s["workflow"]["id"].as_str().unwrap().to_string();
    let ver = s["workflow"]["version"].as_u64().unwrap();

    let resp = submit(&rt, &id, ver, "go").await.unwrap();
    assert_eq!(resp["error"]["code"], "CHAIN_FAILED", "{resp}");

    let inst = rt.load_instance(&id).await.unwrap();
    assert!(
        inst.cancelled_at.is_some(),
        "submit-chain exhaustion must durably cancel (finding #13)"
    );
}

// ── fence: ordinary permanent failures stay recoverable ──────────────────────

#[tokio::test]
async fn a_plain_permanent_failure_stays_recoverable() {
    // Pre-#13 semantics are PRESERVED for every other failure: a permanent
    // executor error that is not an agent-walk exhaustion leaves the instance
    // re-fireable (the operator may fix config and re-poke).
    let exec = Arc::new(ExhaustedExecutor("boom: config bug".into()));
    let (rt, _audit) =
        build_runtime_with_executor(single_agent_config(), exec as Arc<dyn Executor>);
    let s = start(&rt, "wf").await.unwrap();
    let id = s["workflow"]["id"].as_str().unwrap().to_string();
    let ver = s["workflow"]["version"].as_u64().unwrap();

    let resp = submit(&rt, &id, ver, "edit").await.unwrap();
    assert_eq!(resp["error"]["code"], "EXECUTOR_FAILED", "{resp}");

    let inst = rt.load_instance(&id).await.unwrap();
    assert!(
        inst.cancelled_at.is_none(),
        "a non-exhaustion permanent failure must NOT cancel"
    );
}
