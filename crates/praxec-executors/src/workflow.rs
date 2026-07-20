//! A `workflow` executor that starts a sub-workflow and, when the child is
//! non-terminal, returns `ExecuteResult.suspend` so dispatch durably parks the
//! parent (re-driven when the child terminates). It does NOT poll: it spawns
//! (or reuses the recorded child on a re-drive), checks status once, and either
//! advances (terminal), errors (failed), or suspends (running/waiting). Two
//! invocation shapes:
//!
//! **Legacy (input-only):**
//!
//! ```yaml
//! executor:
//!   kind: workflow
//!   definitionId: with_artifact_lock
//!   input:
//!     artifact: "$.context.artifact_name"
//!     owner: "$.workflow.input.user"
//! ```
//!
//! In this shape the sub-workflow inherits the full host context as its
//! return value (back-compat for pre-v0.6 callers).
//!
//! NOTE: there is no per-call `timeoutMs`/`noProgressTimeoutMs` knob — those
//! bounded the old poll loop, which is gone. Child liveness is owned by the
//! runtime (definition-level timeouts still apply to the child mission itself).
//! The `kind_doctor` flags either knob on a `kind: workflow` step as retired.
//!
//! **Capability (use: block, SPEC §6):**
//!
//! ```yaml
//! executor:
//!   kind: workflow
//!   definitionId: cap.plan.vet
//!   use:
//!     inputs:
//!       plan: "$.context.draft_plan"
//!     outputs:
//!       "$.context.vet_verdict": verdict
//!       "$.context.vet_findings": findings
//! ```
//!
//! In this shape the capability runs in a fresh blackboard populated from
//! `use.inputs`; on completion ONLY the outputs declared in `use.outputs`
//! propagate back to the host. Each projected value is validated against
//! the capability's `snippet.outputs` schema (embedded as `_snippetOutputs`
//! at config-resolve time). A validation failure aborts the transition
//! with `ExecutorError::SchemaViolation` and emits a
//! `cap.output.schema_violation` audit event — no partial outputs reach
//! the host blackboard (the cap-scoping firewall).

use std::sync::{Arc, OnceLock};

use async_trait::async_trait;
use serde_json::{Map, Value, json};

use praxec_core::RunEnv;
use praxec_core::audit::{AuditEvent, AuditSink};
use praxec_core::error::ExecutorError;
use praxec_core::model::{
    ExecuteRequest, ExecuteResult, GetWorkflow, ParentLink, Principal, StartWorkflow, StepSuspend,
    SubworkflowSuspend,
};
use praxec_core::ports::Executor;
use praxec_core::runtime::WorkflowRuntime;
use praxec_core::use_binding::{
    project_use_outputs, repair_outputs_against_snippet, resolve_use_inputs,
    validate_outputs_against_snippet,
};

/// Maximum nesting depth for sub-workflows. A `workflow`-kind transition
/// whose sub-workflow itself contains a `workflow` transition recurses; past
/// this cap we reject fail-fast with `WORKFLOW_DEPTH_EXCEEDED` rather than
/// letting a (possibly cyclic) definition graph recurse until it exhausts the
/// stack or hangs. The cap exists to catch authoring bugs — legitimate
/// nesting this deep is vanishingly rare. (Analogous to `parallel`'s
/// `max_recursion_depth`; see PARALLEL_DEPTH there.)
const MAX_WORKFLOW_DEPTH: u32 = 10;

pub struct WorkflowExecutor {
    /// The runtime that spawns + drives sub-workflows. Late-bound via
    /// [`OnceLock`] to break the construction cycle: the production
    /// `WorkflowRuntime` is built *around* the executor registry that contains
    /// this executor, so the registry (and therefore this executor) must exist
    /// before the runtime does. `default_registry_with_late_workflow` registers
    /// a runtime-less `WorkflowExecutor` and returns its handle; the binary calls
    /// [`WorkflowExecutor::set_runtime`] once the runtime exists. (Same
    /// chicken-and-egg pattern `ParallelExecutor`/`set_registry` uses.)
    runtime: Arc<OnceLock<WorkflowRuntime>>,
    audit: Arc<dyn AuditSink>,
}

