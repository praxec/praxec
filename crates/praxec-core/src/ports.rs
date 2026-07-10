use std::sync::Arc;

use async_trait::async_trait;
use serde_json::Value;

use crate::error::ExecutorError;
use crate::model::*;
use crate::plan::{
    CallerId, Cohort, DeliverableStatus, PlanGraph, PlanId, PlanStatus, PlannerError,
};

#[async_trait]
pub trait DefinitionStore: Send + Sync {
    async fn load(&self, definition_id: &str) -> anyhow::Result<Value>;
}

/// SPEC §8.4 — opt-in writable extension of `DefinitionStore`. Implementations
/// are constructed only when `praxec.authoring.write_enabled` is true at
/// gateway startup. Runtime call sites hold this as
/// `Option<Arc<dyn DefinitionStoreWritable>>` — `None` means the write path
/// is disabled and the registry executor fails fast with `WRITE_DISABLED`.
///
/// The implementation MUST emit `definition.published` to the audit sink
/// BEFORE the new snapshot becomes loadable (audit-before-commit), mirroring
/// SPEC §7.3 record-first ordering for transition records. Audit failure
/// MUST abort the commit and return an error containing `RECORD_WRITE_FAILED`
/// in its display.
///
/// `expected_prior_hash` is the optimistic-concurrency precondition for
/// **edit-publish** (SPEC §8.4): `None` creates-or-overwrites unconditionally;
/// `Some(hash)` requires the current definition to hash (via
/// [`crate::config::compute_definition_hash`]) to exactly that value, else the
/// write is rejected with `CONFLICT_STALE` — the snapshot the author edited
/// moved under them. There is no lenient fallback: a store that can't honor the
/// precondition MUST reject, never silently overwrite.
#[async_trait]
pub trait DefinitionStoreWritable: DefinitionStore {
    async fn register(
        &self,
        definition_id: &str,
        definition: Value,
        expected_prior_hash: Option<&str>,
    ) -> anyhow::Result<()>;
}

#[async_trait]
pub trait WorkflowStore: Send + Sync {
    async fn create(&self, instance: WorkflowInstance) -> anyhow::Result<WorkflowInstance>;
    async fn load(&self, workflow_id: &str) -> anyhow::Result<WorkflowInstance>;
    async fn save_if_version(
        &self,
        instance: WorkflowInstance,
        expected_version: u64,
    ) -> anyhow::Result<WorkflowInstance>;

    /// SPEC §32 — find an in-flight workflow instance by `run_id`.
    ///
    /// Returns `Ok(Some(workflow_id))` if an instance with that `run_id`
    /// exists, `Ok(None)` otherwise. This is correctness-critical: it backs
    /// the duplicate-run guard. It is a REQUIRED method — there is no lenient
    /// default, because a backend that silently returned `Ok(None)` would
    /// disable the uniqueness assertion without any signal. Every backend MUST
    /// index `run_id` (or scan) and answer truthfully.
    async fn find_by_run_id(&self, run_id: &str) -> anyhow::Result<Option<String>>;

    /// List instances currently suspended on a repo lock (their context holds
    /// a `_lock_wait` record). Used at startup to re-register suspended
    /// workflows into the `LockScheduler`. REQUIRED — no lenient default: a
    /// backend silently returning an empty list would drop lock-resume on
    /// restart (suspended workflows would never resume on release). Every
    /// backend MUST scan and answer truthfully.
    async fn list_waiting_on_lock(&self) -> anyhow::Result<Vec<WorkflowInstance>>;

    /// List instances currently suspended on a sub-workflow (their context
    /// holds a `_subworkflow_wait` record). Used at startup to re-drive parents
    /// whose child terminated during a gateway restart. REQUIRED — no lenient
    /// default: a backend silently returning an empty list would drop
    /// sub-workflow resume on restart (a parent whose child finished during
    /// downtime would stay `waiting` forever). Every backend MUST scan and
    /// answer truthfully.
    async fn list_waiting_on_subworkflow(&self) -> anyhow::Result<Vec<WorkflowInstance>>;
}

