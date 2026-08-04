//! D5 — a definition's `lifecycle:` is ECHOED in the `start`/`command
//! {definitionId}` response so a caller sees maturity AT run, not only in
//! `describe`. A placeholder (`stub`) lifecycle in particular must ride along so
//! a provisional executor is never silently mistaken for a working one; a
//! definition that declares no lifecycle adds no key (no false surfacing).

use std::sync::Arc;

use praxec_core::WorkflowRuntime;
use praxec_core::audit::{AuditSink, MemoryAuditSink};
use praxec_core::guards::DefaultGuardEvaluator;
use praxec_core::model::{Principal, StartWorkflow};
use praxec_core::store::{ConfigDefinitionStore, InMemoryWorkflowStore};
use serde_json::{Value, json};

struct EmptyRegistry;
impl praxec_core::ports::ExecutorRegistry for EmptyRegistry {
    fn get(&self, _kind: &str) -> Option<Arc<dyn praxec_core::ports::Executor>> {
        None
    }
}

fn config_with(lifecycle: Option<&str>) -> Value {
    let mut wf = json!({
        "initialState": "a",
        "states": {
            "a": {
                "transitions": {
                    "go": { "target": "b", "actor": "human", "executor": { "kind": "noop" } }
                }
            },
            "b": { "terminal": true }
        }
    });
    if let Some(l) = lifecycle {
        wf["lifecycle"] = Value::String(l.to_string());
    }
    json!({ "version": "1.0.0", "workflows": { "p": wf } })
}

fn runtime_for(config: &Value) -> WorkflowRuntime {
    WorkflowRuntime::new(
        Arc::new(ConfigDefinitionStore::from_config(config)),
        Arc::new(InMemoryWorkflowStore::new()),
        Arc::new(EmptyRegistry),
        Arc::new(DefaultGuardEvaluator::new()),
        Arc::new(MemoryAuditSink::new()) as Arc<dyn AuditSink>,
    )
    .with_writable_repo_roots(vec![praxec_core::RepoRoot::for_test()])
}

async fn start(runtime: &WorkflowRuntime) -> Value {
    runtime
        .start(StartWorkflow {
            definition_id: "p".into(),
            input: json!({}),
            principal: Principal::anonymous(),
            run_env: praxec_core::RunEnv::for_test(),
            depth: 0,
            parent: None,
        })
        .await
        .unwrap()
}

#[tokio::test]
async fn start_echoes_declared_stub_lifecycle() {
    let runtime = runtime_for(&config_with(Some("stub")));
    let resp = start(&runtime).await;
    assert_eq!(
        resp.get("lifecycle").and_then(Value::as_str),
        Some("stub"),
        "start must echo the definition's placeholder lifecycle: {resp}"
    );
}

#[tokio::test]
async fn start_echoes_working_lifecycle() {
    let runtime = runtime_for(&config_with(Some("stable")));
    let resp = start(&runtime).await;
    assert_eq!(
        resp.get("lifecycle").and_then(Value::as_str),
        Some("stable"),
        "start echoes any declared lifecycle: {resp}"
    );
}

#[tokio::test]
async fn start_omits_lifecycle_when_undeclared() {
    let runtime = runtime_for(&config_with(None));
    let resp = start(&runtime).await;
    assert!(
        resp.get("lifecycle").is_none(),
        "no lifecycle key when the definition declares none: {resp}"
    );
}
