//! Shared fixtures for the split `transition_records_*` integration tests.
//!
//! These helpers were previously inlined at the top of `transition_records.rs`.
//! Extracted verbatim during the SPLIT-002 audit-resolution work so the two
//! sibling test files (`transition_records_basic.rs`,
//! `transition_records_executor.rs`) can share them.

#![allow(dead_code)]

use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use praxec_core::WorkflowRuntime;
use praxec_core::audit::{AuditEvent, AuditSink};
use praxec_core::guards::DefaultGuardEvaluator;
use praxec_core::ports::{Executor, ExecutorRegistry};
use praxec_core::store::{ConfigDefinitionStore, InMemoryWorkflowStore};
use serde_json::{Value, json};

// ---- test harness -----------------------------------------------------------

/// Executor that does nothing useful and never fails. Deterministic chains
/// reference `{ "kind": "noop" }`; the registry hands this back for any kind.
pub struct NoopExecutor;

#[async_trait]
impl Executor for NoopExecutor {
    async fn execute(
        &self,
        _: praxec_core::model::ExecuteRequest,
    ) -> Result<praxec_core::model::ExecuteResult, praxec_core::error::ExecutorError> {
        Ok(praxec_core::model::ExecuteResult::default())
    }
}

pub struct SingleExecRegistry {
    pub inner: Arc<dyn Executor>,
}

impl ExecutorRegistry for SingleExecRegistry {
    fn get(&self, _kind: &str) -> Option<Arc<dyn Executor>> {
        Some(self.inner.clone())
    }
}

/// An `AuditSink` that fails all `workflow.transition` audit events and
/// succeeds for all other event types.
pub struct FailingAuditSink {
    recorded: Mutex<Vec<AuditEvent>>,
}

impl FailingAuditSink {
    pub fn fail_all_transition_records() -> Self {
        Self {
            recorded: Mutex::new(Vec::new()),
        }
    }
}

#[async_trait]
impl AuditSink for FailingAuditSink {
    async fn record(&self, event: AuditEvent) -> anyhow::Result<()> {
        let is_transition_record = event.event_type == "workflow.transition";
        if is_transition_record {
            anyhow::bail!("simulated audit sink failure");
        }
        self.recorded.lock().unwrap().push(event);
        Ok(())
    }

    async fn list_events(&self) -> Option<Vec<AuditEvent>> {
        Some(self.recorded.lock().unwrap().clone())
    }
}

pub fn build_runtime(
    config: Value,
    audit: Arc<dyn AuditSink>,
) -> (WorkflowRuntime, Arc<InMemoryWorkflowStore>) {
    let definitions = Arc::new(ConfigDefinitionStore::from_config(&config));
    let store = Arc::new(InMemoryWorkflowStore::new());
    let executors = Arc::new(SingleExecRegistry {
        inner: Arc::new(NoopExecutor),
    });
    let guards = Arc::new(DefaultGuardEvaluator::new());
    let runtime = WorkflowRuntime::new(definitions, store.clone(), executors, guards, audit);
    (runtime, store)
}

// ---- configs ----------------------------------------------------------------

/// a -> b -> c -> d, all deterministic, d terminal. One `start` applies three
/// transitions via the deterministic chain.
pub fn three_step_chain() -> Value {
    json!({
        "version": "1.0.0",
        "workflows": {
            "pipeline": {
                "initialState": "a",
                "states": {
                    "a": {
                        "transitions": {
                            "s1": { "target": "b", "actor": "deterministic", "executor": { "kind": "noop" } }
                        }
                    },
                    "b": {
                        "transitions": {
                            "s2": { "target": "c", "actor": "deterministic", "executor": { "kind": "noop" } }
                        }
                    },
                    "c": {
                        "transitions": {
                            "s3": { "target": "d", "actor": "deterministic", "executor": { "kind": "noop" } }
                        }
                    },
                    "d": { "terminal": true }
                }
            }
        }
    })
}

/// a -> b, single agent transition. Used to drive a `submit` and observe what
/// happens when the transition record write fails.
pub fn single_agent_transition() -> Value {
    json!({
        "version": "1.0.0",
        "workflows": {
            "pipeline": {
                "initialState": "a",
                "states": {
                    "a": {
                        "transitions": {
                            "go": { "target": "b", "actor": "agent", "executor": { "kind": "noop" } }
                        }
                    },
                    "b": { "terminal": true }
                }
            }
        }
    })
}

// ---- executor used by blackboard-delta tests --------------------------------

/// An executor that returns a fixed `output` value — drives controlled
/// `merge_output` behaviour for delta tests.
pub struct FixedOutputExecutor {
    pub output: Value,
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
