//! Shared scenario harness for the baseline + stress integration test
//! suites. Each sibling `tests/*.rs` file pulls this in with `mod common;`.

// Each integration-test binary compiles this module, but only uses a
// subset of helpers — silence the dead-code warnings that creates.
#![allow(dead_code)]

pub mod chain;
pub mod invariants;
pub mod transition_records;

use std::sync::Arc;

use async_trait::async_trait;
use praxec_core::audit::{AuditSink, MemoryAuditSink};
use praxec_core::config::resolve_str;
use praxec_core::error::ExecutorError;
use praxec_core::guards::DefaultGuardEvaluator;
use praxec_core::model::{
    Evidence, ExecuteRequest, ExecuteResult, Principal, StartWorkflow, SubmitTransition,
};
use praxec_core::ports::{EvidenceStore, Executor, ExecutorRegistry};
use praxec_core::store::{ConfigDefinitionStore, InMemoryEvidenceStore, InMemoryWorkflowStore};
use praxec_core::WorkflowRuntime;
use serde_json::Value;

/// A canned executor that returns a fixed output (and optional evidence) on
/// every call. Useful for deterministic scenarios where the runtime is the
/// thing under test.
pub struct FixedExecutor {
    output: Value,
    evidence_kinds: Vec<String>,
}

impl FixedExecutor {
    pub fn new(output: Value) -> Self {
        Self {
            output,
            evidence_kinds: vec![],
        }
    }
}

#[async_trait]
impl Executor for FixedExecutor {
    async fn execute(&self, _req: ExecuteRequest) -> Result<ExecuteResult, ExecutorError> {
        Ok(ExecuteResult {
            output: self.output.clone(),
            evidence: self
                .evidence_kinds
                .iter()
                .map(|k| Evidence {
                    kind: k.clone(),
                    id: format!("ev_{}", k),
                    uri: None,
                    summary: None,
                    digest: None,
                    confidence: None,
                })
                .collect(),
            child_workflow_id: None,
            next_transition: None,
            suspend: None,
            telemetry: None,
        })
    }
}

/// A registry that returns a single executor for any kind. Lets scenarios
/// inject canned outputs without touching the YAML executor.kind field.
pub struct AnyKind(pub Arc<dyn Executor>);
impl ExecutorRegistry for AnyKind {
    fn get(&self, _kind: &str) -> Option<Arc<dyn Executor>> {
        Some(self.0.clone())
    }
}

/// A registry that maps kind names to specific executors. Use when scenarios
/// need different per-step behavior.
pub struct ByKind(std::collections::HashMap<String, Arc<dyn Executor>>);
impl ByKind {
    pub fn new() -> Self {
        Self(std::collections::HashMap::new())
    }
    pub fn with(mut self, kind: &str, exec: Arc<dyn Executor>) -> Self {
        self.0.insert(kind.to_string(), exec);
        self
    }
}
impl ExecutorRegistry for ByKind {
    fn get(&self, kind: &str) -> Option<Arc<dyn Executor>> {
        self.0.get(kind).cloned()
    }
}

pub struct Scenario {
    runtime: WorkflowRuntime,
    audit: Arc<MemoryAuditSink>,
    last: Option<Value>,
}

impl Scenario {
    pub fn build(yaml: &str, executors: Arc<dyn ExecutorRegistry>) -> Self {
        Self::build_with_evidence(yaml, executors, false)
    }

    pub fn build_with_evidence(
        yaml: &str,
        executors: Arc<dyn ExecutorRegistry>,
        with_evidence: bool,
    ) -> Self {
        let config = resolve_str(yaml).expect("config parses + resolves");
        let definitions = Arc::new(ConfigDefinitionStore::from_config(&config));
        let store = Arc::new(InMemoryWorkflowStore::new());
        let audit = Arc::new(MemoryAuditSink::new());
        let evidence: Arc<dyn EvidenceStore> = Arc::new(InMemoryEvidenceStore::new());

        let guards: Arc<dyn praxec_core::ports::GuardEvaluator> = if with_evidence {
            Arc::new(DefaultGuardEvaluator::with_evidence(evidence.clone()))
        } else {
            Arc::new(DefaultGuardEvaluator::new())
        };

        let runtime = WorkflowRuntime::new(
            definitions,
            store,
            executors,
            guards,
            audit.clone() as Arc<dyn AuditSink>,
        );
        let runtime = if with_evidence {
            runtime.with_evidence(evidence)
        } else {
            runtime
        };

        Scenario {
            runtime,
            audit,
            last: None,
        }
    }

    pub async fn start(&mut self, def: &str, input: Value, principal: Principal) -> &Value {
        let resp = self
            .runtime
            .start(StartWorkflow {
                definition_id: def.to_string(),
                input,
                principal,
                trace_id: None,
                run_id: None,
                depth: 0,
                parent: None,
            })
            .await
            .expect("start succeeds");
        self.last = Some(resp);
        self.last.as_ref().unwrap()
    }

    pub async fn submit(&mut self, transition: &str, args: Value, principal: Principal) -> &Value {
        let workflow_id = self.last.as_ref().unwrap()["workflow"]["id"]
            .as_str()
            .unwrap()
            .to_string();
        let version = self.last.as_ref().unwrap()["workflow"]["version"]
            .as_u64()
            .unwrap();
        let resp = self
            .runtime
            .submit(SubmitTransition {
                workflow_id,
                expected_version: version,
                transition: transition.to_string(),
                arguments: args,
                principal,
                summary: None,
                trace_id: None,
                run_id: None,
            })
            .await
            .expect("submit returns Ok (rejection is in body)");
        self.last = Some(resp);
        self.last.as_ref().unwrap()
    }

    /// Submit with an explicit (e.g. stale) expectedVersion.
    pub async fn submit_with_version(
        &mut self,
        transition: &str,
        version: u64,
        args: Value,
        principal: Principal,
    ) -> &Value {
        let workflow_id = self.last.as_ref().unwrap()["workflow"]["id"]
            .as_str()
            .unwrap()
            .to_string();
        let resp = self
            .runtime
            .submit(SubmitTransition {
                workflow_id,
                expected_version: version,
                transition: transition.to_string(),
                arguments: args,
                principal,
                summary: None,
                trace_id: None,
                run_id: None,
            })
            .await
            .expect("submit returns Ok");
        self.last = Some(resp);
        self.last.as_ref().unwrap()
    }

    pub fn last(&self) -> &Value {
        self.last.as_ref().unwrap()
    }

    pub fn link_rels(&self) -> Vec<String> {
        self.last
            .as_ref()
            .unwrap()
            .get("links")
            .and_then(Value::as_array)
            .map(|arr| {
                arr.iter()
                    .filter_map(|l| l["rel"].as_str().map(str::to_string))
                    .collect()
            })
            .unwrap_or_default()
    }

    pub fn audit_event_types(&self) -> Vec<String> {
        self.audit.event_types()
    }
}

pub fn anon() -> Principal {
    Principal::anonymous()
}

pub fn principal(perms: &[&str]) -> Principal {
    Principal {
        subject: "tester".into(),
        roles: vec![],
        permissions: perms.iter().map(|s| s.to_string()).collect(),
    }
}

pub fn human() -> Principal {
    Principal {
        subject: "human-tester".into(),
        roles: vec![Principal::HUMAN_ROLE.into()],
        permissions: vec![],
    }
}