#[async_trait]
pub trait Executor: Send + Sync {
    async fn execute(&self, request: ExecuteRequest) -> Result<ExecuteResult, ExecutorError>;
}

pub trait ExecutorRegistry: Send + Sync {
    fn get(&self, kind: &str) -> Option<Arc<dyn Executor>>;
}

/// SPEC §33 D2 — the executor for `kind: llm` builds its per-turn tool
/// list by asking the runtime for the transitions currently available
/// at the workflow's state, with guards already filtered.
///
/// The runtime implements this by wrapping `runtime_links::links` +
/// `runtime_response::filter_links_by_guards`, then handing the executor
/// a guard-filtered list it can translate one-to-one into provider tool
/// definitions. State-aware tool narrowing is enforced: a transition the
/// model isn't allowed to take at this state is simply absent from the
/// list.
///
/// Returned shape mirrors `runtime_response::response().links` — each
/// entry is a JSON object carrying at least `rel`, optionally `title`,
/// `inputSchema`, and `actor`. The executor consumes them; if any are
/// malformed the runtime impl is responsible (executors fail-fast with
/// `ExecutorError::Other` if they can't map a link to a tool).
#[async_trait]
pub trait TransitionResolver: Send + Sync {
    async fn available_transitions(
        &self,
        instance: &WorkflowInstance,
        principal: &Principal,
    ) -> anyhow::Result<Vec<Value>>;
}

#[async_trait]
pub trait GuardEvaluator: Send + Sync {
    async fn evaluate(
        &self,
        guard: &Value,
        instance: &WorkflowInstance,
        arguments: &Value,
        principal: &Principal,
    ) -> anyhow::Result<bool>;

    /// SPEC §20.1 + §20.4 — when a guard rejects for a specific named
    /// reason (e.g. `EVIDENCE_DIGEST_REQUIRED`,
    /// `EVIDENCE_CONFIDENCE_BELOW_THRESHOLD`), implementers return the code
    /// alongside the pass/fail bool so the caller can surface the precise
    /// rejection in `error.code` instead of generic `GUARD_REJECTED`.
    ///
    /// Default impl delegates to `evaluate` and returns `None` for the
    /// diagnostic — preserves backward compat for any external implementer.
    async fn evaluate_with_diagnostic(
        &self,
        guard: &Value,
        instance: &WorkflowInstance,
        arguments: &Value,
        principal: &Principal,
    ) -> anyhow::Result<(bool, Option<String>)> {
        let pass = self.evaluate(guard, instance, arguments, principal).await?;
        Ok((pass, None))
    }
}

#[async_trait]
pub trait EvidenceStore: Send + Sync {
    /// Append a new evidence record for the given workflow.
    async fn record(&self, workflow_id: &str, evidence: Evidence) -> anyhow::Result<()>;

    /// Return every recorded evidence item for a workflow.
    async fn list(&self, workflow_id: &str) -> anyhow::Result<Vec<Evidence>>;
}

/// P12 R1.4 (`docs/await-resume-architecture.md`) — durable storage for
/// parked agent tool-loop sessions: the agent-loop half of the await/resume
/// primitive. The agent runner `park`s a [`ParkedAgentSession`] when a session
/// hits its suspend signal, and `load`s + `remove`s it on a correlated resume.
/// The production backend is SQLite (the design mandates the sqlite
/// governance store, not a file — a parked frame must survive a power cycle).
#[async_trait]
pub trait ParkedSessionStore: Send + Sync {
    /// Persist a newly parked session. The `correlation_id` MUST be fresh —
    /// parking twice under one id is an **error**, never a silent overwrite
    /// (an overwrite would destroy a live parked frame).
    async fn park(&self, record: ParkedAgentSession) -> anyhow::Result<()>;

