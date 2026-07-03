//! SPEC §5.5 / V18 — runtime emission of `cap.terminated` on abnormal
//! capability termination. Covers the cap-failure axis distinct from V17
//! (which targets schema violations specifically).
//!
//! Setup: the cap is configured with a non-existent executor connection,
//! so its first transition fails with a `Permanent` ExecutorError. The
//! sub-workflow reaches the `failed` terminal status. The WorkflowExecutor
//! observes this, emits `cap.terminated` with `error_kind: cap_failed`,
//! and returns a Permanent error. The host transition's submit fails
//! and the host context stays clean.

use std::sync::{Arc, OnceLock};

use async_trait::async_trait;
use praxec_core::audit::{AuditSink, MemoryAuditSink};
use praxec_core::config::resolve_str;
use praxec_core::error::ExecutorError;
use praxec_core::guards::DefaultGuardEvaluator;
use praxec_core::model::{
    ExecuteRequest, ExecuteResult, Principal, StartWorkflow, SubmitTransition,
};
use praxec_core::ports::{Executor, ExecutorRegistry, WorkflowStore};
use praxec_core::runtime::WorkflowRuntime;
use praxec_core::store::{ConfigDefinitionStore, InMemoryEvidenceStore, InMemoryWorkflowStore};
use praxec_executors::workflow::WorkflowExecutor;
use serde_json::{Value, json};

/// Executor that always fails permanently. Wired up as `kind: mcp` so the
/// cap's primary transition (which the V6 check requires to be `kind: mcp`
/// for the `plan` verb) fails when fired.
struct FailingExecutor;
#[async_trait]
impl Executor for FailingExecutor {
    async fn execute(&self, _r: ExecuteRequest) -> Result<ExecuteResult, ExecutorError> {
        Err(ExecutorError::Permanent(
            "deliberate cap-internal failure for V18 test".to_string(),
        ))
    }
}
struct NoopExecutor;
#[async_trait]
impl Executor for NoopExecutor {
    async fn execute(&self, _r: ExecuteRequest) -> Result<ExecuteResult, ExecutorError> {
        Ok(ExecuteResult::default())
    }
}
struct V18Registry {
    workflow_executor: OnceLock<Arc<WorkflowExecutor>>,
}
impl V18Registry {
    fn new() -> Self {
        Self {
            workflow_executor: OnceLock::new(),
        }
    }
    fn install(&self, e: Arc<WorkflowExecutor>) {
        self.workflow_executor.set(e).map_err(|_| ()).unwrap();
    }
}
impl ExecutorRegistry for V18Registry {
    fn get(&self, kind: &str) -> Option<Arc<dyn Executor>> {
        match kind {
            "workflow" => self
                .workflow_executor
                .get()
                .map(|w| w.clone() as Arc<dyn Executor>),
            "mcp" => Some(Arc::new(FailingExecutor)),
            _ => Some(Arc::new(NoopExecutor)),
        }
    }
}

#[tokio::test]
async fn v18_rejects_cap_terminating_abnormally() {
    let yaml = r#"
version: "1.0.0"
workflows:
  cap.plan.vet:
    verb: plan
    initialState: vetting
    snippet:
      inputs:  {}
      outputs:
        verdict: { type: string }
    states:
      vetting:
        transitions:
          run_check:
            target: done
            # `actor: deterministic` makes start() auto-fire this
            # transition during its chain phase. FailingExecutor then
            # crashes it with Permanent → chain fails → cap reaches
            # `failed` status → WorkflowExecutor emits cap.terminated
            # with error_kind: cap_failed.
            actor: deterministic
            executor: { kind: mcp, connection: any }
      done: { terminal: true }
  flow.add-feature:
    initialState: planning
    states:
      planning:
        transitions:
          plan_drafted:
            target: done
            executor:
              kind: workflow
              definitionId: cap.plan.vet
              use:
                outputs:
                  "$.context.vet_verdict": verdict
      done: { terminal: true }
"#;
    let config = resolve_str(yaml).expect("resolves");

    let audit = Arc::new(MemoryAuditSink::new());
    let definitions = Arc::new(ConfigDefinitionStore::from_config(&config));
    let store: Arc<dyn WorkflowStore> = Arc::new(InMemoryWorkflowStore::new());
    let evidence = Arc::new(InMemoryEvidenceStore::new());
    let guards = Arc::new(DefaultGuardEvaluator::with_evidence(evidence.clone()));
    let registry = Arc::new(V18Registry::new());
    let runtime = WorkflowRuntime::new(
        definitions,
        store.clone(),
        registry.clone() as Arc<dyn ExecutorRegistry>,
        guards,
        audit.clone() as Arc<dyn AuditSink>,
    )
    .with_evidence(evidence);
    registry.install(Arc::new(WorkflowExecutor::new(
        runtime.clone(),
        audit.clone() as Arc<dyn AuditSink>,
    )));

    let start_resp = runtime
        .start(StartWorkflow {
            definition_id: "flow.add-feature".to_string(),
            input: json!({}),
            principal: Principal::anonymous(),
            trace_id: None,
            run_id: None,
            depth: 0,
            parent: None,
        })
        .await
        .expect("start succeeds");
    let host_wf_id = start_resp
        .pointer("/workflow/id")
        .and_then(Value::as_str)
        .unwrap()
        .to_string();
    let host_version = start_resp
        .pointer("/workflow/version")
        .and_then(Value::as_u64)
        .unwrap();

    let submit_resp = runtime
        .submit(SubmitTransition {
            workflow_id: host_wf_id,
            expected_version: host_version,
            transition: "plan_drafted".to_string(),
            arguments: json!({}),
            principal: Principal::anonymous(),
            summary: None,
            trace_id: None,
            run_id: None,
        })
        .await
        .expect("submit returns Ok even on cap termination");

    // The transition itself fails because the cap died mid-state-machine.
    let status = submit_resp
        .pointer("/result/status")
        .and_then(Value::as_str)
        .unwrap_or("?");
    assert_eq!(
        status, "failed",
        "host submit should fail when cap terminates abnormally. resp: {submit_resp:#}"
    );

    // cap.terminated MUST be in the audit log with error_kind set.
    let snapshot = audit.snapshot();
    let cap_term = snapshot
        .iter()
        .find(|e| e.event_type == "cap.terminated")
        .expect("cap.terminated event must be emitted");
    let kind = cap_term
        .payload
        .get("error_kind")
        .and_then(Value::as_str)
        .unwrap_or("?");
    assert_eq!(
        kind, "cap_failed",
        "error_kind should be cap_failed for abnormal terminal status; got '{kind}'"
    );
    let parent_corr = cap_term
        .payload
        .get("parent_correlation_id")
        .and_then(Value::as_str);
    assert!(
        parent_corr.is_some(),
        "parent_correlation_id MUST be set on cap.terminated"
    );

    // Host context MUST stay clean — no partial vet_verdict write.
    let host_context = submit_resp
        .pointer("/context")
        .and_then(Value::as_object)
        .expect("host context");
    assert!(
        !host_context.contains_key("vet_verdict"),
        "host slot must not be written when cap terminates abnormally; got {host_context:#?}"
    );
}
