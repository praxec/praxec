//! Shared fixtures for the deterministic-chain test suites
//! (`chain_basic`, `chain_audit`, `chain_guidance`).
//!
//! These helpers were originally co-located with `deterministic_chain.rs`
//! before that file was split for size. Their semantics are preserved
//! byte-for-byte from the pre-split version.

#![allow(dead_code)]

use std::sync::{
    atomic::{AtomicUsize, Ordering},
    Arc, Mutex,
};

use async_trait::async_trait;
use praxec_core::audit::{AuditSink, MemoryAuditSink};
use praxec_core::error::ExecutorError;
use praxec_core::guards::DefaultGuardEvaluator;
use praxec_core::model::{ExecuteRequest, ExecuteResult, ExecutorTelemetry};
use praxec_core::ports::{Executor, ExecutorRegistry};
use praxec_core::store::{ConfigDefinitionStore, InMemoryWorkflowStore};
use praxec_core::WorkflowRuntime;
use serde_json::{json, Value};

// ---- test harness -----------------------------------------------------------

pub struct FixedExecutor {
    output: Value,
    call_count: AtomicUsize,
}

impl FixedExecutor {
    pub fn new(output: Value) -> Self {
        Self {
            output,
            call_count: AtomicUsize::new(0),
        }
    }
    pub fn count(&self) -> usize {
        self.call_count.load(Ordering::SeqCst)
    }
}

#[async_trait]
impl Executor for FixedExecutor {
    async fn execute(&self, _: ExecuteRequest) -> Result<ExecuteResult, ExecutorError> {
        self.call_count.fetch_add(1, Ordering::SeqCst);
        Ok(ExecuteResult {
            output: self.output.clone(),
            evidence: vec![],
            child_workflow_id: None,
            next_transition: None,
            suspend: None,
            telemetry: None,
        })
    }
}

/// Returns a fixed output plus per-call cost telemetry — stands in for the
/// agent executor so a test can assert the runtime folds telemetry into the
/// `agent.completed` audit event.
pub struct TelemetryExecutor {
    output: Value,
    telemetry: ExecutorTelemetry,
}

impl TelemetryExecutor {
    pub fn new(output: Value, telemetry: ExecutorTelemetry) -> Self {
        Self { output, telemetry }
    }
}

#[async_trait]
impl Executor for TelemetryExecutor {
    async fn execute(&self, _: ExecuteRequest) -> Result<ExecuteResult, ExecutorError> {
        Ok(ExecuteResult {
            output: self.output.clone(),
            evidence: vec![],
            child_workflow_id: None,
            next_transition: None,
            suspend: None,
            telemetry: Some(self.telemetry.clone()),
        })
    }
}

/// Records the `executor_config` of the last `execute` call — lets a test
/// assert how the auto-drive composed the agent step (goal text, expected keys).
pub struct CapturingExecutor {
    output: Value,
    configs: Mutex<Vec<Value>>,
}

impl CapturingExecutor {
    pub fn new(output: Value) -> Self {
        Self {
            output,
            configs: Mutex::new(Vec::new()),
        }
    }
    /// The `executor_config` of the first call of a given `kind` (the auto-drive
    /// invokes the synthesized `kind: agent` step, then the transition's own
    /// executor — so callers must select, not assume the last call).
    pub fn config_for_kind(&self, kind: &str) -> Option<Value> {
        self.configs
            .lock()
            .unwrap()
            .iter()
            .find(|c| c.get("kind").and_then(Value::as_str) == Some(kind))
            .cloned()
    }
}

#[async_trait]
impl Executor for CapturingExecutor {
    async fn execute(&self, req: ExecuteRequest) -> Result<ExecuteResult, ExecutorError> {
        self.configs
            .lock()
            .unwrap()
            .push(req.executor_config.clone());
        Ok(ExecuteResult {
            output: self.output.clone(),
            evidence: vec![],
            child_workflow_id: None,
            next_transition: None,
            suspend: None,
            telemetry: None,
        })
    }
}

pub struct FailAfterN {
    succeed_count: AtomicUsize,
    max_successes: usize,
}

impl FailAfterN {
    pub fn new(max_successes: usize) -> Self {
        Self {
            succeed_count: AtomicUsize::new(0),
            max_successes,
        }
    }
}