    /// Load a parked session. `Ok(None)` when the id is unknown (already
    /// resumed / never parked) so the caller can answer with its own typed
    /// error; `Err` only for storage failure or a corrupt row.
    async fn load(&self, correlation_id: &str) -> anyhow::Result<Option<ParkedAgentSession>>;

    /// Remove a parked session — after its frame terminally completes, or
    /// when it is superseded by a new suspension of the resumed frame.
    async fn remove(&self, correlation_id: &str) -> anyhow::Result<()>;

    /// Every parked session, oldest first — the durable "pending awaits"
    /// inbox a human drains later when running headless.
    async fn list(&self) -> anyhow::Result<Vec<ParkedAgentSession>>;
}

/// SPEC §5.9 — tracks `gateway.describe` calls per workflow + subject so the
/// `guidance_acknowledged` guard (§17.4) can verify that the body was
/// fetched AND that the fetched body's hash still matches the current
/// definition snapshot. Hash-flip invalidation is the TRIZ-bounded
/// semantic teeth (FMECA FM-4): we can't prove the LLM *read* the body,
/// but we can prove it fetched the *current* one.
#[async_trait]
pub trait GuidanceAcknowledgmentStore: Send + Sync {
    /// Record that `subject` was fetched for `workflow_id` while the body's
    /// normalized hash was `body_hash`.
    async fn record(&self, workflow_id: &str, subject: &str, body_hash: &str)
    -> anyhow::Result<()>;

    /// Return the hash of the body last fetched for `(workflow_id, subject)`,
    /// or `None` if no fetch was recorded.
    async fn last_acknowledged_hash(
        &self,
        workflow_id: &str,
        subject: &str,
    ) -> anyhow::Result<Option<String>>;
}

/// SPEC §22 — same shape as [`GuidanceAcknowledgmentStore`] but tracks
/// SCRIPT subject acknowledgments separately. Distinct trait so a
/// gateway can wire one without the other (e.g. authoring-time gateway
/// gets both; runtime gateway gets only the script one for destructive-
/// script guards). Hash-flip invalidation is the same semantic.
#[async_trait]
pub trait ScriptAcknowledgmentStore: Send + Sync {
    async fn record(&self, workflow_id: &str, subject: &str, body_hash: &str)
    -> anyhow::Result<()>;

    async fn last_acknowledged_hash(
        &self,
        workflow_id: &str,
        subject: &str,
    ) -> anyhow::Result<Option<String>>;
}

/// SPEC §33 PA1 — lock-aware planner.
///
/// The Planner is the IP boundary between Praxec's runtime and the
/// scheduling implementation. The runtime holds an `Arc<dyn Planner>`;
/// operators register whichever implementation suits their deployment.
///
/// # Implementations
///
/// - Open source: the standalone `cpm-planner` crate (separate repo) is a
///   textbook critical-path-method implementation, consumed over MCP.
/// - Closed source: a richer FrontRails implementation lives in a separate
///   workspace and links the published `praxec-core` crate.
///
/// # Semantics
///
/// - [`Planner::submit_plan`] is idempotent on a deterministic hash of
///   `(graph, caller)`. Resubmitting an identical plan returns the existing
///   [`PlanId`] instead of creating a duplicate.
/// - [`Planner::acquire_cohort`] returns up to `max_count` deliverables that
///   are simultaneously:
///   (a) all prerequisites complete,
///   (b) file sets mutually disjoint within the returned cohort,
///   (c) file sets disjoint from every currently held lock.
///   The implementation MUST lock the returned deliverables atomically; the
///   trait contract is that this is what the call promises (PA3 enforces).
/// - [`Planner::mark_status`] with [`DeliverableStatus::Complete`] or
///   [`DeliverableStatus::Failed`] releases the lock. If the supplied
///   `caller_id` does not match the lock holder, the call returns
///   [`PlannerError::LockNotHeld`].
/// - [`Planner::heartbeat`] refreshes the TTL on a held lock. TTL itself is
///   implementation-defined; PA3 sets the open-source default to 5 minutes.
/// - [`Planner::status`] is a cheap read-only snapshot and is safe to poll.
/// - [`Planner::force_release`] is the operator escape hatch. Implementations
///   MUST emit an audit event carrying the supplied `reason`.
#[async_trait]
pub trait Planner: Send + Sync {
    /// Submit a [`PlanGraph`]. Idempotent on `(graph, caller_id)`; an
    /// identical resubmission returns the existing [`PlanId`].
    async fn submit_plan(&self, graph: PlanGraph) -> Result<PlanId, PlannerError>;

