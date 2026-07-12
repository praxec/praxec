//! `submit` entry point for [`WorkflowRuntime`]. The 455-LOC submit method
//! plus its lifecycle audit lives here; the type definition and other
//! entry points (`start`, `get`) remain in `runtime.rs`. All methods
//! share the same `impl WorkflowRuntime` block split across sibling files.

use anyhow::bail;
use serde_json::{Value, json};
use uuid::Uuid;

use crate::error::{ExecutorError, LlmErrorCode};
use crate::mapping::merge_output;
use crate::mission::StatusHint;
use crate::model::{Evidence, NextTransition, Principal, SubmitTransition, WorkflowInstance};
use crate::reliability::{ReliabilityPolicy, execute_with_reliability};
use crate::runtime::runtime_links::{
    is_terminal, push_failed_chain_recovery_link, push_state_recovery_links, transition_definition,
};
use crate::runtime::runtime_records::{blackboard_delta, validate_blackboard_writes};
use crate::runtime::runtime_schema::{apply_schema_defaults, validate_schema};
use crate::runtime::{ChainOutcome, TransitionRecordParams, WorkflowRuntime};

/// SPEC §33 D3 — one `dispatch_once` cycle's two outputs: the response
/// the caller would see if the chain stopped here, plus the optional
/// next transition the executor selected for the next cycle. The chain
/// loop in `submit()` consumes the `next_transition`; only the `response`
/// flows back to callers.
/// TTL on repo file-locks held while a file-owning transition executes. A
/// holder that dies (crash/hang) has its lock reaped after this, so the FIFO
/// wait-queue never deadlocks. Long work refreshes it via heartbeat.
const LOCK_TTL: std::time::Duration = std::time::Duration::from_secs(300);

/// Render a lock file-set as JSON-friendly strings for audit payloads.
fn files_as_strings(files: &[std::path::PathBuf]) -> Vec<String> {
    files
        .iter()
        .map(|p| p.to_string_lossy().into_owned())
        .collect()
}

/// Clear a resolved `_subworkflow_wait` when a `kind: workflow` leaf advances
/// PAST a suspended sub-workflow (the executor returned a terminal child
/// result, NOT a suspend). The wait is durable bookkeeping that records which
/// child a leaf is parked on; once that leaf resolves, leaving the wait behind
/// is a latent bug: a LATER sequential `kind: workflow` leaf would reuse the
/// done child (see `workflow.rs` reuse path), and `recover_suspended_subworkflows`
/// would re-drive a transition that no longer applies.
///
/// Guarded by transition identity: only the wait belonging to the leaf that
/// just resolved (`wait.transition == resolved_transition`) is removed, so a
/// wait for a different, still-pending leaf is never clobbered. Returns whether
/// it cleared anything (for audit/observability at the call site).
pub(crate) fn clear_subworkflow_wait_on_advance(
    context: &mut Value,
    resolved_transition: &str,
) -> bool {
    let Some(obj) = context.as_object_mut() else {
        return false;
    };
    let matches = obj
        .get("_subworkflow_wait")
        .and_then(|w| w.get("transition"))
        .and_then(Value::as_str)
        .map(|t| t == resolved_transition)
        .unwrap_or(false);
    if matches {
        obj.remove("_subworkflow_wait");
        true
    } else {
        false
    }
}

/// P12 R1.4 — the `_agent_await` twin of [`clear_subworkflow_wait_on_advance`]:
/// when a transition that was parked on an agent `await_human` finally
/// advances (the resumed session completed and merged its output), drop the
/// durable wait marker so a later fire of the same transition starts a FRESH
/// agent run instead of trying to resume a consumed frame. Guarded by
/// transition identity, exactly like the sub-workflow twin.
pub(crate) fn clear_agent_await_on_advance(context: &mut Value, resolved_transition: &str) -> bool {
    let Some(obj) = context.as_object_mut() else {
        return false;
    };
    let matches = obj
        .get("_agent_await")
        .and_then(|w| w.get("transition"))
        .and_then(Value::as_str)
        .map(|t| t == resolved_transition)
        .unwrap_or(false);
    if matches {
        obj.remove("_agent_await");
        true
    } else {
        false
    }
}

/// P12 R1.4 — read the durable `_agent_await` marker when it belongs to
/// `transition` (transition-identity guarded, like the sub-workflow wait).
/// Returns `(correlation_id, prompt)` so callers can enforce the human-origin
/// gate and surface the pending question.
pub(crate) fn agent_await_for<'a>(
    context: &'a Value,
    transition: &str,
) -> Option<(&'a str, &'a str)> {
    let wait = context.get("_agent_await")?;
    if wait.get("transition").and_then(Value::as_str) != Some(transition) {
        return None;
    }
    let correlation_id = wait.get("correlation_id").and_then(Value::as_str)?;
    let prompt = wait.get("prompt").and_then(Value::as_str).unwrap_or("");
    Some((correlation_id, prompt))
}

struct DispatchOutcome {
    response: Value,
    next_transition: Option<NextTransition>,
}

impl DispatchOutcome {
    /// Build an outcome with no chain continuation — used by every
    /// `dispatch_once` exit path except the successful executor-result
    /// path that observed a `NextTransition`.
    fn terminal(response: Value) -> Self {
        Self {
            response,
            next_transition: None,
        }
    }
}

impl WorkflowRuntime {
    /// SPEC §33 D3 — runtime-driven submit chain.
    ///
    /// Each call to `dispatch_once` is one atomic submit cycle (audit,
    /// commit, and chain). If the executor produces a `NextTransition`,
    /// the runtime synthesizes a fresh `SubmitTransition` using the
    /// committed workflow's NEW version as `expected_version` and loops
    /// back into another `dispatch_once`. The chain runs until either
    /// the executor stops producing a `NextTransition`, or the
    /// `max_chained_llm_turns` cap is hit (in which case the cycle that
    /// would have exceeded the cap is NOT dispatched; instead a
    /// `LLM_CHAIN_DEPTH_EXCEEDED` audit fires and an
    /// `ExecutorError::Llm(ChainDepthExceeded, _)` propagates).
    ///
    /// `principal`, `trace_id`, `run_id` are preserved across the chain
    /// — every chained turn is the same caller submitting iteratively.
    /// `summary` on each chained `SubmitTransition` is taken from the
    /// executor's `NextTransition.summary` so the per-turn transition
    /// record carries the model's reasoning summary.
    pub async fn submit(&self, request: SubmitTransition) -> anyhow::Result<Value> {
        let max_chain = self.max_chained_llm_turns;
        let principal = request.principal.clone();
        let trace_id = request.trace_id.clone();
        let run_id = request.run_id.clone();

        let mut current = request;
        let mut depth: u32 = 0;

        loop {
            let outcome = self.dispatch_once(current).await?;
            let DispatchOutcome {
                response,
                next_transition,
            } = outcome;

            let Some(next) = next_transition else {
                return Ok(response);
            };

            // Pull the post-commit version and workflow id off the
            // response we'd otherwise return. dispatch_once produces a
            // response shaped per `runtime_response::response`, so
            // these projections are stable.
            let workflow_id = match response
                .get("workflow")
                .and_then(|w| w.get("id"))
                .and_then(Value::as_str)
            {
                Some(s) => s.to_string(),
                None => {
                    // Defensive: if the response shape ever drifts, do
                    // not silently swallow the chain. Surface the
                    // misshape rather than entering an infinite loop.
                    bail!("submit chain: dispatched response missing workflow.id");
                }
            };
            let expected_version = match response
                .get("workflow")
                .and_then(|w| w.get("version"))
                .and_then(Value::as_u64)
            {
                Some(v) => v,
                None => {
                    bail!("submit chain: dispatched response missing workflow.version");
                }
            };

            depth = depth.saturating_add(1);
            if depth > max_chain {
                // SPEC §33 FMECA F3 / D3 — cap breach. Emit a typed
                // audit event so operators can see the chain blew its
                // budget, then propagate the error.
                self.record_or_self_event(
                    crate::audit::AuditEvent::new("transition.rejected")
                        .with_workflow(&workflow_id)
                        .with_actor(&principal.subject)
                        .with_payload(json!({
                            "transition": next.transition,
                            "code": LlmErrorCode::ChainDepthExceeded.as_wire_code(),
                            "message": format!(
                                "LLM chain depth exceeded after {} turns (cap = {})",
                                depth, max_chain
                            ),
                            "fromState": response
                                .get("workflow")
                                .and_then(|w| w.get("state"))
                                .cloned()
                                .unwrap_or(Value::Null),
                        })),
                )
                .await;

                return Err(anyhow::Error::new(ExecutorError::Llm(
                    LlmErrorCode::ChainDepthExceeded,
                    format!("LLM chain depth exceeded after {depth} turns (cap = {max_chain})"),
                )));
            }

            current = SubmitTransition {
                workflow_id,
                expected_version,
                transition: next.transition,
                arguments: next.arguments,
                principal: principal.clone(),
                summary: next.summary,
                trace_id: trace_id.clone(),
                run_id: run_id.clone(),
            };
        }
    }