impl WorkflowExecutor {
    /// Build with the runtime already known (the eager path — used by tests and
    /// any caller that has the runtime in hand). Equivalent to
    /// [`WorkflowExecutor::late`] followed by [`set_runtime`](Self::set_runtime).
    pub fn new(runtime: WorkflowRuntime, audit: Arc<dyn AuditSink>) -> Self {
        let cell = OnceLock::new();
        let _ = cell.set(runtime);
        Self {
            runtime: Arc::new(cell),
            audit,
        }
    }

    /// Build a runtime-less executor for the production registry. The runtime
    /// is injected later via [`set_runtime`](Self::set_runtime) once it has
    /// been constructed around the registry.
    pub fn late(audit: Arc<dyn AuditSink>) -> Self {
        Self {
            runtime: Arc::new(OnceLock::new()),
            audit,
        }
    }

    /// Wire the runtime after the registry (and the runtime built around it)
    /// exist. Must be called exactly once. A second call is a construction bug
    /// — silently keeping the first runtime would route sub-workflows through a
    /// stale runtime — so panic rather than let the mistake hide (mirrors
    /// `ParallelExecutor::set_registry`).
    pub fn set_runtime(&self, runtime: WorkflowRuntime) {
        if self.runtime.set(runtime).is_err() {
            panic!(
                "WORKFLOW_EXECUTOR_DOUBLE_WIRED: set_runtime called more than once; \
                 the WorkflowRuntime must be wired exactly once after construction."
            );
        }
    }

    fn runtime(&self) -> Result<&WorkflowRuntime, ExecutorError> {
        self.runtime.get().ok_or_else(|| {
            ExecutorError::Permanent(
                "WORKFLOW_EXECUTOR_NOT_WIRED: runtime was not set after construction. \
                 Call WorkflowExecutor::set_runtime(runtime) once the WorkflowRuntime that \
                 wraps this executor's registry has been built."
                    .into(),
            )
        })
    }
}

