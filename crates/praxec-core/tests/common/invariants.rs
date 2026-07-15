//! Shared fixtures for the invariants test suites (proxy, governance,
//! actor/audit). Extracted from the original `tests/invariants.rs` so the
//! three sibling test binaries can share a single recording-executor harness.

#![allow(dead_code)]

use std::sync::{
    Arc, Mutex,
    atomic::{AtomicUsize, Ordering},
};

use async_trait::async_trait;
use praxec_core::WorkflowRuntime;
use praxec_core::audit::{AuditSink, MemoryAuditSink};
use praxec_core::error::ExecutorError;
use praxec_core::guards::DefaultGuardEvaluator;
use praxec_core::model::{ExecuteRequest, ExecuteResult, Principal};
use praxec_core::ports::{Executor, ExecutorRegistry};
use praxec_core::store::{ConfigDefinitionStore, InMemoryWorkflowStore};
use serde_json::{Value, json};

#[derive(Default)]
pub struct RecordingExecutor {
    pub calls: Mutex<Vec<Value>>,
    pub output: Mutex<Value>,
    pub failures_left: AtomicUsize,
}

impl RecordingExecutor {
    pub fn new(output: Value) -> Self {
        Self {
            calls: Mutex::new(vec![]),
            output: Mutex::new(output),
            failures_left: AtomicUsize::new(0),
        }
    }
    pub fn count(&self) -> usize {
        self.calls.lock().unwrap().len()
    }
}

#[async_trait]
impl Executor for RecordingExecutor {
    async fn execute(&self, request: ExecuteRequest) -> Result<ExecuteResult, ExecutorError> {
        self.calls
            .lock()
            .unwrap()
            .push(request.executor_config.clone());
        if self.failures_left.load(Ordering::SeqCst) > 0 {
            self.failures_left.fetch_sub(1, Ordering::SeqCst);
            return Err(ExecutorError::Transient("recorded failure".into()));
        }
        Ok(ExecuteResult {
            output: self.output.lock().unwrap().clone(),
            evidence: vec![],
            child_workflow_id: None,
            next_transition: None,
            suspend: None,
            telemetry: None,
        })
    }
}

pub struct SingleExecRegistry {
    pub inner: Arc<RecordingExecutor>,
}

impl ExecutorRegistry for SingleExecRegistry {
    fn get(&self, kind: &str) -> Option<Arc<dyn Executor>> {
        match kind {
            "noop" | "test" | "mcp" | "cli" | "human" => Some(self.inner.clone()),
            _ => None,
        }
    }
}

pub fn build_runtime(
    config: Value,
    exec_output: Value,
) -> (
    WorkflowRuntime,
    Arc<RecordingExecutor>,
    Arc<MemoryAuditSink>,
) {
    let definitions = Arc::new(ConfigDefinitionStore::from_config(&config));
    let store = Arc::new(InMemoryWorkflowStore::new());
    let executor = Arc::new(RecordingExecutor::new(exec_output));
    let executors = Arc::new(SingleExecRegistry {
        inner: executor.clone(),
    });
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
    (runtime, executor, audit)
}

pub fn proxy_config() -> Value {
    json!({
        "version": "1.0.0",
        "proxy": {
            "expose": [
                {
                    "name": "echo",
                    "title": "Echo",
                    "inputSchema": {
                        "type": "object",
                        "required": ["msg"],
                        "properties": { "msg": { "type": "string" } },
                        "additionalProperties": false
                    },
                    "executor": { "kind": "noop" }
                }
            ]
        }
    })
}

pub fn governed_config() -> Value {
    json!({
        "version": "1.0.0",
        "workflows": {
            "demo": {
                "initialState": "open",
                "states": {
                    "open": {
                        "transitions": {
                            "approve": {
                                "title": "Approve",
                                "target": "done",
                                "guards": [
                                    { "kind": "permission", "permission": "demo.approve" }
                                ],
                                "executor": { "kind": "noop" }
                            }
                        }
                    },
                    "done": { "terminal": true }
                }
            }
        }
    })
}

pub fn principal_with(perms: &[&str]) -> Principal {
    Principal {
        subject: "tester".into(),
        roles: vec![],
        permissions: perms.iter().map(|s| s.to_string()).collect(),
    }
}

pub fn human_only_config() -> Value {
    json!({
        "version": "1.0.0",
        "workflows": {
            "approval": {
                "initialState": "pending",
                "states": {
                    "pending": {
                        "transitions": {
                            "approve": {
                                "title": "Approve",
                                "target": "done",
                                "actor": "human",
                                "executor": { "kind": "noop" }
                            }
                        }
                    },
                    "done": { "terminal": true }
                }
            }
        }
    })
}

pub fn human_principal() -> Principal {
    Principal {
        subject: "alice".into(),
        roles: vec![Principal::HUMAN_ROLE.into()],
        permissions: vec![],
    }
}
