//! SPEC §17.3 — `dry_run` executor. Instantiates a fresh, isolated
//! `WorkflowRuntime` per invocation and runs a scripted set of inputs
//! against a candidate definition. Returns the audit trace and final
//! workflow state.
//!
//! **Isolation invariant (poka-yoke — FMECA FM-6):** the signature cannot
//! accept production stores or audit sinks. The runtime is built internally
//! from `InMemoryWorkflowStore` + `MemoryAuditSink`. There is no `new_with_*`
//! constructor that takes external state; mutation of caller state is
//! impossible by construction.

use std::sync::Arc;

use async_trait::async_trait;
use praxec_core::WorkflowRuntime;
use praxec_core::audit::{AuditSink, MemoryAuditSink};
use praxec_core::config;
use praxec_core::error::ExecutorError;
use praxec_core::guards::DefaultGuardEvaluator;
use praxec_core::model::{ExecuteRequest, ExecuteResult, Principal, StartWorkflow};
use praxec_core::ports::{Executor, ExecutorRegistry};
use praxec_core::store::{ConfigDefinitionStore, InMemoryWorkflowStore};
use serde_json::{Value, json};

use crate::NoopExecutor;

/// The dry-run executor takes ONLY a definition and a script. Internal
/// state is constructed fresh per call. No constructor or field exposes
/// production storage.
pub struct DryRunExecutor;

#[async_trait]
impl Executor for DryRunExecutor {
    async fn execute(&self, request: ExecuteRequest) -> Result<ExecuteResult, ExecutorError> {
        let args = &request.arguments;
        // Prefer explicit `arguments.definition`; fall back to
        // `workflow.context.candidate_definition` for use in the reference
        // authoring workflow (§17.1) where deterministic chain transitions
        // receive `arguments: {}` and the candidate lives in context.
        let definition = args
            .get("definition")
            .cloned()
            .or_else(|| {
                request
                    .workflow
                    .context
                    .get("candidate_definition")
                    .cloned()
            })
            .ok_or_else(|| {
                ExecutorError::Permanent(
                    "dry_run: missing required argument `definition` \
                     (and no `candidate_definition` in workflow context)"
                        .into(),
                )
            })?;
        let definition_id = args
            .get("definition_id")
            .and_then(Value::as_str)
            .or_else(|| {
                request
                    .workflow
                    .context
                    .get("candidate_definition_id")
                    .and_then(Value::as_str)
            })
            .unwrap_or("dry_run_target")
            .to_string();
        let input = args.get("input").cloned().unwrap_or_else(|| json!({}));

        // Build the candidate config: wrap the supplied definition under
        // `workflows.<id>` and resolve through the standard config pipeline
        // so schema defaults, validation, and hash-stamping all apply.
        let candidate_config = json!({
            "version": "1.0.0",
            "workflows": { &definition_id: definition },
        });
        let resolved = config::resolve(candidate_config).map_err(|e| {
            ExecutorError::Permanent(format!("dry_run: candidate config failed to resolve: {e}"))
        })?;

        // Build an ISOLATED runtime: in-memory store, memory audit sink,
        // noop executor registry. No caller state can be referenced.
        let definitions = Arc::new(ConfigDefinitionStore::from_config(&resolved));
        let store = Arc::new(InMemoryWorkflowStore::new());
        let executors: Arc<dyn ExecutorRegistry> = Arc::new(AlwaysNoopRegistry);
        let guards = Arc::new(DefaultGuardEvaluator::new());
        let audit = Arc::new(MemoryAuditSink::new());

        let runtime = WorkflowRuntime::new(
            definitions,
            store,
            executors,
            guards,
            audit.clone() as Arc<dyn AuditSink>,
        );

        // Drive the workflow with the supplied input. We capture the start
        // response + accumulated audit events as the trace.
        let start_resp = runtime
            .start(StartWorkflow {
                definition_id: definition_id.clone(),
                input,
                principal: Principal::anonymous(),
                // Preview inherits the caller's run-ambient env: the dry-run
                // executes against the same repo root the live run operates on.
                run_env: request.workflow.run_env.clone(),
                // Top-level dry-run start: depth 0.
                depth: 0,
                parent: None,
            })
            .await
            .map_err(|e| ExecutorError::Permanent(format!("dry_run: start failed: {e}")))?;

        let trace: Vec<Value> = audit
            .snapshot()
            .iter()
            .map(|e| {
                json!({
                    "event_type":   e.event_type,
                    "workflow_id":  e.workflow_id,
                    "correlation":  e.correlation_id,
                    "actor":        e.actor,
                    "payload":      e.payload,
                })
            })
            .collect();

        Ok(ExecuteResult {
            output: json!({
                "outcome":      "ok",
                "start_response": start_resp,
                "trace":        trace,
            }),
            evidence: vec![],
            child_workflow_id: None,
            next_transition: None,
            suspend: None,
            telemetry: None,
        })
    }
}

/// Internal registry that always returns the noop executor for any kind.
/// Used by dry-run so the runtime can advance through transitions whose
/// real executors aren't available in the isolated sandbox.
struct AlwaysNoopRegistry;
impl ExecutorRegistry for AlwaysNoopRegistry {
    fn get(&self, _kind: &str) -> Option<Arc<dyn Executor>> {
        Some(Arc::new(NoopExecutor))
    }
}