#[async_trait]
impl Executor for WorkflowExecutor {
    async fn execute(&self, request: ExecuteRequest) -> Result<ExecuteResult, ExecutorError> {
        let definition_id = request
            .executor_config
            .get("definitionId")
            .and_then(Value::as_str)
            .ok_or_else(|| {
                ExecutorError::Permanent("workflow executor requires 'definitionId'".to_string())
            })?
            .to_string();

        // Sub-workflow recursion-depth guard. `current_depth` is the depth at
        // which THIS executor is running (0 at the top level); a sub-workflow
        // it spawns runs at `current_depth + 1`. Read straight off the parent
        // instance — persisted depth survives an async re-drive (a child driven
        // on another task) where the former `WORKFLOW_DEPTH` task-local would
        // have read 0 and silently defeated the guard. Reject before doing any
        // work once the cap is reached.
        let current_depth = request.workflow.depth;
        if current_depth >= MAX_WORKFLOW_DEPTH {
            return Err(ExecutorError::Permanent(format!(
                "WORKFLOW_DEPTH_EXCEEDED: sub-workflow nesting reached depth {} (cap {}). \
                 Likely a cyclic or runaway `workflow`-kind transition graph. The cap exists \
                 to catch authoring bugs; legitimate nesting this deep is vanishingly rare.",
                current_depth + 1,
                MAX_WORKFLOW_DEPTH
            )));
        }
        let child_depth = current_depth + 1;

        let parent_corr = request
            .correlation_id
            .clone()
            .unwrap_or_else(|| "unset-corr".to_string());

        // Branch on whether this is a capability invocation (`use:` block)
        // or the legacy input-only shape. Capability invocations get the
        // scoping firewall + snippet-output validation; legacy callers
        // keep their pre-v0.6 behavior unchanged.
        let use_block = request.executor_config.get("use").cloned();
        let snippet_outputs = request
            .executor_config
            .get("_snippetOutputs")
            .cloned()
            .unwrap_or(Value::Null);

        let runtime = self.runtime()?;

        // Reuse-or-spawn. A parent re-driven after a sub-workflow suspend
        // carries the child it's parked on in its persisted context
        // (`_subworkflow_wait.child_workflow_id`, written by
        // `suspend_on_subworkflow`). On re-drive we re-check THAT child rather
        // than start a fresh one — otherwise every re-drive would spawn a new
        // child and orphan the previous. First pass (no record) spawns as
        // before. The status to branch on comes from `start` on the spawn path
        // (it may already be terminal — a deterministic auto-chain) and from a
        // single `get` on the reuse path. No poll, no sleep: one check, then
        // advance / fail / suspend.
        // Transition-scoped reuse. A parent with SEQUENTIAL `kind: workflow`
        // leaves can carry a `_subworkflow_wait` written by an EARLIER, already-
        // resolved leaf if that wait was not cleared on advance. Only reuse the
        // recorded child when the wait's `transition` matches THIS transition —
        // the wait stores `transition` precisely to identify which leaf is
        // parked. A mismatch means the wait belongs to a different leaf, so we
        // treat it as no recorded child and spawn fresh. Without this guard,
        // leaf B would reuse leaf A's (already-done) child and map A's output.
        let reuse_child_id = request
            .workflow
            .context
            .pointer("/_subworkflow_wait")
            .and_then(|wait| {
                let wait_transition = wait.pointer("/transition").and_then(Value::as_str);
                let matches = match (&request.transition, wait_transition) {
                    (Some(req_t), Some(wait_t)) => req_t == wait_t,
                    _ => false,
                };
                if matches {
                    wait.pointer("/child_workflow_id")
                        .and_then(Value::as_str)
                        .map(str::to_string)
                } else {
                    None
                }
            });

        let (sub_workflow_id, status_resp) = match reuse_child_id {
            Some(child_id) => {
                // Re-drive: re-check the recorded child once. `use.inputs` are
                // NOT re-resolved — the child was already seeded at spawn; a
                // second resolve would be dead work (and the child is running).
                let get_resp = runtime
                    .get(GetWorkflow {
                        workflow_id: child_id.clone(),
                        principal: Principal::anonymous(),
                        trace_id: None,
                        run_id: None,
                    })
                    .await
                    .map_err(|e| {
                        ExecutorError::Permanent(format!("failed to get sub-workflow: {e}"))
                    })?;
                (child_id, get_resp)
            }
            None => {
                let sub_input = match &use_block {
                    Some(use_val) => {
                        let use_inputs = use_val.get("inputs").cloned().unwrap_or(json!({}));
                        Value::Object(resolve_use_inputs(
                            &use_inputs,
                            &request.arguments,
                            &request.workflow.context,
                            &request.workflow.input,
                            Some(&request.workflow.run_env),
                        ))
                    }
                    None => {
                        let input = request
                            .executor_config
                            .get("input")
                            .cloned()
                            .unwrap_or_else(|| json!({}));
                        resolve_input(
                            &input,
                            &request.workflow.context,
                            &request.arguments,
                            Some(&request.workflow.run_env),
                        )?
                    }
                };

                // Per-spawn repo_root override (v0.0.22). A `kind: workflow`
                // transition may route its child to a DIFFERENT declared writable
                // repo — the cross-repo case (`flow.drive-program` routes each
                // deliverable to its own repo). The `repoRoot` value is resolved
                // against the parent's spawn-time scopes (like `use.inputs`), then
                // matched against the declared writable repos with the EXACT same
                // invariant as a top-level `repoRoot` selector — a declared repo's
                // canonical path only, never an arbitrary/hallucinated path. When
                // set, the child's run-ambient root becomes the routed repo (so it
                // uses `$.run.repo_root` uniformly); run/trace correlation is
                // preserved. Absent → inherit the parent's env verbatim.
                let child_run_env = match request
                    .executor_config
                    .get("repoRoot")
                    .and_then(Value::as_str)
                {
                    Some(expr) => {
                        let resolved = praxec_core::mapping::read_in_scopes(
                            expr,
                            &request.arguments,
                            &request.workflow.context,
                            &request.workflow.input,
                            None,
                            Some(&request.workflow.run_env),
                        );
                        let selector = resolved.as_ref().and_then(Value::as_str).ok_or_else(|| {
                            ExecutorError::Permanent(format!(
                                "REPO_ROOT_OVERRIDE_UNRESOLVED: workflow-executor `repoRoot: {expr}` \
                                 resolved to {resolved:?}, not a path string naming a writable repo"
                            ))
                        })?;
                        let root = runtime.resolve_run_repo_root(Some(selector)).map_err(|e| {
                            ExecutorError::Permanent(format!("REPO_ROOT_OVERRIDE_INVALID: {e}"))
                        })?;
                        RunEnv::new(
                            root,
                            request.workflow.run_env.run_id.clone(),
                            request.workflow.run_env.trace_id.clone(),
                        )
                    }
                    None => request.workflow.run_env.clone(),
                };

                // Emit cap.invoked for capability calls so audit reconstruction
                // can link parent ↔ child via parent_correlation_id (SPEC §6.3).
                if use_block.is_some() {
                    self.audit
                        .record(
                            AuditEvent::new("cap.invoked")
                                .with_workflow(request.workflow.id.clone())
                                .with_correlation(parent_corr.clone())
                                .with_payload(json!({
                                    "definitionId": definition_id,
                                    "parent_correlation_id": parent_corr,
                                })),
                        )
                        .await
                        .unwrap_or_else(
                            |e| tracing::warn!(error = %e, "audit emit failed; event dropped"),
                        );
                }

                // `Box::pin` the recursive drive. A `kind: workflow` transition
                // spawns a child by calling `runtime.start`, which synchronously
                // drives the child's OWN deterministic chain — which may spawn
                // again. A cyclic graph therefore recurses `start → chain →
                // execute → start` on one stack until the depth guard fires at
                // MAX_WORKFLOW_DEPTH. Left inline, each level embeds the child's
                // whole `start` state machine into this executor's future, so the
                // live stack grows by a large frame per level and overflows in a
                // debug build BEFORE the guard is reached (the guard becomes a
                // correctness fiction). Boxing moves each level's state machine to
                // the heap, so per-level stack is one thin poll frame and the
                // guard is what actually stops the recursion — fail-fast, not
                // fatal-abort. Costs one heap alloc per sub-workflow spawn.
                let start_resp = Box::pin(runtime.start(StartWorkflow {
                    definition_id: definition_id.clone(),
                    input: sub_input,
                    principal: Principal::anonymous(),
                        // Inherit the parent's run-ambient env (or the routed
                        // repo when a `repoRoot` override is set — see
                        // `child_run_env` above). The run/trace correlation must
                        // survive the spawn (the former `trace_id: None,
                        // run_id: None` here silently reset correlation at every
                        // sub-workflow boundary). Inheritance is what makes a
                        // coding leaf get the real root with zero hand-threaded
                        // `repo_path`.
                        run_env: child_run_env,
                        // Stamp the child one level deeper than this parent so
                        // the recursion guard sees an accurate depth even when
                        // the child is driven on a different task.
                        depth: child_depth,
                        // P2 (Task C) — link the child back to THIS parent
                        // transition. When the child terminates, the runtime
                        // re-drives the parent's pending transition (re-entering
                        // the reuse path above), which sees the child terminal
                        // and advances. Only the SPAWN path links — the reuse
                        // path re-checks an already-linked child.
                    parent: Some(ParentLink {
                        workflow_id: request.workflow.id.clone(),
                        transition: request.transition.clone().unwrap_or_default(),
                    }),
                }))
                .await
                .map_err(|e| {
                        if use_block.is_some() {
                            let kind = "cap_start_failed";
                            let audit = self.audit.clone();
                            let wf_id = request.workflow.id.clone();
                            let def_id = definition_id.clone();
                            let corr = parent_corr.clone();
                            let err_msg = e.to_string();
                            tokio::spawn(async move {
                                audit
                                    .record(
                                        AuditEvent::new("cap.terminated")
                                            .with_workflow(wf_id)
                                            .with_correlation(corr.clone())
                                            .with_payload(json!({
                                                "definitionId":          def_id,
                                                "parent_correlation_id": corr,
                                                "error_kind":            kind,
                                                "error":                 err_msg,
                                            })),
                                    )
                                    .await
                                    .unwrap_or_else(
                                        |e| tracing::warn!(error = %e, "audit emit failed; event dropped"),
                                    );
                            });
                        }
                        ExecutorError::Permanent(format!("failed to start sub-workflow: {e}"))
                    })?;

                let sub_workflow_id = start_resp
                    .pointer("/workflow/id")
                    .and_then(Value::as_str)
                    .ok_or_else(|| {
                        ExecutorError::Permanent(
                            "sub-workflow response missing workflow.id".to_string(),
                        )
                    })?
                    .to_string();

                // SPEC §5.5 — start() may have auto-chained through a
                // deterministic transition that failed mid-stream (CHAIN_FAILED),
                // resolving the cap straight to a `failed` mission (ADR-0008).
                // Short-circuit that here (the failure is the start response's
                // own status) so the root cause propagates instead of being
                // flattened by the generic `failed` branch below.
                let start_status = start_resp
                    .pointer("/result/status")
                    .and_then(Value::as_str)
                    .unwrap_or("");
                let start_reason = start_resp
                    .pointer("/result/reason")
                    .and_then(Value::as_str)
                    .unwrap_or("");
                if start_status == "failed" {
                    let error_msg = start_resp
                        .pointer("/error/message")
                        .and_then(Value::as_str)
                        .unwrap_or("sub-workflow failed at start")
                        .to_string();
                    if use_block.is_some() {
                        emit_cap_terminated(
                            &self.audit,
                            &request,
                            &definition_id,
                            &parent_corr,
                            if start_reason == "timed_out" {
                                "cap_timeout"
                            } else {
                                "cap_failed"
                            },
                            Some(json!({ "terminal_status": start_status, "reason": start_reason, "error": error_msg })),
                        )
                        .await;
                    }
                    return Err(ExecutorError::Permanent(format!(
                        "sub-workflow '{definition_id}' failed during start ({start_reason}): {error_msg}"
                    )));
                }

                // Legacy audit event (kept for back-compat with existing
                // consumers).
                self.audit
                    .record(
                        AuditEvent::new("sub_workflow.started")
                            .with_workflow(request.workflow.id.clone())
                            .with_correlation(parent_corr.clone())
                            .with_payload(json!({
                                "sub_workflow_id": sub_workflow_id,
                                "definition_id":   definition_id,
                            })),
                    )
                    .await
                    .unwrap_or_else(
                        |e| tracing::warn!(error = %e, "audit emit failed; event dropped"),
                    );

                (sub_workflow_id, start_resp)
            }
        };

        // Check status ONCE (no loop, no sleep). `succeeded` advances on the
        // fast-path; `failed` errors; anything else (`running`/`waiting`)
        // returns `suspend` so dispatch durably parks the parent on the child
        // and re-drives this executor when the child terminates.
        let status = status_resp
            .pointer("/result/status")
            .and_then(Value::as_str)
            .unwrap_or("running");

        match status {
            "succeeded" => {
                let child_context = status_resp
                    .pointer("/context")
                    .cloned()
                    .unwrap_or_else(|| json!({}));

                self.audit
                    .record(
                        AuditEvent::new("sub_workflow.completed")
                            .with_workflow(request.workflow.id.clone())
                            .with_correlation(parent_corr.clone())
                            .with_payload(json!({
                                "sub_workflow_id": sub_workflow_id,
                            })),
                    )
                    .await
                    .unwrap_or_else(
                        |e| tracing::warn!(error = %e, "audit emit failed; event dropped"),
                    );

                // Capability shape: project ONLY declared outputs; validate;
                // return projected map keyed by cap output name so the
                // synthesized transition `output:` mapping (built at
                // config-resolve time) plucks via `$.output.<cap_output_name>`.
                // Anything else in the child context dies with the capability
                // instance.
                if let Some(use_val) = use_block.as_ref() {
                    let use_outputs = use_val.get("outputs").cloned().unwrap_or(json!({}));
                    let mut projected_by_host = project_use_outputs(&use_outputs, &child_context);
                    // Deterministic-repair rung (P12 R3.1): coerce a trivially-
                    // repairable Null output (e.g. an array field a commodity
                    // model emitted as `null`) to its empty/default value BEFORE
                    // validation — zero model calls. The repaired map is what
                    // propagates forward via `rekey_by_cap_output_name` below.
                    let repaired_slots = repair_outputs_against_snippet(
                        &snippet_outputs,
                        &use_outputs,
                        &mut projected_by_host,
                    );
                    if !repaired_slots.is_empty() {
                        tracing::debug!(
                            slots = ?repaired_slots,
                            "cap output deterministic-repair applied (P12 R3.1)"
                        );
                    }
                    if let Err(violations) = validate_outputs_against_snippet(
                        &snippet_outputs,
                        &use_outputs,
                        &projected_by_host,
                    ) {
                        let diff: Vec<Value> = violations
                            .iter()
                            .map(|v| {
                                json!({
                                    "slot":   v.slot,
                                    "reason": v.reason,
                                })
                            })
                            .collect();
                        self.audit
                            .record(
                                AuditEvent::new("cap.output.schema_violation")
                                    .with_workflow(request.workflow.id.clone())
                                    .with_correlation(parent_corr.clone())
                                    .with_payload(json!({
                                        "definitionId":          definition_id,
                                        "parent_correlation_id": parent_corr,
                                        "violations":            diff,
                                    })),
                            )
                            .await
                            .unwrap_or_else(
                                |e| tracing::warn!(error = %e, "audit emit failed; event dropped"),
                            );
                        emit_cap_terminated(
                            &self.audit,
                            &request,
                            &definition_id,
                            &parent_corr,
                            "schema_violation",
                            Some(json!({ "violations": violations.len() })),
                        )
                        .await;
                        return Err(ExecutorError::SchemaViolation(format!(
                            "capability '{definition_id}' produced outputs failing snippet \
                             contract: {}",
                            violations
                                .iter()
                                .map(|v| format!("{}: {}", v.slot, v.reason))
                                .collect::<Vec<_>>()
                                .join("; ")
                        )));
                    }
                    // Rekey by cap output name so the synthesized transition
                    // output's `$.output.<cap_output_name>` pointers resolve.
                    let by_cap_name = rekey_by_cap_output_name(&use_outputs, &projected_by_host);
                    return Ok(ExecuteResult {
                        output: Value::Object(by_cap_name),
                        evidence: vec![],
                        child_workflow_id: Some(sub_workflow_id),
                        next_transition: None,
                        suspend: None,
                        telemetry: None,
                    });
                }

                // Legacy shape — full child context returned, as today.
                Ok(ExecuteResult {
                    output: child_context,
                    evidence: vec![],
                    child_workflow_id: Some(sub_workflow_id),
                    next_transition: None,
                    suspend: None,
                    telemetry: None,
                })
            }
            "failed" => {
                let reason = status_resp
                    .pointer("/result/reason")
                    .and_then(Value::as_str)
                    .unwrap_or("");
                self.audit
                    .record(
                        AuditEvent::new("sub_workflow.failed")
                            .with_workflow(request.workflow.id.clone())
                            .with_correlation(parent_corr.clone())
                            .with_payload(json!({
                                "sub_workflow_id": sub_workflow_id,
                                "status":          status,
                                "reason":          reason,
                            })),
                    )
                    .await
                    .unwrap_or_else(
                        |e| tracing::warn!(error = %e, "audit emit failed; event dropped"),
                    );
                if use_block.is_some() {
                    emit_cap_terminated(
                        &self.audit,
                        &request,
                        &definition_id,
                        &parent_corr,
                        if reason == "timed_out" {
                            "cap_timeout"
                        } else {
                            "cap_failed"
                        },
                        Some(json!({ "terminal_status": status, "reason": reason })),
                    )
                    .await;
                }

                Err(ExecutorError::Permanent(format!(
                    "sub-workflow failed ({reason})"
                )))
            }
            // `running` / `waiting` — the child is non-terminal. Suspend: hand
            // the child id back to dispatch, which durably parks the parent
            // (writes `_subworkflow_wait`, responds `waiting`) and re-drives this
            // executor when the child terminates. No poll.
            _ => Ok(ExecuteResult {
                output: json!({}),
                evidence: vec![],
                child_workflow_id: Some(sub_workflow_id.clone()),
                next_transition: None,
                suspend: Some(StepSuspend::Subworkflow(SubworkflowSuspend {
                    child_workflow_id: sub_workflow_id,
                })),
                telemetry: None,
            }),
        }
    }
}