    /// Acquire up to `max_count` deliverables that are ready to run *and*
    /// have mutually disjoint `owned_files` (within the cohort and against
    /// all currently held locks). The returned [`Cohort`] carries one
    /// [`crate::plan::LockInfo`] per acquired deliverable, in the same
    /// order.
    async fn acquire_cohort(
        &self,
        plan_id: &PlanId,
        caller_id: &CallerId,
        max_count: usize,
    ) -> Result<Cohort, PlannerError>;

    /// Update the lifecycle state of a deliverable. Setting `Complete` or
    /// `Failed` releases the lock; `caller_id` MUST be the lock holder or
    /// the call is rejected with [`PlannerError::LockNotHeld`].
    async fn mark_status(
        &self,
        plan_id: &PlanId,
        deliverable_id: &str,
        caller_id: &CallerId,
        status: DeliverableStatus,
    ) -> Result<(), PlannerError>;

    /// Refresh the TTL on a held lock. Rejected with
    /// [`PlannerError::LockNotHeld`] if `caller_id` is not the holder,
    /// or with [`PlannerError::LockExpired`] if the lock already lapsed.
    async fn heartbeat(
        &self,
        plan_id: &PlanId,
        deliverable_id: &str,
        caller_id: &CallerId,
    ) -> Result<(), PlannerError>;

    /// Cheap read-only snapshot. Safe to poll on a timer.
    async fn status(&self, plan_id: &PlanId) -> Result<PlanStatus, PlannerError>;

    /// Operator escape hatch: forcibly release a lock regardless of holder
    /// or TTL. Implementations MUST emit an audit event carrying `reason`.
    async fn force_release(
        &self,
        plan_id: &PlanId,
        deliverable_id: &str,
        reason: &str,
    ) -> Result<(), PlannerError>;
}

#[cfg(test)]
mod tests {
    use super::*;

    struct StubStore;

    #[async_trait]
    impl WorkflowStore for StubStore {
        async fn create(&self, _instance: WorkflowInstance) -> anyhow::Result<WorkflowInstance> {
            unimplemented!()
        }
        async fn load(&self, _workflow_id: &str) -> anyhow::Result<WorkflowInstance> {
            unimplemented!()
        }
        async fn save_if_version(
            &self,
            _instance: WorkflowInstance,
            _expected_version: u64,
        ) -> anyhow::Result<WorkflowInstance> {
            unimplemented!()
        }
        // find_by_run_id and list_waiting_on_lock are now REQUIRED methods
        // (no lenient default). The stub answers truthfully for its empty state.
        async fn find_by_run_id(&self, _run_id: &str) -> anyhow::Result<Option<String>> {
            Ok(None)
        }
        async fn list_waiting_on_lock(&self) -> anyhow::Result<Vec<WorkflowInstance>> {
            Ok(Vec::new())
        }
        async fn list_waiting_on_subworkflow(&self) -> anyhow::Result<Vec<WorkflowInstance>> {
            Ok(Vec::new())
        }
    }

    #[tokio::test]
    async fn find_by_run_id_is_required_and_stub_reports_none() {
        let s = StubStore;
        let result = s.find_by_run_id("r-test").await.unwrap();
        assert_eq!(result, None);
    }
}