#[async_trait]
impl Executor for FailAfterN {
    async fn execute(&self, _: ExecuteRequest) -> Result<ExecuteResult, ExecutorError> {
        let n = self.succeed_count.fetch_add(1, Ordering::SeqCst);
        if n < self.max_successes {
            Ok(ExecuteResult {
                output: json!({}),
                evidence: vec![],
                child_workflow_id: None,
                next_transition: None,
                suspend: None,
                telemetry: None,
            })
        } else {
            Err(ExecutorError::Permanent("simulated failure".into()))
        }
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

pub fn build_runtime_with_executor(
    config: Value,
    executor: Arc<dyn Executor>,
) -> (WorkflowRuntime, Arc<MemoryAuditSink>) {
    let definitions = Arc::new(ConfigDefinitionStore::from_config(&config));
    let store = Arc::new(InMemoryWorkflowStore::new());
    let executors = Arc::new(SingleExecRegistry { inner: executor });
    let guards = Arc::new(DefaultGuardEvaluator::new());
    let audit = Arc::new(MemoryAuditSink::new());
    let runtime = WorkflowRuntime::new(
        definitions,
        store,
        executors,
        guards,
        audit.clone() as Arc<dyn AuditSink>,
    );
    (runtime, audit)
}

pub fn build_runtime(config: Value) -> (WorkflowRuntime, Arc<FixedExecutor>, Arc<MemoryAuditSink>) {
    let executor = Arc::new(FixedExecutor::new(json!({})));
    let (runtime, audit) =
        build_runtime_with_executor(config, executor.clone() as Arc<dyn Executor>);
    (runtime, executor, audit)
}

// ---- configs ----------------------------------------------------------------

/// A → B → C where A→B is deterministic and B→C is agent.
/// Chain should auto-execute A→B, then stop at B waiting for agent.
pub fn linear_chain_stops_at_agent() -> Value {
    json!({
        "version": "1.0.0",
        "workflows": {
            "pipeline": {
                "initialState": "a",
                "states": {
                    "a": {
                        "goal": "Initialize the pipeline",
                        "guidance": "System will auto-validate inputs",
                        "transitions": {
                            "validate": {
                                "title": "Validate",
                                "target": "b",
                                "actor": "deterministic",
                                "executor": { "kind": "noop" }
                            }
                        }
                    },
                    "b": {
                        "goal": "Review validation results",
                        "guidance": "Check the context for validation output before proceeding",
                        "transitions": {
                            "deploy": {
                                "title": "Deploy",
                                "target": "c",
                                "actor": "agent",
                                "executor": { "kind": "noop" }
                            }
                        }
                    },
                    "c": { "terminal": true }
                }
            }
        }
    })
}

/// A → B → C → D all deterministic, D is terminal.
pub fn fully_deterministic_to_terminal() -> Value {
    json!({
        "version": "1.0.0",
        "workflows": {
            "pipeline": {
                "initialState": "a",
                "states": {
                    "a": {
                        "transitions": {
                            "step1": {
                                "target": "b",
                                "actor": "deterministic",
                                "executor": { "kind": "noop" }
                            }
                        }
                    },
                    "b": {
                        "transitions": {
                            "step2": {
                                "target": "c",
                                "actor": "deterministic",
                                "executor": { "kind": "noop" }
                            }
                        }
                    },
                    "c": {
                        "transitions": {
                            "step3": {
                                "target": "d",
                                "actor": "deterministic",
                                "executor": { "kind": "noop" }
                            }
                        }
                    },
                    "d": { "terminal": true }
                }
            }
        }
    })
}

/// Mixed state: A has both deterministic and agent transitions.
/// Chain should NOT execute — stops at mixed states.
pub fn mixed_state_config() -> Value {
    json!({
        "version": "1.0.0",
        "workflows": {
            "pipeline": {
                "initialState": "a",
                "states": {
                    "a": {
                        "transitions": {
                            "auto_check": {
                                "target": "b",
                                "actor": "deterministic",
                                "executor": { "kind": "noop" }
                            },
                            "manual_override": {
                                "target": "c",
                                "actor": "agent",
                                "executor": { "kind": "noop" }
                            }
                        }
                    },
                    "b": { "terminal": true },
                    "c": { "terminal": true }
                }
            }
        }
    })
}

/// Chain with maxChainDepth: 2 but 5 deterministic steps.
pub fn depth_limited_config() -> Value {
    json!({
        "version": "1.0.0",
        "workflows": {
            "pipeline": {
                "initialState": "a",
                "maxChainDepth": 2,
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
                    "d": {
                        "transitions": {
                            "s4": { "target": "e", "actor": "deterministic", "executor": { "kind": "noop" } }
                        }
                    },
                    "e": { "terminal": true }
                }
            }
        }
    })
}