/// Rebuild the projected map keyed by capability output name. The runtime's
/// merge_output projection plucks via `$.output.<cap_output_name>`, so we
/// must hand it that shape — not the host-path-keyed map that
/// `project_use_outputs` produces (that one is keyed by host path because
/// other callers/tests use it directly).
fn rekey_by_cap_output_name(
    use_outputs: &Value,
    projected_by_host_path: &Map<String, Value>,
) -> Map<String, Value> {
    let mut out = Map::new();
    let Some(bindings) = use_outputs.as_object() else {
        return out;
    };
    for (host_path, cap_name_value) in bindings {
        let Some(cap_name) = cap_name_value.as_str() else {
            continue;
        };
        if let Some(v) = projected_by_host_path.get(host_path) {
            out.insert(cap_name.to_string(), v.clone());
        }
    }
    out
}

/// Fire-and-forget audit emission for `cap.terminated`. Used by every
/// abnormal-termination path in the capability branch (cap_start_failed,
/// cap_timeout, cap_failed, schema_violation). The audit emission itself
/// never blocks the executor's error return.
async fn emit_cap_terminated(
    audit: &Arc<dyn AuditSink>,
    request: &ExecuteRequest,
    definition_id: &str,
    parent_corr: &str,
    error_kind: &str,
    extra_payload: Option<Value>,
) {
    let mut payload = json!({
        "definitionId":          definition_id,
        "parent_correlation_id": parent_corr,
        "error_kind":            error_kind,
    });
    if let (Some(extra), Some(obj)) = (extra_payload, payload.as_object_mut()) {
        if let Some(extra_obj) = extra.as_object() {
            for (k, v) in extra_obj {
                obj.insert(k.clone(), v.clone());
            }
        }
    }
    audit
        .record(
            AuditEvent::new("cap.terminated")
                .with_workflow(request.workflow.id.clone())
                .with_correlation(parent_corr.to_string())
                .with_payload(payload),
        )
        .await
        .unwrap_or_else(|e| tracing::warn!(error = %e, "audit emit failed; event dropped"));
}

fn resolve_input(
    input: &Value,
    context: &Value,
    arguments: &Value,
    run_env: Option<&RunEnv>,
) -> Result<Value, ExecutorError> {
    match input {
        Value::String(s) if s.starts_with("$.") => {
            // CMP-006 (executors): a legacy `$.` sub-workflow input that fails to
            // resolve used to seed the child with null (with a warn). That silently
            // hands the child a wrong (null) input. Fail fast — a `$.`-rooted input
            // that doesn't resolve is an authoring bug, not a null seed.
            praxec_core::mapping::read_in_scopes(s, arguments, context, &json!({}), None, run_env)
                .ok_or_else(|| {
                    ExecutorError::Permanent(format!(
                        "SUBWORKFLOW_INPUT_UNRESOLVED: legacy `input:` reference '{s}' did not \
                         resolve against the available scopes (arguments / context). Refusing to \
                         seed the child with null."
                    ))
                })
        }
        Value::Object(map) => {
            let mut resolved = serde_json::Map::new();
            for (k, v) in map {
                resolved.insert(k.clone(), resolve_input(v, context, arguments, run_env)?);
            }
            Ok(Value::Object(resolved))
        }
        other => Ok(other.clone()),
    }
}