    /// SPEC §33 D3 — one atomic submit cycle. Hoisted out of `submit()`
    /// so the D3 chain loop can drive multiple cycles back-to-back when
    /// an LLM executor returns `ExecuteResult.next_transition`. Audit
    /// ordering invariants (§7.3 record-first, single `save_if_version`
    /// per cycle, end-of-cycle `workflow.transitioned`) are preserved
    /// byte-for-byte from the pre-D3 implementation. The returned
    /// `DispatchOutcome.next_transition` is `None` for every code path
    /// EXCEPT a successful executor invocation that produced one;
    /// callers (the chain loop in `submit`) interpret `Some(_)` as
    /// "loop again" and `None` as "return the response".
    /// Durably suspend the workflow on lock contention. Writes a `_lock_wait`
    /// record into the persisted context (survives restart), bumps the
    /// version, saves, and returns a `waiting_on_lock` response. The executor
    /// is NOT run. Re-driven later by the `LockScheduler` once the files free.
    #[allow(clippy::too_many_arguments)]
    async fn suspend_on_lock(
        &self,
        definition: &Value,
        instance: &WorkflowInstance,
        transition: &str,
        expected_version: u64,
        principal: &Principal,
        files: &[std::path::PathBuf],
        conflict: &crate::repo_locks::LockConflict,
    ) -> anyhow::Result<DispatchOutcome> {
        let mut next = instance.clone();
        let blocked_by: Vec<Value> = conflict
            .conflicts
            .iter()
            .map(|(f, h)| json!({ "file": f.to_string_lossy(), "holder": h }))
            .collect();
        let lock_wait = json!({
            "files": files_as_strings(files),
            "blockedBy": blocked_by,
            "transition": transition,
        });
        match next.context.as_object_mut() {
            Some(obj) => {
                obj.insert("_lock_wait".to_string(), lock_wait);
            }
            None => {
                next.context = json!({ "_lock_wait": lock_wait });
            }
        }
        next.version = instance.version + 1;
        // CRITICAL (durable-lifecycle): the durable `_lock_wait` record MUST
        // commit, or `recover_suspended_locks` never re-enqueues the workflow
        // after a restart (silently stranded) and the version isn't bumped
        // (concurrent-submit race). A STALE_WORKFLOW_VERSION here is a genuine
        // rejection — surface it via `?`, exactly like the other commit paths
        // in `dispatch_once`. NEVER swallow into an un-suspended instance and
        // return a `waiting_on_lock` response that lies about durability.
        let saved = self.store.save_if_version(next, expected_version).await?;
        if let Some(sched) = &self.lock_scheduler {
            sched
                .enqueue(
                    saved.id.clone(),
                    transition.to_string(),
                    files.to_vec(),
                    principal.clone(),
                )
                .await;
        }
        let _ = self
            .audit
            .record(
                instance
                    .audit_event("lock.wait.suspended")
                    .with_payload(json!({ "files": files_as_strings(files) })),
            )
            .await;
        let resp = self
            .response(
                definition,
                &saved,
                StatusHint::WaitingOnLock,
                None,
                principal,
            )
            .await;
        Ok(DispatchOutcome::terminal(resp))
    }

    /// P2 — durably suspend the transition on a sub-workflow wait. A
    /// `kind: workflow` executor whose child is non-terminal returns
    /// `ExecuteResult.suspend` instead of advancing; the runtime writes a
    /// `_subworkflow_wait` record into the persisted context (survives
    /// restart), bumps the version, saves, and returns a `waiting` response.
    /// The transition does NOT advance to its target — the parent is re-driven
    /// when the child terminates. Mirrors `suspend_on_lock` exactly.
    pub(crate) async fn suspend_on_subworkflow(
        &self,
        definition: &Value,
        instance: &WorkflowInstance,
        transition: &str,
        expected_version: u64,
        principal: &Principal,
        suspend: crate::model::SubworkflowSuspend,
    ) -> anyhow::Result<Value> {
        let mut next = instance.clone();
        let wait = json!({
            "child_workflow_id": suspend.child_workflow_id,
            "transition": transition,
        });
        match next.context.as_object_mut() {
            Some(obj) => {
                obj.insert("_subworkflow_wait".to_string(), wait);
            }
            None => {
                next.context = json!({ "_subworkflow_wait": wait });
            }
        }
        next.version = instance.version + 1;
        // The durable `_subworkflow_wait` record MUST commit, or the parent is
        // silently stranded (never re-driven when the child terminates) and the
        // version isn't bumped (concurrent-submit race). A STALE_WORKFLOW_VERSION
        // here is a genuine rejection — surface it via `?`, exactly like
        // `suspend_on_lock`. NEVER swallow it into a fake `waiting` response.
        let saved = self.store.save_if_version(next, expected_version).await?;
        let _ = self
            .audit
            .record(
                instance
                    .audit_event("sub_workflow.wait.suspended")
                    .with_payload(json!({ "child_workflow_id": suspend.child_workflow_id })),
            )
            .await;
        let resp = self
            .response(
                definition,
                &saved,
                StatusHint::WaitingOnSubworkflow,
                None,
                principal,
            )
            .await;
        Ok(resp)
    }

    /// P12 R1.4 — durably suspend the transition on a parked agent session
    /// (`await_human`). Mirrors [`Self::suspend_on_subworkflow`] EXACTLY —
    /// the same waiting representation, a different resume signal: writes an
    /// `_agent_await` record `{correlation_id, prompt, transition}` into the
    /// persisted context (survives restart; the agent's conversation itself
    /// is already parked in the `ParkedSessionStore` under `correlation_id`),
    /// bumps the version, saves, and returns a `waiting` response. The
    /// transition does NOT advance — a human resumes it by re-submitting the
    /// same transition with `arguments.reply`.
    pub(crate) async fn suspend_on_agent_await(
        &self,
        definition: &Value,
        instance: &WorkflowInstance,
        transition: &str,
        expected_version: u64,
        principal: &Principal,
        suspend: crate::model::AgentAwaitSuspend,
    ) -> anyhow::Result<Value> {
        let mut next = instance.clone();
        let wait = json!({
            "correlation_id": suspend.correlation_id,
            "prompt": suspend.prompt,
            "transition": transition,
        });
        match next.context.as_object_mut() {
            Some(obj) => {
                obj.insert("_agent_await".to_string(), wait);
            }
            None => {
                next.context = json!({ "_agent_await": wait });
            }
        }
        next.version = instance.version + 1;
        // The durable `_agent_await` record MUST commit, or the parked frame is
        // orphaned (no marker routes the human's reply back to the transition).
        // A STALE_WORKFLOW_VERSION here is a genuine rejection — surface it via
        // `?`, exactly like the sub-workflow twin. NEVER a fake `waiting`.
        let saved = self.store.save_if_version(next, expected_version).await?;
        let _ = self
            .audit
            .record(
                instance
                    .audit_event("agent.await.suspended")
                    .with_payload(json!({
                        "correlation_id": suspend.correlation_id,
                        "prompt": suspend.prompt,
                        "transition": transition,
                    })),
            )
            .await;
        let mut resp = self
            .response(
                definition,
                &saved,
                StatusHint::WaitingOnAgentAwait,
                None,
                principal,
            )
            .await;
        // Surface the pending question + resume handle on the waiting
        // response itself, so an interactive caller can relay it to the human
        // without digging through the audit trail.
        resp["await"] = json!({
            "source": "agent_await",
            "correlationId": suspend.correlation_id,
            "prompt": suspend.prompt,
            "transition": transition,
        });
        Ok(resp)
    }

    /// Re-drive suspended workflows whose files are now free (FIFO). Called on
    /// every lock release; also public for restart recovery and tests.
    pub async fn resume_ready_locks(&self) {
        let (Some(locks), Some(sched)) = (self.repo_locks.clone(), self.lock_scheduler.clone())
        else {
            return;
        };
        while let Some(w) = sched.take_ready(locks.as_ref()).await {
            self.redrive(w).await;
        }
    }

    /// Re-submit a suspended workflow's pending transition now that its files
    /// are free. Boxed to break the submit→dispatch_once→resume→redrive→submit
    /// async cycle.
    async fn redrive(&self, w: crate::lock_scheduler::Waiter) {
        let Ok(inst) = self.store.load(&w.workflow_id).await else {
            return;
        };
        let _ = self
            .audit
            .record(
                inst.audit_event("lock.resumed")
                    .with_payload(json!({ "transition": w.transition.clone() })),
            )
            .await;
        let submit = SubmitTransition {
            workflow_id: w.workflow_id,
            expected_version: inst.version,
            transition: w.transition,
            arguments: json!({}),
            principal: w.principal,
            summary: None,
            trace_id: None,
            run_id: None,
        };
        let _ = Box::pin(self.submit(submit)).await;
    }

    /// P2 (Task C) — liveness for suspended parents. When a workflow reaches a
    /// TERMINAL state, if it was spawned by a `kind: workflow` transition (its
    /// `parent` link is set), re-drive the parent's pending transition. That
    /// re-submit re-enters the `WorkflowExecutor` reuse path, which re-checks
    /// THIS now-terminal child and advances the parent past the `kind: workflow`
    /// transition.
    ///
    /// Mirrors `redrive`: load the parent, read its LIVE version (a stored
    /// `expected_version` would be stale — the suspend bumped it), audit, and
    /// `Box::pin(self.submit(..))` to break the
    /// submit→dispatch_once→resume→submit async cycle.
    ///
    /// Termination: the parent advances to its OWN target (past the
    /// `kind: workflow` transition) and does not re-suspend on the same
    /// now-terminal child, so the re-drive does not loop. The child is driven
    /// to terminal by exactly one submit upstream (the test keeps it a single
    /// human gate), so this fires once per parent.
    pub(crate) async fn resume_parent_if_any(&self, terminal: &WorkflowInstance) {
        let Some(parent) = terminal.parent.clone() else {
            return;
        };
        // An empty transition name can't be re-fired — surface the corruption
        // rather than scheduling a doomed redrive (mirrors the lock-recovery
        // corruption guard).
        if parent.transition.is_empty() {
            tracing::warn!(
                target: "praxec_core::runtime",
                child = %terminal.id,
                parent = %parent.workflow_id,
                "sub_workflow.parent.corrupt: child carries a parent link with an \
                 empty transition; cannot re-drive the parent"
            );
            return;
        }
        let Ok(parent_inst) = self.store.load(&parent.workflow_id).await else {
            tracing::warn!(
                target: "praxec_core::runtime",
                child = %terminal.id,
                parent = %parent.workflow_id,
                "sub_workflow.parent.resume: parent instance not loadable; cannot re-drive"
            );
            return;
        };
        self.record_or_self_event(
            parent_inst
                .audit_event("sub_workflow.parent.resumed")
                .with_payload(json!({
                    "child_workflow_id": terminal.id,
                    "transition": parent.transition,
                })),
        )
        .await;
        let submit = SubmitTransition {
            workflow_id: parent.workflow_id,
            // LIVE version off the loaded instance — never a stored one.
            expected_version: parent_inst.version,
            transition: parent.transition,
            arguments: json!({}),
            principal: Principal::anonymous(),
            summary: None,
            trace_id: None,
            run_id: None,
        };
        let _ = Box::pin(self.submit(submit)).await;
    }

