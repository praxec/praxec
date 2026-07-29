//! Atomic behavioral contract A2 — `use.inputs` seeds the child.
//!
//! A child workflow invoked via `kind: workflow` with `use.inputs: { k: <v> }`
//! must see `$.workflow.input.k == <v>` INSIDE the child. Hosted in
//! `praxec-executors` (not `praxec-core`) for the same reason
//! `scoped_capability_io_roundtrip.rs` is: exercising the real `kind: workflow`
//! dispatch needs `WorkflowExecutor`, which lives in this crate, and
//! `praxec-core` cannot depend on `praxec-executors` (would be a cycle).
//!
//! Harness copied verbatim (registry/runtime wiring) from
//! `scoped_capability_io_roundtrip.rs` — only the fixture and assertion are
//! new.
//!
//! **What it asserts.** Rather than reading `$.workflow.input.k` back out of
//! the child directly (there is no public seam that returns a child's raw
//! input to the parent's caller), the child's own deterministic transition
//! reads `$.workflow.input.k` and writes it straight to its output
//! (`echoed: "$.workflow.input.k"`), which `use.outputs` then projects onto
//! the host's context. If `use.inputs` had NOT seeded the child (or seeded
//! the wrong value), the projected value would come back null or wrong —
//! so the host-side read is a faithful proxy for "the child saw
//! `$.workflow.input.k` equal to the value the host passed via `use.inputs`."

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

// ---- harness (copied from scoped_capability_io_roundtrip.rs) -------------

struct CapTestRegistry {
    workflow_executor: OnceLock<Arc<WorkflowExecutor>>,
}

impl CapTestRegistry {
    fn new() -> Self {
        Self {
            workflow_executor: OnceLock::new(),
        }
    }

    fn install(&self, exec: Arc<WorkflowExecutor>) {
        self.workflow_executor
            .set(exec)
            .map_err(|_| ())
            .expect("workflow executor installed twice");
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
        _request: ExecuteRequest,
    ) -> Result<ExecuteResult, praxec_core::error::ExecutorError> {
        Ok(ExecuteResult::default())
    }
}

// ---- A2 ------------------------------------------------------------------

#[tokio::test]
async fn use_inputs_seeds_the_child_workflow_input() {
    // `cap.echo.input` declares one snippet input `k`. Its only transition
    // reads `$.workflow.input.k` (the child's OWN blackboard input — seeded
    // at spawn by `use.inputs`) and writes it to `echoed`. The host maps
    // `use.inputs.k` from its own `$.context.provided_value` and projects the
    // child's `echoed` output back to `$.context.echoed_value`.
    let yaml = r#"
version: "1.0.0"
workflows:
  cap.echo.input:
    initialState: ready
    snippet:
      inputs:  { k: { type: string } }
      outputs: { echoed: { type: string } }
    states:
      ready:
        transitions:
          go:
            target: done
            actor: deterministic
            executor: { kind: noop }
            output:
              echoed: "$.workflow.input.k"
      done:
        terminal: true
  flow.host:
    initialState: planning
    initialContext:
      provided_value: "seeded-by-use-inputs"
    states:
      planning:
        transitions:
          go:
            target: done
            executor:
              kind: workflow
              definitionId: cap.echo.input
              use:
                inputs:
                  k: "$.context.provided_value"
                outputs:
                  "$.context.echoed_value": echoed
      done:
        terminal: true
"#;
    let config = resolve_str(yaml).expect("config resolves");

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
    .with_writable_repo_roots(vec![praxec_core::RepoRoot::for_test()])
    .with_evidence(evidence);

    let workflow_executor = Arc::new(WorkflowExecutor::new(
        runtime.clone(),
        audit.clone() as Arc<dyn AuditSink>,
    ));
    test_registry.install(workflow_executor);

    let start_resp = runtime
        .start(StartWorkflow {
            definition_id: "flow.host".to_string(),
            input: json!({}),
            principal: Principal::anonymous(),
            run_env: praxec_core::RunEnv::for_test(),
            depth: 0,
            parent: None,
        })
        .await
        .expect("start should succeed");

    let host_wf_id = start_resp
        .pointer("/workflow/id")
        .and_then(Value::as_str)
        .expect("workflow id present")
        .to_string();
    let host_version = start_resp
        .pointer("/workflow/version")
        .and_then(Value::as_u64)
        .expect("version present");

    // `go` is a non-deterministic (agent-default) transition on the HOST side
    // (only the cap's own internal `go` is `actor: deterministic`), so the
    // host does not auto-chain through it — mirrors
    // `scoped_capability_io_roundtrip.rs`.
    let after = if start_resp
        .pointer("/workflow/state")
        .and_then(Value::as_str)
        == Some("done")
    {
        start_resp
    } else {
        runtime
            .submit(SubmitTransition {
                workflow_id: host_wf_id.clone(),
                expected_version: host_version,
                transition: "go".to_string(),
                arguments: json!({}),
                principal: Principal::anonymous(),
                summary: None,
                trace_id: None,
                run_id: None,
            })
            .await
            .expect("go should succeed")
    };

    let final_state = after
        .pointer("/workflow/state")
        .and_then(Value::as_str)
        .expect("state present");
    assert_eq!(
        final_state, "done",
        "host should reach terminal state; got {final_state}. Full resp: {after:#}"
    );

    let echoed_value = after
        .pointer("/context/echoed_value")
        .and_then(Value::as_str)
        .expect("echoed_value should be projected into host context");
    assert_eq!(
        echoed_value, "seeded-by-use-inputs",
        "the child's own `$.workflow.input.k` must equal the value the host passed \
         via `use.inputs.k` — proven by round-tripping it through the child's output \
         and the host's `use.outputs` projection"
    );
}
