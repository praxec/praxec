//! SPEC §5.3 / V17 — runtime output validation.
//!
//! A capability that produces an output failing its declared
//! `snippet.outputs` schema:
//! (a) emits a `cap.output.schema_violation` audit event
//! (b) returns `ExecutorError::SchemaViolation` from the WorkflowExecutor
//! (c) leaves the host blackboard untouched — no partial output propagates
//!
//! See `tests/scoped_capability_io_roundtrip.rs` for the positive case.

use std::sync::{Arc, OnceLock};

use async_trait::async_trait;
use praxec_core::audit::{AuditSink, MemoryAuditSink};
use praxec_core::config::resolve_str;
use praxec_core::guards::DefaultGuardEvaluator;
use praxec_core::model::{
    ExecuteRequest, ExecuteResult, Principal, StartWorkflow, SubmitTransition,
};
use praxec_core::ports::{Executor, ExecutorRegistry, WorkflowStore};
use praxec_core::runtime::WorkflowRuntime;
use praxec_core::store::{ConfigDefinitionStore, InMemoryEvidenceStore, InMemoryWorkflowStore};
use praxec_executors::workflow::WorkflowExecutor;
use serde_json::{Value, json};

struct CapTestRegistry {
    workflow_executor: OnceLock<Arc<WorkflowExecutor>>,
}
impl CapTestRegistry {
    fn new() -> Self {
        Self {
            workflow_executor: OnceLock::new(),
        }
    }
    fn install(&self, e: Arc<WorkflowExecutor>) {
        self.workflow_executor.set(e).map_err(|_| ()).unwrap();
    }
}
impl ExecutorRegistry for CapTestRegistry {
    fn get(&self, kind: &str) -> Option<Arc<dyn Executor>> {
        if kind == "workflow" {
            return self
                .workflow_executor
                .get()
                .map(|w| w.clone() as Arc<dyn Executor>);
        }
        Some(Arc::new(NoopExecutor))
    }
}
struct NoopExecutor;
#[async_trait]
impl Executor for NoopExecutor {
    async fn execute(
        &self,
        _r: ExecuteRequest,
    ) -> Result<ExecuteResult, praxec_core::error::ExecutorError> {
        Ok(ExecuteResult::default())
    }
}

#[tokio::test]
async fn v17_rejects_cap_output_that_fails_snippet_schema() {
    // The cap seeds initialContext.verdict = "approved" — but its
    // snippet declares verdict ∈ {pass, fail, needs-revision}. The
    // host's submit MUST fail with a schema violation and the host's
    // context MUST stay clean.
    let yaml = r#"
version: "1.0.0"
workflows:
  cap.plan.vet:
    verb: plan
    # Initial state is terminal so the cap auto-completes against
    # initialContext — that's what surfaces the bad `verdict` value to
    # the WorkflowExecutor for snippet-schema validation. Mirrors the
    # M2 acceptance test's pattern.
    initialState: ready
    initialContext:
      verdict: "approved"  # NOT in the snippet's enum → V17 fires
    snippet:
      inputs:  {}
      outputs:
        verdict: { type: string, enum: [pass, fail, needs-revision] }
    states:
      ready: { terminal: true }
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
    let test_registry = Arc::new(CapTestRegistry::new());

    let runtime = WorkflowRuntime::new(
        definitions,
        store.clone(),
        test_registry.clone() as Arc<dyn ExecutorRegistry>,
        guards,
        audit.clone() as Arc<dyn AuditSink>,
    )
    .with_evidence(evidence);
    test_registry.install(Arc::new(WorkflowExecutor::new(
        runtime.clone(),
        audit.clone() as Arc<dyn AuditSink>,
    )));

    // Drive the host to its only non-deterministic transition.
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

    // Submit fails because the cap's schema violation prevents
    // projection — the runtime returns a rejected/failed response.
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
        .expect("submit returns Ok even when transition fails-fast");

    let status = submit_resp
        .pointer("/result/status")
        .and_then(Value::as_str)
        .unwrap_or("?");
    assert_eq!(
        status, "failed",
        "submit should return failed when cap schema violation aborts the transition. \
         resp: {submit_resp:#}"
    );

    let event_types = audit.event_types();
    assert!(
        event_types
            .iter()
            .any(|t| t == "cap.output.schema_violation"),
        "cap.output.schema_violation must be in audit log; got {event_types:?}"
    );

    // Host context MUST NOT contain the violating slot.
    let host_context = submit_resp
        .pointer("/context")
        .and_then(Value::as_object)
        .expect("host context");
    assert!(
        !host_context.contains_key("vet_verdict"),
        "vet_verdict must not be written on schema violation; got {host_context:#?}"
    );
}