    /// Re-register workflows suspended on a lock before a restart, so they
    /// auto-resume once their files free. Call once after construction.
    pub async fn recover_suspended_locks(&self) {
        let Some(sched) = self.lock_scheduler.clone() else {
            return;
        };
        let suspended = self.store.list_waiting_on_lock().await.unwrap_or_default();
        for inst in suspended {
            if let Some(lw) = inst.context.get("_lock_wait") {
                let files: Vec<std::path::PathBuf> = lw
                    .get("files")
                    .and_then(|v| v.as_array())
                    .map(|a| {
                        a.iter()
                            .filter_map(|x| x.as_str())
                            .map(std::path::PathBuf::from)
                            .collect()
                    })
                    .unwrap_or_default();
                let transition = lw
                    .get("transition")
                    .and_then(|v| v.as_str())
                    .unwrap_or_default()
                    .to_string();
                // A `_lock_wait` record with an empty `transition` or empty
                // `files` is corrupt: re-enqueueing it would schedule a doomed
                // redrive (no transition to re-fire / no files to wait on),
                // masking the corruption behind a silently stuck workflow.
                // Skip it and make the corruption observable instead.
                if transition.is_empty() || files.is_empty() {
                    tracing::warn!(
                        target: "praxec_core::runtime",
                        workflow = %inst.id,
                        transition = %transition,
                        file_count = files.len(),
                        "lock.recover.corrupt: skipping suspended instance with empty \
                         transition or files in its _lock_wait record"
                    );
                    self.record_or_self_event(
                        inst.audit_event("lock.recover.corrupt")
                            .with_payload(json!({
                                "transition": transition,
                                "files": files_as_strings(&files),
                                "reason": "empty transition or files in _lock_wait record",
                            })),
                    )
                    .await;
                    continue;
                }
                sched
                    .enqueue(inst.id.clone(), transition, files, Principal::anonymous())
                    .await;
            }
        }
    }

    /// Re-drive parents suspended on a sub-workflow before a restart, so a child
    /// that terminated during downtime still resumes its parent. Call once after
    /// construction (next to `recover_suspended_locks`).
    ///
    /// Unlike lock recovery there is no scheduler to re-register into: a
    /// suspended sub-workflow parent resumes by RE-DRIVING its pending
    /// transition directly. The re-submit re-enters the `WorkflowExecutor` reuse
    /// path, which re-checks the recorded child and advances the parent (child
    /// now terminal) — or re-suspends harmlessly (child still non-terminal). The
    /// re-drive reads the parent's LIVE version off the loaded instance (a
    /// stored `expected_version` would be stale — the suspend bumped it).
    pub async fn recover_suspended_subworkflows(&self) {
        let suspended = self
            .store
            .list_waiting_on_subworkflow()
            .await
            .unwrap_or_default();
        for inst in suspended {
            // Re-drive the parent's pending transition; the executor reuse path
            // re-checks the recorded child and advances/fails (or re-suspends if
            // the child is still non-terminal — harmless, no loop: the re-drive
            // is a single submit, not a poll).
            if let Some(wait) = inst.context.get("_subworkflow_wait") {
                let transition = wait
                    .get("transition")
                    .and_then(|v| v.as_str())
                    .unwrap_or_default()
                    .to_string();
                let child_workflow_id = wait
                    .get("child_workflow_id")
                    .and_then(|v| v.as_str())
                    .unwrap_or_default()
                    .to_string();
                // A `_subworkflow_wait` record with a missing or empty `transition`
                // is corrupt: re-firing it would blind-submit an empty transition
                // (or, on a missing one, silently no-op and mask the corruption
                // behind a stuck workflow). Skip it and make the corruption
                // observable instead (mirrors the lock-recovery corruption guard
                // and `resume_parent_if_any`).
                if transition.is_empty() {
                    tracing::warn!(
                        target: "praxec_core::runtime",
                        workflow = %inst.id,
                        "sub_workflow.recover.corrupt: skipping suspended parent with a \
                         missing or empty transition in its _subworkflow_wait record"
                    );
                    self.record_or_self_event(
                        inst.audit_event("sub_workflow.recover.corrupt")
                            .with_payload(json!({
                                "child_workflow_id": child_workflow_id,
                                "reason": "missing or empty transition in _subworkflow_wait record",
                            })),
                    )
                    .await;
                    continue;
                }
                // A recovery-driven resume is a notable runtime action: emit the
                // SAME audit event the live re-drive path (`resume_parent_if_any`)
                // uses, so audit consumers see recovery-driven resumes identically
                // to live ones.
                self.record_or_self_event(
                    inst.audit_event("sub_workflow.parent.resumed")
                        .with_payload(json!({
                            "child_workflow_id": child_workflow_id,
                            "transition": transition,
                        })),
                )
                .await;
                let submit = SubmitTransition {
                    workflow_id: inst.id.clone(),
                    // LIVE version off the loaded instance — never a stored one.
                    expected_version: inst.version,
                    transition,
                    arguments: json!({}),
                    principal: Principal::anonymous(),
                    summary: None,
                    trace_id: None,
                    run_id: None,
                };
                // Boxed to break the submit→dispatch→resume→submit async cycle,
                // mirroring `resume_parent_if_any`.
                let _ = Box::pin(self.submit(submit)).await;
            }
        }
    }

    async fn dispatch_once(&self, request: SubmitTransition) -> anyhow::Result<DispatchOutcome> {
        let instance = self.store.load(&request.workflow_id).await?;
        // In-flight: resolve the definition from the instance's carried
        // snapshot, never from the live `DefinitionStore` (SPEC §8.3).
        let definition = instance.definition.clone();

        let correlation_id = format!("cor_{}", Uuid::new_v4().simple());

        // T24 — cancelled workflows refuse submit. The caller sees
        // WORKFLOW_CANCELLED with the original reason in the error
        // body so retry loops don't loop forever.
        if let Some(cancelled_at) = instance.cancelled_at {
            bail!(
                "WORKFLOW_CANCELLED: workflow {} was cancelled at {} (reason: {})",
                request.workflow_id,
                cancelled_at,
                instance.cancelled_reason.as_deref().unwrap_or("(none)"),
            );
        }

        // Lazy timeout check: if more than `definition.timeoutMs` has elapsed
        // since start, fire onTimeout and short-circuit before the submit
        // gets validated / executed.
        if let Some(timed_out) = self
            .check_and_apply_timeout(&definition, instance.clone(), &request.principal)
            .await?
        {
            return Ok(DispatchOutcome::terminal(
                self.response(
                    &definition,
                    &timed_out,
                    StatusHint::TimedOut,
                    None,
                    &request.principal,
                )
                .await,
            ));
        }

        self.audit
            .record(
                instance
                    .audit_event("transition.requested")
                    .with_correlation(&correlation_id)
                    .with_actor(&request.principal.subject)
                    .with_payload(json!({
                        "transition": request.transition,
                        "expectedVersion": request.expected_version,
                        "fromState": instance.state,
                    })),
            )
            .await?;

        if instance.version != request.expected_version {
            return Ok(DispatchOutcome::terminal(
                self.record_rejected(
                    &definition,
                    &instance,
                    "STALE_WORKFLOW_VERSION",
                    format!(
                        "Expected workflow version {}, but current version is {}.",
                        request.expected_version, instance.version
                    ),
                    &request.transition,
                    &correlation_id,
                    &request.principal,
                )
                .await,
            ));
        }

        let transition =
            match transition_definition(&definition, &instance.state, &request.transition) {
                Some(value) => value.clone(),
                None => {
                    return Ok(DispatchOutcome::terminal(
                        self.record_rejected(
                            &definition,
                            &instance,
                            "INVALID_TRANSITION",
                            format!(
                                "Transition '{}' is not valid from state '{}'.",
                                request.transition, instance.state
                            ),
                            &request.transition,
                            &correlation_id,
                            &request.principal,
                        )
                        .await,
                    ));
                }
            };

        // Actor gate. A transition tagged `actor: "human"` requires the
        // submitter to be a human principal (see `Principal::is_human`).
        // Closes the loophole where an agent could call a human-only
        // transition directly even though no agent-actor link was ever
        // offered. Other actor values (`agent`, missing, custom) impose
        // no submit-time check — humans can drive agent transitions, and
        // executor-layer behaviour (e.g. the `human` executor stopping
        // state advancement) remains the second line of defence.
        if transition.get("actor").and_then(Value::as_str) == Some("human")
            && !request.principal.is_human()
        {
            return Ok(DispatchOutcome::terminal(
                self.record_rejected(
                    &definition,
                    &instance,
                    "ACTOR_MISMATCH",
                    format!(
                        "Transition '{}' requires a human principal; \
                         submitter '{}' has no '{}' role.",
                        request.transition,
                        request.principal.subject,
                        Principal::HUMAN_ROLE
                    ),
                    &request.transition,
                    &correlation_id,
                    &request.principal,
                )
                .await,
            ));
        }

        // P12 R1.4 origin gate (mirrors P16, `docs/await-resume-architecture.md`):
        // when this transition is parked on an `_agent_await` (a `kind: agent`
        // session suspended on `await_human`), re-submitting it is *resolving a
        // human gate* — only a proven-human principal may do that. No LLM in
        // the chain (including an auto-drive re-fire) may supply or trigger
        // the reply; a non-human submit is rejected typed, never executed.
        if agent_await_for(&instance.context, &request.transition).is_some()
            && !request.principal.is_human()
        {
            return Ok(DispatchOutcome::terminal(
                self.record_rejected(
                    &definition,
                    &instance,
                    "AWAIT_RESUME_NOT_HUMAN",
                    format!(
                        "Transition '{}' is parked on an agent `await_human` gate; only a \
                         human principal may resume it (submitter '{}' has no '{}' role). \
                         Resume via `praxec approvals` / a human-authenticated submit \
                         carrying `arguments.reply`.",
                        request.transition,
                        request.principal.subject,
                        Principal::HUMAN_ROLE
                    ),
                    &request.transition,
                    &correlation_id,
                    &request.principal,
                )
                .await,
            ));
        }

        // SPEC §29 — generic per-state fire cap. A transition may declare
        // `max_fires_per_visit: N` to bound how many times it can fire
        // before the workflow advances to a different state. Counter
        // lives in synthetic context slot `_fire_count.<state>.<transition>`
        // and resets on state exit (handled in clear_state_local_slots_on_exit
        // — synthetic slots whose state matches the leaving state get
        // scrubbed). Useful for `ask_human` self-loops (prevent agent
        // spamming) but generic — applies to any transition.
        if let Some(max_fires) = transition
            .get("max_fires_per_visit")
            .and_then(Value::as_u64)
        {
            let key = format!("_fire_count.{}.{}", instance.state, request.transition);
            let current = crate::model::read_counter_slot(&instance.context, &key)?;
            if current >= max_fires {
                return Ok(DispatchOutcome::terminal(
                    self.record_rejected(
                        &definition,
                        &instance,
                        "TRANSITION_FIRE_CAP_EXCEEDED",
                        format!(
                            "Transition '{}' has fired {} times in state '{}' \
                             (max_fires_per_visit = {}). Cap is per-state-entry \
                             and resets when the workflow advances. Either raise \
                             the cap, or have the workflow advance to a different \
                             state before re-firing.",
                            request.transition, current, instance.state, max_fires
                        ),
                        &request.transition,
                        &correlation_id,
                        &request.principal,
                    )
                    .await,
                ));
            }
        }

        let mut arguments = request.arguments;
        apply_schema_defaults(transition.pointer("/inputSchema"), &mut arguments);
        if let Err(err) = validate_schema(
            transition.pointer("/inputSchema"),
            &arguments,
            "transition input",
        ) {
            return Ok(DispatchOutcome::terminal(
                self.record_rejected(
                    &definition,
                    &instance,
                    "INPUT_SCHEMA_VIOLATION",
                    err.to_string(),
                    &request.transition,
                    &correlation_id,
                    &request.principal,
                )
                .await,
            ));
        }

        let outcome = match self
            .guards_pass(
                &transition,
                &instance,
                &arguments,
                &request.principal,
                &correlation_id,
            )
            .await
        {
            Ok(o) => o,
            Err(err) => {
                // SPEC §9: a guard hitting an unset slot must fail fast with
                // rich context, not a silent `false`. The runtime is the
                // backstop here even when static `check` would have caught
                // it. Other guard evaluator failures still propagate as
                // anyhow errors (executor/audit/etc. — not a SPEC-classified
                // rejection).
                if let Some(unset) = err.downcast_ref::<crate::guards::UnsetSlotError>() {
                    return Ok(DispatchOutcome::terminal(
                        self.record_rejected(
                            &definition,
                            &instance,
                            "GUARD_UNSET_SLOT",
                            unset.to_string(),
                            &request.transition,
                            &correlation_id,
                            &request.principal,
                        )
                        .await,
                    ));
                }
                return Err(err);
            }
        };
        let guard_results = outcome.evaluated;
        if !outcome.pass {
            // SPEC §20.4 — when a §20.1 filter (require_digest /
            // min_confidence) attributed the rejection, surface the
            // specific code so callers can distinguish it from generic
            // GUARD_REJECTED.
            let (code, msg) = match outcome.diagnostic.as_deref() {
                Some("EVIDENCE_DIGEST_REQUIRED") => (
                    "EVIDENCE_DIGEST_REQUIRED",
                    "Evidence guard quorum failed: a `require_digest: true` \
                     clause excluded records missing a content digest."
                        .to_string(),
                ),
                Some("EVIDENCE_CONFIDENCE_BELOW_THRESHOLD") => (
                    "EVIDENCE_CONFIDENCE_BELOW_THRESHOLD",
                    "Evidence guard quorum failed: a `min_confidence` clause \
                     excluded records whose confidence was below threshold \
                     (or missing entirely)."
                        .to_string(),
                ),
                _ => (
                    "GUARD_REJECTED",
                    "One or more guards rejected the transition.".to_string(),
                ),
            };
            return Ok(DispatchOutcome::terminal(
                self.record_rejected(
                    &definition,
                    &instance,
                    code,
                    msg,
                    &request.transition,
                    &correlation_id,
                    &request.principal,
                )
                .await,
            ));
        }

        let mut next = instance.clone();
        let mut accumulated_evidence: Vec<Evidence> = Vec::new();
        let mut child_workflow_id: Option<String> = None;
        let mut executor_outcome: Option<(bool, u64)> = None;
        // SPEC §33 D3 — captured here from the executor's ExecuteResult so
        // the chain loop in `submit()` can drive the next cycle. Only the
        // success branch of the executor invocation populates it; every
        // other dispatch_once exit returns `None` via DispatchOutcome::terminal.
        let mut captured_next_transition: Option<NextTransition> = None;

        if let Some(executor_config) = transition.get("executor") {
            let policy = ReliabilityPolicy::from_value(transition.get("reliability"))?;

            // Repo write-exclusion gate (SPEC: global file locks). Acquire this
            // transition's owned_files before executing; on contention durably
            // suspend (waiting_on_lock); release the moment execution returns.
            let lock_files = crate::repo_locks::owned_files_in(executor_config);
            let lock_holder = format!("wf:{}", instance.id);
            if let Some(locks) = self.repo_locks.clone() {
                if !lock_files.is_empty() {
                    if let Err(conflict) = locks.acquire(&lock_files, &lock_holder, LOCK_TTL).await
                    {
                        return self
                            .suspend_on_lock(
                                &definition,
                                &instance,
                                &request.transition,
                                request.expected_version,
                                &request.principal,
                                &lock_files,
                                &conflict,
                            )
                            .await;
                    }
                    let _ = self
                        .audit
                        .record(
                            instance
                                .audit_event("lock.acquired")
                                .with_payload(json!({ "files": files_as_strings(&lock_files) })),
                        )
                        .await;
                }
            }

            let exec_started = std::time::Instant::now();
            let exec_result = execute_with_reliability(
                self.executors.as_ref(),
                &self.audit,
                &next,
                Some(&request.transition),
                &arguments,
                executor_config.clone(),
                &policy,
                &correlation_id,
            )
            .await;

            // Release the lock the instant execution returns — success OR error.
            if let Some(locks) = self.repo_locks.clone() {
                if !lock_files.is_empty() {
                    locks.release(&lock_files, &lock_holder).await;
                    let _ = self
                        .audit
                        .record(
                            instance
                                .audit_event("lock.released")
                                .with_payload(json!({ "files": files_as_strings(&lock_files) })),
                        )
                        .await;
                    // Auto-resume the FIFO-first waiter whose files just freed.
                    self.resume_ready_locks().await;
                }
            }

            match exec_result {
                Ok(result) => {
                    // The one step-suspend channel: an executor that parked
                    // instead of advancing returns `suspend`; the runtime
                    // durably records the wait and returns a `waiting`
                    // response — NEVER a failure. Both sources park the
                    // mission identically (a context wait-marker + a
                    // `Waiting`-mapping StatusHint); they differ only in what
                    // resumes them (child termination vs a human reply).
                    if let Some(suspend) = result.suspend.clone() {
                        let resp = match suspend {
                            crate::model::StepSuspend::Subworkflow(s) => {
                                self.suspend_on_subworkflow(
                                    &definition,
                                    &instance,
                                    &request.transition,
                                    request.expected_version,
                                    &request.principal,
                                    s,
                                )
                                .await?
                            }
                            crate::model::StepSuspend::AgentAwait(a) => {
                                self.suspend_on_agent_await(
                                    &definition,
                                    &instance,
                                    &request.transition,
                                    request.expected_version,
                                    &request.principal,
                                    a,
                                )
                                .await?
                            }
                        };
                        return Ok(DispatchOutcome::terminal(resp));
                    }
                    executor_outcome = Some((true, exec_started.elapsed().as_millis() as u64));
                    merge_output(
                        &mut next.context,
                        transition.get("output"),
                        &arguments,
                        &next.input,
                        &result.output,
                    )?;
                    // P2 — the executor returned a TERMINAL child result (not a
                    // suspend), so this `kind: workflow` leaf is resolving and
                    // advancing. Drop the durable `_subworkflow_wait` it parked
                    // on; otherwise a LATER sequential `kind: workflow` leaf
                    // would reuse this (now-done) child. Guarded by transition
                    // identity so a wait for a different, still-pending leaf is
                    // untouched.
                    clear_subworkflow_wait_on_advance(&mut next.context, &request.transition);
                    // P12 R1.4 — likewise for an `_agent_await`: the executor
                    // returned a real result (a resumed session completed), so
                    // the consumed wait marker must not leak into a later fire
                    // of the same transition.
                    clear_agent_await_on_advance(&mut next.context, &request.transition);
                    // SPEC §6.2: typed blackboard slots are validated *before*
                    // the transition advances. A mismatch aborts here so the
                    // caller sees BLACKBOARD_TYPE_ERROR and the snapshot stays
                    // at the pre-transition version.
                    if let Err((slot, reason)) = validate_blackboard_writes(
                        &definition,
                        transition.get("output"),
                        &next.context,
                    ) {
                        return Ok(DispatchOutcome::terminal(
                            self.record_rejected(
                                &definition,
                                &instance,
                                "BLACKBOARD_TYPE_ERROR",
                                format!("output write to typed slot '{slot}': {reason}"),
                                &request.transition,
                                &correlation_id,
                                &request.principal,
                            )
                            .await,
                        ));
                    }

                    // SPEC §28: declarative slot constraints evaluated at
                    // write-time. Catches violations at the agent's edit
                    // site, not at downstream guard read. Compose with
                    // typed-schema validation above (which handles regex /
                    // min / max / length / enum); §28 adds the things
                    // JSON Schema can't express (path_allowlist,
                    // subset_of dynamic reference).
                    if let Err(v) = crate::slot_constraint::evaluate_constraints(
                        &definition,
                        &instance.state,
                        &next.context,
                    ) {
                        return Ok(DispatchOutcome::terminal(
                            self.record_rejected(
                                &definition,
                                &instance,
                                "SLOT_CONSTRAINT_VIOLATED",
                                v.message,
                                &request.transition,
                                &correlation_id,
                                &request.principal,
                            )
                            .await,
                        ));
                    }
                    child_workflow_id = result.child_workflow_id.clone();
                    // SPEC §33 D3 — capture for the chain loop. Cloned (not
                    // moved) because the executor's full ExecuteResult is
                    // consumed by the existing code below.
                    captured_next_transition = result.next_transition.clone();
                    // SPEC §20.1 — validate every evidence record's
                    // confidence range BEFORE accepting it into the
                    // workflow's accumulated evidence. Out-of-range
                    // values fail-fast with INVALID_CONFIDENCE rather
                    // than poisoning downstream guards.
                    for ev in &result.evidence {
                        if let Err(bad) = ev.validate_confidence() {
                            return Ok(DispatchOutcome::terminal(
                                self.record_rejected(
                                    &definition,
                                    &instance,
                                    "INVALID_CONFIDENCE",
                                    format!(
                                        "Evidence record (kind='{}', id='{}') has \
                                         confidence={} outside the allowed range \
                                         0.0..=1.0 (SPEC §20.1).",
                                        ev.kind, ev.id, bad
                                    ),
                                    &request.transition,
                                    &correlation_id,
                                    &request.principal,
                                )
                                .await,
                            ));
                        }
                    }
                    accumulated_evidence.extend(result.evidence);
                }
                Err(err) => {
                    // SPEC §33 audit fixup (F3 STUB-004): when the
                    // executor returns `ExecutorError::LlmWithUpdates`,
                    // the typed code is accompanied by a side-effect
                    // blackboard payload that MUST persist before the
                    // rejection is recorded. The motivating case is
                    // FMECA F1's `_llm.consecutive_no_tool_call`
                    // counter — without persistence the counter never
                    // ticks up across failed turns, and the F1 cap
                    // (`max_iterations` consecutive failures) never
                    // fires. The merge uses the same `merge_output`
                    // pathway the success path uses; persistence
                    // bumps `next.version` so the response reflects
                    // the new state and the next caller's
                    // `expectedVersion` stays consistent.
                    //
                    // Merge / save failures degrade to "counter doesn't
                    // persist for this turn" — logged loudly. The
                    // rejection is still recorded so observability is
                    // preserved even when the counter mechanism breaks.
                    // CMP-015 — persisting the side-effect counter can fail two
                    // very different ways and they MUST NOT be conflated:
                    //
                    //  - A version CONFLICT (a concurrent writer advanced the
                    //    instance between our load and this save) means our
                    //    whole `next` snapshot is stale. Silently continuing
                    //    with a stale `instance.clone()` and recording an
                    //    EXECUTOR_FAILED rejection at the wrong version would
                    //    corrupt version coherence. Propagate it as the
                    //    standard STALE_WORKFLOW_VERSION rejection (mirrors the
                    //    pre-dispatch staleness check) so the caller retries
                    //    against the current version.
                    //
                    //  - A genuinely transient store error (I/O, etc.) is
                    //    non-critical for *this* turn: we warn-and-continue,
                    //    but stamp `persistenceDropped` into the rejection
                    //    audit payload so the dropped counter is observable.
                    let mut persistence_dropped = false;
                    let effective_instance: WorkflowInstance = if let Some(updates) =
                        err.slot_updates()
                    {
                        match merge_output(
                            &mut next.context,
                            transition.get("output"),
                            &arguments,
                            &next.input,
                            updates,
                        ) {
                            Ok(()) => {
                                next.version = instance.version + 1;
                                match self
                                    .store
                                    .save_if_version(next.clone(), request.expected_version)
                                    .await
                                {
                                    Ok(saved) => saved,
                                    Err(save_err) => {
                                        // All store backends signal a
                                        // version conflict with the stable
                                        // substring "stale workflow version".
                                        if save_err.to_string().contains("stale workflow version") {
                                            return Ok(DispatchOutcome::terminal(
                                                self.record_rejected(
                                                    &definition,
                                                    &instance,
                                                    "STALE_WORKFLOW_VERSION",
                                                    format!(
                                                        "Concurrent writer advanced workflow \
                                                             {} while persisting executor \
                                                             side-effects; expected version {}. \
                                                             Reload and retry.",
                                                        instance.id, request.expected_version
                                                    ),
                                                    &request.transition,
                                                    &correlation_id,
                                                    &request.principal,
                                                )
                                                .await,
                                            ));
                                        }
                                        tracing::warn!(
                                            target: "praxec_core::runtime",
                                            error = %save_err,
                                            workflow = %instance.id,
                                            "LlmWithUpdates persistence failed; counter \
                                             will not survive this turn"
                                        );
                                        persistence_dropped = true;
                                        instance.clone()
                                    }
                                }
                            }
                            Err(merge_err) => {
                                tracing::warn!(
                                    target: "praxec_core::runtime",
                                    error = %merge_err,
                                    workflow = %instance.id,
                                    "LlmWithUpdates merge into next.context failed; \
                                     counter will not survive this turn"
                                );
                                persistence_dropped = true;
                                instance.clone()
                            }
                        }
                    } else {
                        instance.clone()
                    };

                    self.audit
                        .record(
                            effective_instance
                                .audit_event("transition.rejected")
                                .with_correlation(&correlation_id)
                                .with_actor(&request.principal.subject)
                                .with_payload(json!({
                                    "transition": request.transition,
                                    "code": "EXECUTOR_FAILED",
                                    "errorClass": err.class().token(),
                                    "message": err.to_string(),
                                    // CMP-015 — surface dropped side-effect
                                    // persistence so a broken counter is
                                    // observable in the audit trail.
                                    "persistenceDropped": persistence_dropped,
                                })),
                        )
                        .await?;
                    return Ok(DispatchOutcome::terminal(
                        self.failed_response(
                            &definition,
                            &effective_instance,
                            &err,
                            &request.transition,
                            &request.principal,
                        )
                        .await,
                    ));
                }
            }
        } else {
            // A transition can carry `output:` writes WITHOUT an executor (e.g.
            // a round-counter increment on a pure deterministic gate). The
            // direct-submit path historically merged output ONLY inside the
            // executor branch, so an executor-less `output:` was silently
            // dropped. Apply it here, mirroring the executor path's merge +
            // typed-blackboard validation (no executor output → the mappings
            // resolve against context/arguments). Keeps parity with the chain
            // path so a gate behaves identically whether auto-chained or
            // submitted directly.
            merge_output(
                &mut next.context,
                transition.get("output"),
                &arguments,
                &next.input,
                &Value::Null,
            )?;
            if let Err((slot, reason)) =
                validate_blackboard_writes(&definition, transition.get("output"), &next.context)
            {
                return Ok(DispatchOutcome::terminal(
                    self.record_rejected(
                        &definition,
                        &instance,
                        "BLACKBOARD_TYPE_ERROR",
                        format!("output write to typed slot '{slot}': {reason}"),
                        &request.transition,
                        &correlation_id,
                        &request.principal,
                    )
                    .await,
                ));
            }
        }

        // SPEC §6.3 — write the optional model-authored summary to
        // `context.summary`. Reserved slot; never a guard input (`check`
        // errors on guards reading it); surfaced in every response.
        if let Some(summary) = &request.summary {
            if let Some(ctx) = next.context.as_object_mut() {
                ctx.insert("summary".into(), Value::String(summary.clone()));
            }
        }

        // SPEC §29 — increment per-state fire counter on successful fire.
        // The pre-check at the top of submit consults this counter to
        // enforce `max_fires_per_visit`. Stored in synthetic context
        // slot `_fire_count.<state>.<transition>`; scrubbed on state exit.
        if transition
            .get("max_fires_per_visit")
            .and_then(Value::as_u64)
            .is_some()
        {
            let key = format!("_fire_count.{}.{}", instance.state, request.transition);
            let current = crate::model::read_counter_slot(&next.context, &key)?;
            if let Some(ctx) = next.context.as_object_mut() {
                ctx.insert(key, json!(current.saturating_add(1)));
            }
        }

        // Pick the destination state. By default it's the transition's
        // `target`, but `branches: [{ when, target }]` can override based on
        // the executor's result and the post-output context. First branch
        // whose `when` guard passes wins; otherwise the declared target.
        let from_state = next.state.clone();
        let mut target = self
            .resolve_target(
                &transition,
                &next,
                &arguments,
                &request.principal,
                &correlation_id,
            )
            .await?;

        // SPEC §26 — `while: <guard>` loop. When the FROM state declares
        // a while-guard AND that guard evaluates truthy against the
        // post-transition context, re-route target to from_state so the
        // workflow re-enters the same state. Tracks iteration count in
        // the synthetic `_while_iter.<state>` context slot; resets when
        // we actually leave. `max_iterations` cap is REQUIRED on while:
        // and enforced here — exceeding it fails with WHILE_ITERATION_CAP_EXCEEDED.
        if let Some(rerouted) = self
            .apply_while_loop(
                &definition,
                &from_state,
                &target,
                &mut next,
                &arguments,
                &request.principal,
                &correlation_id,
            )
            .await?
        {
            target = rerouted;
        }

        // SPEC §27 — clear state-local slots when actually leaving the
        // state. No-op for self-loops / while: re-entry (target == from).
        self.clear_state_local_slots_on_exit(
            &definition,
            &from_state,
            &target,
            &mut next,
            &correlation_id,
            &request.principal,
        )
        .await;

        next.state = target;
        next.version += 1;

        // Record-first: emit the transition record BEFORE committing the
        // snapshot. The transition's declared `actor` (default `agent`) is the
        // record's actor; `deterministic`/`system` actors carry a null
        // principal, others carry the submitter's subject. If the record write
        // fails we abort here and never call `save_if_version`, so the
        // instance version stays unchanged.
        let actor = transition
            .get("actor")
            .and_then(Value::as_str)
            .unwrap_or("agent");
        let principal = if actor == "deterministic" || actor == "system" {
            None
        } else {
            Some(request.principal.subject.as_str())
        };
        let delta = blackboard_delta(&instance.context, &next.context);
        self.emit_transition_record(TransitionRecordParams {
            instance: &next,
            from_state: &from_state,
            transition_name: &request.transition,
            transition_def: &transition,
            actor,
            principal,
            arguments: &arguments,
            blackboard_delta: delta,
            guard_results,
            child_workflow_id,
            executor_outcome,
            correlation_id: &correlation_id,
        })
        .await?;

        let next = self
            .store
            .save_if_version(next, request.expected_version)
            .await?;

        // Persist accumulated evidence so subsequent `evidence` guards can
        // see it. Failures are logged but don't fail the transition — audit
        // is the ground truth for what happened.
        if let Some(estore) = &self.evidence {
            for ev in &accumulated_evidence {
                if let Err(e) = estore.record(&next.id, ev.clone()).await {
                    tracing::warn!(workflow = %next.id, error = %e, "evidence record failed");
                }
            }
        }

        let next = self
            .run_on_enter(definition.clone(), next, &correlation_id)
            .await?;

        self.audit
            .record(
                next.audit_event("workflow.transitioned")
                    .with_correlation(&correlation_id)
                    .with_actor(&request.principal.subject)
                    .with_payload(json!({
                        "transition": request.transition,
                        "state": next.state,
                        "version": next.version,
                    })),
            )
            .await?;

        // Run deterministic chain from the new state
        let max_depth = definition
            .get("maxChainDepth")
            .and_then(Value::as_u64)
            .unwrap_or(50);
        let chain_outcome = self
            .run_deterministic_chain(
                &definition,
                next,
                &request.principal,
                &correlation_id,
                max_depth,
            )
            .await?;

        match chain_outcome {
            ChainOutcome::Completed(result) => {
                if is_terminal(&definition, &result.instance.state) {
                    self.audit
                        .record(
                            result
                                .instance
                                .audit_event("workflow.completed")
                                .with_correlation(&correlation_id)
                                .with_payload(json!({ "state": result.instance.state })),
                        )
                        .await?;
                    self.emit_outcome_recorded(
                        StatusHint::Executed,
                        &definition,
                        &result.instance,
                        &correlation_id,
                        &request.principal,
                    )
                    .await;
                    // P2 (Task C) — this workflow just reached terminal and is
                    // committed. If it was spawned by a `kind: workflow`
                    // transition, re-drive the parent so it advances past its
                    // suspended sub-workflow transition.
                    self.resume_parent_if_any(&result.instance).await;
                }

                let mut response = self
                    .response(
                        &definition,
                        &result.instance,
                        StatusHint::Executed,
                        None,
                        &request.principal,
                    )
                    .await;
                // Merge evidence from submit + chain
                let mut all_evidence = accumulated_evidence;
                all_evidence.extend(result.evidence);
                if !all_evidence.is_empty() {
                    response["evidence"] = serde_json::to_value(&all_evidence)?;
                }
                if !result.steps.is_empty() {
                    response["chain"] = serde_json::to_value(&result.steps)?;
                }
                // SPEC §33 D3 — the only dispatch_once exit that may
                // carry a chain continuation. captured_next_transition
                // is populated iff the cycle's main executor succeeded
                // AND that executor returned a NextTransition.
                Ok(DispatchOutcome {
                    response,
                    next_transition: captured_next_transition,
                })
            }
            ChainOutcome::Failed {
                partial,
                error,
                error_class,
                failed_transition,
            } => {
                let mut response = self
                    .response(
                        &definition,
                        &partial.instance,
                        StatusHint::Failed,
                        Some(json!({
                            "code": "CHAIN_FAILED",
                            "message": error,
                            "errorClass": error_class,
                            "attemptedTransition": failed_transition,
                        })),
                        &request.principal,
                    )
                    .await;
                let mut all_evidence = accumulated_evidence;
                all_evidence.extend(partial.evidence);
                if !all_evidence.is_empty() {
                    response["evidence"] = serde_json::to_value(&all_evidence)?;
                }
                if !partial.steps.is_empty() {
                    response["chain"] = serde_json::to_value(&partial.steps)?;
                }
                // Include the failed deterministic transition in links for recovery
                push_failed_chain_recovery_link(
                    &mut response,
                    &definition,
                    &partial.instance,
                    &failed_transition,
                );
                // selection_error has no single failed transition — surface the
                // state's legal transitions so the caller can recover (no dead-end).
                if failed_transition.is_empty() {
                    push_state_recovery_links(&mut response, &definition, &partial.instance);
                }
                // SPEC §33 D3 — a failed deterministic chain leaves the
                // workflow in a broken state; do not chain further LLM
                // turns. The captured next_transition (if any) is
                // intentionally dropped here.
                Ok(DispatchOutcome::terminal(response))
            }
            ChainOutcome::Suspended {
                partial,
                suspend,
                transition,
            } => {
                // Chain path — a chain leaf signalled a step suspend (P2
                // sub-workflow or P12 agent await). Durably park the parent
                // and respond `waiting`, mirroring the direct-submit suspend
                // path. `partial.instance` already carries any context the
                // PRIOR chain steps committed, at its current version; the
                // suspend save bumps that version by exactly 1 and writes the
                // wait marker. A STALE_WORKFLOW_VERSION here is a genuine
                // rejection — propagate it via `?`, never fake a `waiting`
                // response.
                let resp = match suspend {
                    crate::model::StepSuspend::Subworkflow(s) => {
                        self.suspend_on_subworkflow(
                            &definition,
                            &partial.instance,
                            &transition,
                            partial.instance.version,
                            &request.principal,
                            s,
                        )
                        .await?
                    }
                    crate::model::StepSuspend::AgentAwait(a) => {
                        self.suspend_on_agent_await(
                            &definition,
                            &partial.instance,
                            &transition,
                            partial.instance.version,
                            &request.principal,
                            a,
                        )
                        .await?
                    }
                };
                Ok(DispatchOutcome::terminal(resp))
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::audit::{AuditSink, MemoryAuditSink};
    use crate::guards::DefaultGuardEvaluator;
    use crate::lock_scheduler::LockScheduler;
    use crate::model::{StartWorkflow, WorkflowInstance};
    use crate::ports::{Executor, ExecutorRegistry, WorkflowStore};
    use crate::repo_locks::{RepoLockSpace, RepoLocks};
    use crate::store::{ConfigDefinitionStore, InMemoryWorkflowStore};
    use async_trait::async_trait;
    use chrono::Utc;
    use serde_json::json;
    use std::sync::Arc;
    use std::time::Duration;

    struct EmptyRegistry;
    impl ExecutorRegistry for EmptyRegistry {
        fn get(&self, _kind: &str) -> Option<Arc<dyn Executor>> {
            None
        }
    }

    /// Store wrapper that delegates `create`/`load`/`list_waiting_on_lock` to
    /// an inner `InMemoryWorkflowStore` but makes EVERY `save_if_version` fail
    /// with a store error. Used to prove `suspend_on_lock` propagates a save
    /// failure instead of swallowing it (FIX C2).
    #[derive(Clone)]
    struct SaveFailingStore {
        inner: Arc<InMemoryWorkflowStore>,
    }

    #[async_trait]
    impl WorkflowStore for SaveFailingStore {
        async fn create(&self, instance: WorkflowInstance) -> anyhow::Result<WorkflowInstance> {
            self.inner.create(instance).await
        }
        async fn load(&self, workflow_id: &str) -> anyhow::Result<WorkflowInstance> {
            self.inner.load(workflow_id).await
        }
        async fn save_if_version(
            &self,
            _instance: WorkflowInstance,
            _expected_version: u64,
        ) -> anyhow::Result<WorkflowInstance> {
            anyhow::bail!("injected store I/O failure on save_if_version")
        }
        async fn list_waiting_on_lock(&self) -> anyhow::Result<Vec<WorkflowInstance>> {
            self.inner.list_waiting_on_lock().await
        }
        async fn list_waiting_on_subworkflow(&self) -> anyhow::Result<Vec<WorkflowInstance>> {
            self.inner.list_waiting_on_subworkflow().await
        }
        async fn list_all(&self) -> anyhow::Result<Vec<WorkflowInstance>> {
            self.inner.list_all().await
        }
        async fn find_by_run_id(&self, run_id: &str) -> anyhow::Result<Option<String>> {
            self.inner.find_by_run_id(run_id).await
        }
    }

    /// A definition whose initial state has an agent transition with a
    /// file-owning executor — so the runtime's acquire-gate engages.
    fn lock_owning_config() -> serde_json::Value {
        json!({
            "version": "1.0.0",
            "workflows": {
                "p": {
                    "version": "1.0.0",
                    "initialState": "a",
                    "states": {
                        "a": {
                            "transitions": {
                                "edit": {
                                    "target": "b",
                                    "actor": "agent",
                                    "executor": {
                                        "kind": "noop",
                                        "owned_files": ["src/lib.rs"]
                                    }
                                }
                            }
                        },
                        "b": { "terminal": true }
                    }
                }
            }
        })
    }

    /// FIX C2: when `suspend_on_lock`'s durable `save_if_version` fails, the
    /// whole submit MUST surface the error — never return a `waiting_on_lock`
    /// response while the `_lock_wait` record was silently dropped (a lie that
    /// strands the workflow across restart and bypasses the version bump).
    #[tokio::test]
    async fn suspend_on_lock_propagates_save_failure_instead_of_faking_suspend() {
        let cfg = lock_owning_config();
        let definitions = Arc::new(ConfigDefinitionStore::from_config(&cfg));
        let inner = Arc::new(InMemoryWorkflowStore::new());
        let store = Arc::new(SaveFailingStore {
            inner: inner.clone(),
        });
        let executors = Arc::new(EmptyRegistry);
        let guards = Arc::new(DefaultGuardEvaluator::new());
        let audit = Arc::new(MemoryAuditSink::new());
        let locks: Arc<dyn RepoLocks> = Arc::new(RepoLockSpace::new());
        // Pre-hold the file under a FOREIGN holder so the submit's acquire
        // hits a conflict and routes into suspend_on_lock.
        locks
            .acquire(
                &[std::path::PathBuf::from("src/lib.rs")],
                "wf:other",
                Duration::from_secs(300),
            )
            .await
            .unwrap();

        let runtime = WorkflowRuntime::new(
            definitions,
            store,
            executors,
            guards,
            audit as Arc<dyn AuditSink>,
        )
        .with_repo_locks(locks)
        .with_lock_scheduler(Arc::new(LockScheduler::new()));

        let start = runtime
            .start(StartWorkflow {
                definition_id: "p".into(),
                input: json!({}),
                principal: Principal::anonymous(),
                trace_id: None,
                run_id: None,
                depth: 0,
                parent: None,
            })
            .await
            .expect("start should succeed");
        let workflow_id = start["workflow"]["id"].as_str().unwrap().to_string();

        let result = runtime
            .submit(SubmitTransition {
                workflow_id: workflow_id.clone(),
                expected_version: 0,
                transition: "edit".into(),
                arguments: json!({}),
                principal: Principal::anonymous(),
                summary: None,
                trace_id: None,
                run_id: None,
            })
            .await;

        // The save failed: submit MUST error, not fake a suspend.
        assert!(
            result.is_err(),
            "submit must propagate the durable-save failure, got Ok: {result:?}"
        );

        // And the durable `_lock_wait` record must NOT have been committed —
        // the instance is still at its un-suspended version with no lock-wait
        // record (so recovery won't see a half-written suspend).
        let reloaded = inner.load(&workflow_id).await.unwrap();
        assert!(
            reloaded.context.get("_lock_wait").is_none(),
            "no _lock_wait should be persisted when the save failed"
        );
    }

    /// A stub `kind: workflow` executor that always signals a sub-workflow
    /// suspend — the parent's child is non-terminal, so dispatch_once must
    /// durably park the parent instead of advancing.
    struct SuspendingExecutor;
    #[async_trait]
    impl Executor for SuspendingExecutor {
        async fn execute(
            &self,
            _request: crate::model::ExecuteRequest,
        ) -> Result<crate::model::ExecuteResult, crate::error::ExecutorError> {
            Ok(crate::model::ExecuteResult {
                suspend: Some(crate::model::StepSuspend::Subworkflow(
                    crate::model::SubworkflowSuspend {
                        child_workflow_id: "child_x".into(),
                    },
                )),
                ..Default::default()
            })
        }
    }

    struct SingleExecutorRegistry {
        kind: &'static str,
        executor: Arc<dyn Executor>,
    }
    impl ExecutorRegistry for SingleExecutorRegistry {
        fn get(&self, kind: &str) -> Option<Arc<dyn Executor>> {
            if kind == self.kind {
                Some(self.executor.clone())
            } else {
                None
            }
        }
    }

    /// A one-transition definition whose initial state is `actor: human`
    /// (so a suspended response derives mission status `waiting`) and whose
    /// only transition dispatches a `kind: workflow` executor.
    fn subworkflow_config() -> serde_json::Value {
        json!({
            "version": "1.0.0",
            "workflows": {
                "p": {
                    "version": "1.0.0",
                    "initialState": "a",
                    "states": {
                        "a": {
                            "actor": "agent",
                            "transitions": {
                                "spawn": {
                                    "target": "b",
                                    "executor": { "kind": "workflow" }
                                }
                            }
                        },
                        "b": { "terminal": true }
                    }
                }
            }
        })
    }

    /// Task A — a `kind: workflow` executor whose child is non-terminal returns
    /// `ExecuteResult.suspend`; dispatch_once must durably park the parent
    /// (write `_subworkflow_wait`, bump version by 1, respond `waiting`) WITHOUT
    /// advancing to the transition target. Mirrors the `suspend_on_lock` path.
    #[tokio::test]
    async fn suspend_on_subworkflow_parks_parent_without_advancing() {
        let cfg = subworkflow_config();
        let definitions = Arc::new(ConfigDefinitionStore::from_config(&cfg));
        let store = Arc::new(InMemoryWorkflowStore::new());
        let executors = Arc::new(SingleExecutorRegistry {
            kind: "workflow",
            executor: Arc::new(SuspendingExecutor),
        });
        let guards = Arc::new(DefaultGuardEvaluator::new());
        let audit = Arc::new(MemoryAuditSink::new());

        let runtime = WorkflowRuntime::new(
            definitions,
            store.clone(),
            executors,
            guards,
            audit as Arc<dyn AuditSink>,
        );

        let start = runtime
            .start(StartWorkflow {
                definition_id: "p".into(),
                input: json!({}),
                principal: Principal::anonymous(),
                trace_id: None,
                run_id: None,
                depth: 0,
                parent: None,
            })
            .await
            .expect("start should succeed");
        let workflow_id = start["workflow"]["id"].as_str().unwrap().to_string();

        let response = runtime
            .submit(SubmitTransition {
                workflow_id: workflow_id.clone(),
                expected_version: 0,
                transition: "spawn".into(),
                arguments: json!({}),
                principal: Principal::anonymous(),
                summary: None,
                trace_id: None,
                run_id: None,
            })
            .await
            .expect("submit should succeed (parked, not errored)");

        // Mission status is `waiting` — the parent is parked on the child.
        assert_eq!(
            response["result"]["status"].as_str(),
            Some("waiting"),
            "suspended parent must report a waiting status, got: {response:#}"
        );
        // The instance did NOT advance to the transition target.
        assert_eq!(
            response["workflow"]["state"].as_str(),
            Some("a"),
            "parent must stay in its pre-transition state, not advance to 'b'"
        );

        // The durable `_subworkflow_wait` record is persisted with the child id.
        let reloaded = store.load(&workflow_id).await.unwrap();
        assert_eq!(
            reloaded.context["_subworkflow_wait"]["child_workflow_id"].as_str(),
            Some("child_x"),
            "the parked child id must be recorded for re-drive"
        );
        // The dispatched transition is pinned for Task C's re-drive to read.
        assert_eq!(
            reloaded.context["_subworkflow_wait"]["transition"].as_str(),
            Some("spawn"),
            "the parked transition must be recorded for re-drive"
        );
        // Version bumped by exactly 1 (started at 0).
        assert_eq!(
            reloaded.version, 1,
            "the suspend commit must bump the version by exactly 1"
        );
    }

    // ── P12 R1.4 — agent-await suspend: the SAME waiting representation ──

    /// A `kind: agent` stand-in that parks on `await_human` on its first
    /// dispatch (returns `StepSuspend::AgentAwait`) and completes with a real
    /// output when re-dispatched with `arguments.reply` (the resumed frame).
    struct AwaitingAgentExecutor;
    #[async_trait]
    impl Executor for AwaitingAgentExecutor {
        async fn execute(
            &self,
            request: crate::model::ExecuteRequest,
        ) -> Result<crate::model::ExecuteResult, crate::error::ExecutorError> {
            if request.arguments.get("reply").is_some() {
                return Ok(crate::model::ExecuteResult {
                    output: json!({ "verdict": "approved-and-done" }),
                    ..Default::default()
                });
            }
            Ok(crate::model::ExecuteResult {
                suspend: Some(crate::model::StepSuspend::AgentAwait(
                    crate::model::AgentAwaitSuspend {
                        correlation_id: "corr-7".into(),
                        prompt: "ship it?".into(),
                    },
                )),
                ..Default::default()
            })
        }
    }

    fn agent_await_config() -> serde_json::Value {
        json!({
            "version": "1.0.0",
            "workflows": {
                "p": {
                    "version": "1.0.0",
                    "initialState": "a",
                    "states": {
                        "a": {
                            "actor": "agent",
                            "transitions": {
                                "do_work": {
                                    "target": "b",
                                    "executor": { "kind": "agent" }
                                }
                            }
                        },
                        "b": { "terminal": true }
                    }
                }
            }
        })
    }

    fn agent_await_runtime(store: Arc<InMemoryWorkflowStore>) -> WorkflowRuntime {
        WorkflowRuntime::new(
            Arc::new(ConfigDefinitionStore::from_config(&agent_await_config())),
            store,
            Arc::new(SingleExecutorRegistry {
                kind: "agent",
                executor: Arc::new(AwaitingAgentExecutor),
            }),
            Arc::new(DefaultGuardEvaluator::new()),
            Arc::new(MemoryAuditSink::new()) as Arc<dyn AuditSink>,
        )
    }

    async fn start_and_park(runtime: &WorkflowRuntime) -> (String, Value) {
        let start = runtime
            .start(StartWorkflow {
                definition_id: "p".into(),
                input: json!({}),
                principal: Principal::anonymous(),
                trace_id: None,
                run_id: None,
                depth: 0,
                parent: None,
            })
            .await
            .expect("start should succeed");
        let workflow_id = start["workflow"]["id"].as_str().unwrap().to_string();
        let parked = runtime
            .submit(SubmitTransition {
                workflow_id: workflow_id.clone(),
                expected_version: 0,
                transition: "do_work".into(),
                arguments: json!({}),
                principal: Principal::anonymous(),
                summary: None,
                trace_id: None,
                run_id: None,
            })
            .await
            .expect("submit should succeed (parked, not errored)");
        (workflow_id, parked)
    }

    /// P12 R1.4 — an agent `Suspended` outcome parks the mission exactly like
    /// a workflow-level gate: the response is `waiting` (NOT failed), the
    /// state does not advance, the durable `_agent_await` marker carries the
    /// correlation_id, and the version bumps by exactly 1. Mirrors
    /// `suspend_on_subworkflow_parks_parent_without_advancing` — same waiting
    /// representation, different resume signal.
    #[tokio::test]
    async fn agent_await_suspend_parks_mission_waiting_without_advancing() {
        let store = Arc::new(InMemoryWorkflowStore::new());
        let runtime = agent_await_runtime(store.clone());
        let (workflow_id, response) = start_and_park(&runtime).await;

        assert_eq!(
            response["result"]["status"].as_str(),
            Some("waiting"),
            "a suspended agent must park the mission waiting, got: {response:#}"
        );
        assert_eq!(
            response["workflow"]["state"].as_str(),
            Some("a"),
            "the workflow must stay in its pre-transition state"
        );
        // The pending question + resume handle are surfaced on the response.
        assert_eq!(
            response["await"]["correlationId"].as_str(),
            Some("corr-7"),
            "the waiting response must carry the resume correlation_id"
        );
        assert_eq!(response["await"]["prompt"].as_str(), Some("ship it?"));

        // The durable `_agent_await` marker is persisted for the resume.
        let reloaded = store.load(&workflow_id).await.unwrap();
        assert_eq!(
            reloaded.context["_agent_await"]["correlation_id"].as_str(),
            Some("corr-7")
        );
        assert_eq!(
            reloaded.context["_agent_await"]["transition"].as_str(),
            Some("do_work")
        );
        assert_eq!(
            reloaded.version, 1,
            "the suspend commit must bump the version by exactly 1"
        );
    }

    /// P12 R1.4 origin gate (mirrors P16): once a transition is parked on an
    /// `_agent_await`, a NON-human principal may not re-submit it — an LLM /
    /// auto-drive re-fire is rejected typed instead of resolving (or even
    /// touching) the human gate.
    #[tokio::test]
    async fn a_non_human_cannot_resume_a_parked_agent_await() {
        let store = Arc::new(InMemoryWorkflowStore::new());
        let runtime = agent_await_runtime(store.clone());
        let (workflow_id, _) = start_and_park(&runtime).await;

        let rejected = runtime
            .submit(SubmitTransition {
                workflow_id: workflow_id.clone(),
                expected_version: 1,
                transition: "do_work".into(),
                arguments: json!({ "reply": "sneaky LLM approval" }),
                principal: Principal::anonymous(),
                summary: None,
                trace_id: None,
                run_id: None,
            })
            .await
            .expect("the rejection is a typed response, not an Err");
        assert_eq!(
            rejected["error"]["code"].as_str(),
            Some("AWAIT_RESUME_NOT_HUMAN"),
            "got: {rejected:#}"
        );
        // The gate is intact: still parked, still waiting on the same frame.
        let reloaded = store.load(&workflow_id).await.unwrap();
        assert_eq!(
            reloaded.context["_agent_await"]["correlation_id"].as_str(),
            Some("corr-7"),
            "a rejected non-human resume must not consume the parked frame"
        );
        assert_eq!(reloaded.state, "a");
    }

    /// P12 R1.4 — the resume round-trip: a HUMAN principal re-submits the
    /// parked transition with `arguments.reply`; the executor's resumed result
    /// merges, the transition advances, and the consumed `_agent_await`
    /// marker is cleared so a later fire starts fresh.
    #[tokio::test]
    async fn a_human_reply_resumes_advances_and_clears_the_await_marker() {
        let store = Arc::new(InMemoryWorkflowStore::new());
        let runtime = agent_await_runtime(store.clone());
        let (workflow_id, _) = start_and_park(&runtime).await;

        let human = Principal {
            subject: "matt".into(),
            roles: vec![Principal::HUMAN_ROLE.into()],
            permissions: vec![],
        };
        let resumed = runtime
            .submit(SubmitTransition {
                workflow_id: workflow_id.clone(),
                expected_version: 1,
                transition: "do_work".into(),
                arguments: json!({ "reply": "yes — ship it" }),
                principal: human,
                summary: None,
                trace_id: None,
                run_id: None,
            })
            .await
            .expect("a human resume submit succeeds");
        assert_eq!(
            resumed["workflow"]["state"].as_str(),
            Some("b"),
            "the resumed step must advance to its target, got: {resumed:#}"
        );

        let reloaded = store.load(&workflow_id).await.unwrap();
        assert!(
            reloaded.context.get("_agent_await").is_none(),
            "the consumed await marker must be cleared on advance: {:#}",
            reloaded.context
        );
    }

    /// A definition whose INITIAL state fires a deterministic chain transition
    /// (`actor: deterministic`) whose executor is `kind: workflow`. The chain
    /// runs on `start`. The transition declares an `output` write into a typed
    /// blackboard slot so we can prove no null-through merge happens when the
    /// child suspends. State `b` is terminal (the "downstream slot").
    fn chain_subworkflow_config() -> serde_json::Value {
        json!({
            "version": "1.0.0",
            "workflows": {
                "p": {
                    "version": "1.0.0",
                    "initialState": "a",
                    "blackboard": {
                        "child_result": { "type": "object" }
                    },
                    "states": {
                        "a": {
                            "actor": "agent",
                            "transitions": {
                                "chain_spawn": {
                                    "actor": "deterministic",
                                    "target": "b",
                                    "executor": { "kind": "workflow" },
                                    "output": { "child_result": "$.output" }
                                }
                            }
                        },
                        "b": { "terminal": true }
                    }
                }
            }
        })
    }

    /// Task A (chain path) — a `kind: workflow` executor reached via an
    /// `actor: deterministic` CHAIN transition (on `start`) whose child is
    /// non-terminal returns `ExecuteResult.suspend`. The deterministic chain
    /// MUST durably park the parent (write `_subworkflow_wait`, bump version by
    /// exactly 1, respond `waiting`) WITHOUT advancing to the transition target
    /// and WITHOUT merging the child's (null) output into the downstream slot.
    /// This mirrors `suspend_on_subworkflow_parks_parent_without_advancing` but
    /// exercises the chain loop instead of the direct-submit path.
    #[tokio::test]
    async fn chain_honors_subworkflow_suspend_parks_parent() {
        let cfg = chain_subworkflow_config();
        let definitions = Arc::new(ConfigDefinitionStore::from_config(&cfg));
        let store = Arc::new(InMemoryWorkflowStore::new());
        let executors = Arc::new(SingleExecutorRegistry {
            kind: "workflow",
            executor: Arc::new(SuspendingExecutor),
        });
        let guards = Arc::new(DefaultGuardEvaluator::new());
        let audit = Arc::new(MemoryAuditSink::new());

        let runtime = WorkflowRuntime::new(
            definitions,
            store.clone(),
            executors,
            guards,
            audit as Arc<dyn AuditSink>,
        );

        // The deterministic chain fires during `start`.
        let start = runtime
            .start(StartWorkflow {
                definition_id: "p".into(),
                input: json!({}),
                principal: Principal::anonymous(),
                trace_id: None,
                run_id: None,
                depth: 0,
                parent: None,
            })
            .await
            .expect("start should succeed (parked, not errored)");

        let workflow_id = start["workflow"]["id"].as_str().unwrap().to_string();

        // Mission status is `waiting` — the parent is parked on the child.
        assert_eq!(
            start["result"]["status"].as_str(),
            Some("waiting"),
            "chain-suspended parent must report a waiting status, got: {start:#}"
        );
        // The instance did NOT advance to the transition target.
        assert_eq!(
            start["workflow"]["state"].as_str(),
            Some("a"),
            "parent must stay in its pre-transition state, not advance to 'b'"
        );

        let reloaded = store.load(&workflow_id).await.unwrap();
        // The durable `_subworkflow_wait` record is persisted with the child id.
        assert_eq!(
            reloaded.context["_subworkflow_wait"]["child_workflow_id"].as_str(),
            Some("child_x"),
            "the parked child id must be recorded for re-drive"
        );
        // The dispatched (chain) transition is pinned for Task C's re-drive.
        assert_eq!(
            reloaded.context["_subworkflow_wait"]["transition"].as_str(),
            Some("chain_spawn"),
            "the parked transition must be recorded for re-drive"
        );
        // No null-through: the downstream slot was NOT merged with the child's
        // (null) output. The parent never advanced, so the slot is absent.
        assert!(
            reloaded.context.get("child_result").is_none(),
            "no null output may be merged into the downstream slot when the \
             child suspends, got context: {:#}",
            reloaded.context
        );
        // State on the persisted instance also stayed at the suspending leaf.
        assert_eq!(
            reloaded.state, "a",
            "persisted parent state must stay at the suspending leaf"
        );
        // Version bumped by exactly 1 (created at 0).
        assert_eq!(
            reloaded.version, 1,
            "the suspend commit must bump the version by exactly 1"
        );
    }

    fn instance_with_lock_wait(id: &str, lock_wait: serde_json::Value) -> WorkflowInstance {
        WorkflowInstance {
            id: id.to_string(),
            definition_id: "p".into(),
            definition_version: "1.0.0".into(),
            definition: json!({}),
            state: "a".into(),
            version: 1,
            input: json!({}),
            context: json!({ "_lock_wait": lock_wait }),
            started_at: Utc::now(),
            trace_id: None,
            run_id: None,
            cancelled_at: None,
            cancelled_reason: None,
            depth: 0,
            parent: None,
        }
    }

    /// FIX recover: a `_lock_wait` record with an empty `transition` (or empty
    /// `files`) is corrupt; recovery must SKIP it rather than enqueue a doomed
    /// redrive. Here we seed three instances — one healthy, one with empty
    /// transition, one with empty files — and assert only the healthy one is
    /// enqueued.
    #[tokio::test]
    async fn recover_suspended_locks_skips_corrupt_records() {
        let cfg = lock_owning_config();
        let definitions = Arc::new(ConfigDefinitionStore::from_config(&cfg));
        let store = Arc::new(InMemoryWorkflowStore::new());
        store
            .create(instance_with_lock_wait(
                "wf_healthy",
                json!({ "files": ["src/lib.rs"], "transition": "edit" }),
            ))
            .await
            .unwrap();
        store
            .create(instance_with_lock_wait(
                "wf_empty_transition",
                json!({ "files": ["src/lib.rs"], "transition": "" }),
            ))
            .await
            .unwrap();
        store
            .create(instance_with_lock_wait(
                "wf_empty_files",
                json!({ "files": [], "transition": "edit" }),
            ))
            .await
            .unwrap();

        let executors = Arc::new(EmptyRegistry);
        let guards = Arc::new(DefaultGuardEvaluator::new());
        let audit = Arc::new(MemoryAuditSink::new());
        let sched = Arc::new(LockScheduler::new());
        let runtime = WorkflowRuntime::new(
            definitions,
            store,
            executors,
            guards,
            audit as Arc<dyn AuditSink>,
        )
        .with_repo_locks(Arc::new(RepoLockSpace::new()))
        .with_lock_scheduler(sched.clone());

        runtime.recover_suspended_locks().await;

        // Only the healthy instance should have been enqueued; the two corrupt
        // records are skipped.
        assert_eq!(
            sched.len().await,
            1,
            "only the healthy _lock_wait record should be re-enqueued"
        );
    }
}
