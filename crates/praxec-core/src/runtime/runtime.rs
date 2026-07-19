use std::collections::HashSet;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, RwLock};

use anyhow::bail;
use chrono::Utc;
use serde_json::{Value, json};
use uuid::Uuid;

use serde::Serialize;

use crate::audit::AuditSink;
use crate::error::RuntimeError;
use crate::mission::StatusHint;
use crate::model::*;
use crate::ports::*;
pub use crate::runtime::runtime_links::is_terminal;
pub(crate) use crate::runtime::runtime_links::{
    pointer_escape, push_failed_chain_recovery_link, push_state_recovery_links,
    transition_definition,
};
pub(crate) use crate::runtime::runtime_schema::{
    apply_schema_defaults, required_str, validate_schema,
};

/// SPEC §33 D3 — default chain-depth cap for LLM-driven submit chains.
/// Configurable per-runtime via [`WorkflowRuntime::with_max_chained_llm_turns`].
pub const DEFAULT_MAX_CHAINED_LLM_TURNS: u32 = 32;

// ---------------------------------------------------------------------------
// Deterministic chaining types
// ---------------------------------------------------------------------------

/// One step in a deterministic chain, recording the state traversal.
#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ChainStep {
    pub from_state: String,
    pub transition: String,
    pub to_state: String,
    pub version: u64,
}

/// Outcome of a deterministic chain run.
pub enum ChainOutcome {
    /// Chain completed normally: reached a decision point (non-deterministic
    /// state), terminal state, or depth limit.
    Completed(ChainResult),
    /// Chain stopped because an executor failed or no viable deterministic
    /// transition could be selected.
    Failed {
        partial: ChainResult,
        error: String,
        error_class: String,
        failed_transition: String,
    },
    /// Chain stopped because an executor signalled a step suspend — a
    /// `kind: workflow` child is non-terminal (P2) or a `kind: agent` session
    /// parked on `await_human` (P12 R1.4). The parent must be durably parked
    /// WITHOUT merging the step's (absent) output or advancing the
    /// transition. `partial.instance` carries the context accumulated by
    /// PRIOR chain steps, committed at its current version.
    Suspended {
        partial: ChainResult,
        suspend: crate::model::StepSuspend,
        transition: String,
    },
    /// Chain stopped because an auto-driven leaf could not acquire its declared
    /// `owned_files` — another holder owns the resource.
    ///
    /// Deliberately DISTINCT from [`ChainOutcome::Suspended`]: that means an
    /// executor ran and parked itself; this means the leaf never started. And
    /// distinct from [`ChainOutcome::Failed`]: contention is the expected steady
    /// state under fan-out, not an error. The caller durably records the wait
    /// and enqueues on the `LockScheduler`, so the run resumes when the resource
    /// frees rather than proceeding on a resource it does not hold.
    WaitingOnLock {
        partial: ChainResult,
        transition: String,
        files: Vec<std::path::PathBuf>,
        conflict: crate::repo_locks::LockConflict,
    },
    /// Chain stopped because the run exceeded its cumulative livelock hop
    /// budget (persisted across drives AND restarts) without reaching a
    /// terminal state — a livelock. Distinct from `Failed`: this is a liveness
    /// backstop, not an executor error, and the caller QUARANTINES the run
    /// (cancels it) so it cannot re-drive and burn the model indefinitely.
    Quarantined {
        partial: ChainResult,
        reason: String,
    },
}

/// Accumulated state from a deterministic chain.
pub struct ChainResult {
    pub instance: WorkflowInstance,
    pub steps: Vec<ChainStep>,
    pub evidence: Vec<Evidence>,
}

/// SPEC §7.2 — parameter bundle for `emit_transition_record`. Collected into
/// a struct so the caller doesn't shuffle ~12 positional arguments. Borrows
/// the live instance + correlation id so the helper is a pure projection
/// over the commit context.
pub(crate) struct TransitionRecordParams<'a> {
    pub(crate) instance: &'a WorkflowInstance,
    pub(crate) from_state: &'a str,
    pub(crate) transition_name: &'a str,
    pub(crate) transition_def: &'a Value,
    pub(crate) actor: &'a str,
    pub(crate) principal: Option<&'a str>,
    pub(crate) arguments: &'a Value,
    pub(crate) blackboard_delta: Value,
    pub(crate) guard_results: Vec<Value>,
    pub(crate) child_workflow_id: Option<String>,
    /// `Some((ok, durationMs))` only when the executor actually ran on this
    /// transition. `None` for transitions without an `executor:` and for
    /// `onTimeout` records.
    pub(crate) executor_outcome: Option<(bool, u64)>,
    pub(crate) correlation_id: &'a str,
}

/// The workflow runtime. Holds Arcs of all ports so it can be cloned cheaply
/// and embedded in tool handlers.
#[derive(Clone)]
pub struct WorkflowRuntime {
    pub(crate) definitions: Arc<dyn DefinitionStore>,
    pub(crate) store: Arc<dyn WorkflowStore>,
    pub(crate) executors: Arc<dyn ExecutorRegistry>,
    pub(crate) guards: Arc<dyn GuardEvaluator>,
    pub(crate) audit: Arc<dyn AuditSink>,
    pub(crate) evidence: Option<Arc<dyn EvidenceStore>>,
    /// Global repository write-exclusion locks. When wired, the runtime
    /// acquires a file-owning transition's `owned_files` before executing it
    /// (durably suspending on contention) and releases after. `None` → the
    /// acquire-gate is a no-op (no locking).
    pub(crate) repo_locks: Option<Arc<dyn crate::repo_locks::RepoLocks>>,
    /// FIFO wait-queue for workflows suspended on repo locks; re-driven on
    /// release. `None` → no auto-resume (a suspended workflow waits for the
    /// next inbound submit).
    pub(crate) lock_scheduler: Option<Arc<crate::lock_scheduler::LockScheduler>>,
    /// Set by the supervisor to refuse new `workflow.start` calls during a
    /// graceful drain. Existing `submit`/`get` keep working so in-flight work
    /// finishes cleanly. See `docs/reference/configuration.md` "Zero-downtime config changes".
    pub(crate) draining: Arc<AtomicBool>,
    /// SPEC §30.10.4 — live set of subject names that are still
    /// `PENDING_DEFINITION`. Resolution handlers (define_new, link_as_alias,
    /// cancel) in the MCP layer remove entries from this set. The runtime's
    /// pre-start walk checks this set rather than the baked-in snapshot in
    /// `_lexiconLibrary`, so a retry after resolution proceeds without
    /// needing to re-resolve the entire config.
    ///
    /// `None` (default) means the set was never wired — the runtime falls back
    /// to the baked-in `_lexiconLibrary` snapshot for PENDING_DEFINITION checks.
    /// This preserves backward compatibility for call sites that do not wire the
    /// pending-subjects set (direct construction without an MCP server layer).
    ///
    /// `Some(arc)` means the MCP server layer wired the live set. The runtime
    /// uses the live set exclusively as the source of truth — a subject removed
    /// from the set by a resolution handler is immediately unblocked.
    pub(crate) pending_subjects: Option<Arc<RwLock<HashSet<String>>>>,
    /// SPEC §33 D3 — cap on how many chained submit cycles a single
    /// `submit()` call may drive via `ExecuteResult.next_transition`.
    /// Each cycle is its own atomic `dispatch_once` with full audit
    /// invariants; this caps the runaway-loop risk from a misbehaving
    /// `kind: llm` executor that always returns a next_transition.
    ///
    /// Default 32 (PA-era heuristic — long enough to let well-designed
    /// LLM-driven workflows make progress, short enough to surface a
    /// stuck loop quickly). Override via [`Self::with_max_chained_llm_turns`].
    pub(crate) max_chained_llm_turns: u32,
    /// (1b) Auto-drive skill-surfacing `actor: agent` states. When enabled and a
    /// model binding is available, the deterministic chain — instead of stopping
    /// at a lone `actor: agent` transition — invokes the gateway's `kind: agent`
    /// executor to produce the submission, then fires the transition. This makes
    /// the v0.6 cap/orchestrator composition model executable without an external
    /// driver (caps stay noop+skill-surfacing; V6 untouched). Default OFF.
    pub(crate) auto_drive_agents: bool,
    /// Affinity used for the synthesized `kind: agent` invocation when
    /// auto-driving (resolved via models.yaml). Default `"reasoning"`.
    pub(crate) auto_drive_affinity: String,
    /// MCP connection names exposed as `tools` to an auto-driven agent so it can
    /// actually do tool-using work (editor, structureos, …). Set from the
    /// gateway config's `connections:` keys. Empty = pure-reasoning agents only.
    pub(crate) auto_drive_tools: Vec<String>,
    /// Wall-clock bound (seconds) for an auto-driven agent step — fail-fast so a
    /// non-converging agent surfaces in minutes, not the 600s executor default.
    pub(crate) auto_drive_max_seconds: u64,
    /// The repo roots a run may operate on, from the config's `writable: true`
    /// repos (`/praxec/_writableRepos`). A top-level `start` resolves the run's
    /// mandatory [`crate::run_env::RepoRoot`] from this set (see
    /// [`Self::resolve_run_repo_root`]). Empty means the deployment declared no
    /// writable repo — and, since `repo_root` is mandatory, every start then
    /// fails fast. Sub-workflow spawns never consult this: they inherit the
    /// parent's `run_env` verbatim.
    ///
    /// Hot-swappable (behind `Arc<RwLock>`) so `reload_gated` can re-derive it
    /// from a new config without a restart — matching the `Swappable*` reload
    /// wrappers' `RwLock<Arc<…>>` idiom. Reads (per run `start`) take a read
    /// lock; a reload takes a write lock via [`set_writable_repo_roots`].
    pub(crate) writable_repo_roots: Arc<std::sync::RwLock<Vec<crate::run_env::RepoRoot>>>,
}

impl WorkflowRuntime {
    pub fn new(
        definitions: Arc<dyn DefinitionStore>,
        store: Arc<dyn WorkflowStore>,
        executors: Arc<dyn ExecutorRegistry>,
        guards: Arc<dyn GuardEvaluator>,
        audit: Arc<dyn AuditSink>,
    ) -> Self {
        Self {
            definitions,
            store,
            executors,
            guards,
            audit,
            evidence: None,
            repo_locks: None,
            lock_scheduler: None,
            draining: Arc::new(AtomicBool::new(false)),
            pending_subjects: None,
            max_chained_llm_turns: DEFAULT_MAX_CHAINED_LLM_TURNS,
            auto_drive_agents: false,
            auto_drive_affinity: "reasoning".to_string(),
            auto_drive_tools: Vec::new(),
            auto_drive_max_seconds: 180,
            writable_repo_roots: Arc::new(std::sync::RwLock::new(Vec::new())),
        }
    }

    /// Wire the config-declared writable repo roots (from `/praxec/_writableRepos`)
    /// that a top-level `start` resolves the run's `repo_root` from.
    pub fn with_writable_repo_roots(mut self, roots: Vec<crate::run_env::RepoRoot>) -> Self {
        self.writable_repo_roots = Arc::new(std::sync::RwLock::new(roots));
        self
    }

    /// Hot-swap the writable repo set from a re-derived config (FB-1) — called by
    /// `reload_gated` so `praxec.command { reload: true }` actually rewires which
    /// repos runs may operate on. Takes `&self`: the shared long-lived runtime and
    /// every handle cloned from it see the swap (the slot is a shared `Arc<RwLock>`).
    pub fn set_writable_repo_roots(&self, roots: Vec<crate::run_env::RepoRoot>) {
        *self
            .writable_repo_roots
            .write()
            .expect("LOCK_POISONED: writable_repo_roots") = roots;
    }

    /// The canonical paths of the currently-live writable repos — for the reload
    /// response so the operator sees what actually got wired, not a bare "reloaded".
    pub fn writable_repo_roots_snapshot(&self) -> Vec<String> {
        self.writable_repo_roots
            .read()
            .expect("LOCK_POISONED: writable_repo_roots")
            .iter()
            .map(|r| r.as_str().to_string())
            .collect()
    }

    /// Resolve the mandatory run `repo_root` for a top-level start from the
    /// config-declared writable repos (v0.0.21):
    /// - 0 declared → fail fast (a run requires a repo_root);
    /// - exactly 1 → use it (a `selector` naming it — or a subpath of it — is
    ///   accepted, anything else rejected);
    /// - N declared → require `selector` to name one of them (or a subpath),
    ///   else fail listing the choices.
    ///
    /// The selector resolves against the declared roots by canonical path:
    /// either the exact canonical path of a declared root, OR (FB-3) a
    /// **worktree / subpath UNDER** a declared root — so an operator can scope a
    /// run to a git worktree or subdirectory of an already-declared repo without
    /// pre-declaring every such path. Still bounded by the allowlist: a selector
    /// that canonicalizes to a path outside *every* declared root is rejected, so
    /// a caller (or an LLM) can never inject a hallucinated / out-of-tree root.
    /// This is the same invariant `flow.drive-program`'s per-spawn `repoRoot`
    /// override enforces, now available at top-level `start`.
    pub fn resolve_run_repo_root(
        &self,
        selector: Option<&str>,
    ) -> anyhow::Result<crate::run_env::RepoRoot> {
        let roots = self
            .writable_repo_roots
            .read()
            .expect("LOCK_POISONED: writable_repo_roots");
        let roots = &*roots;
        if roots.is_empty() {
            anyhow::bail!(
                "REPO_ROOT_REQUIRED: a run requires a repo_root, but no writable repo is \
                 declared. Declare a repo with `writable: true` in the gateway config."
            );
        }
        match selector {
            Some(sel) => {
                // Fast path: an exact match against a declared canonical root —
                // no filesystem touch.
                if let Some(r) = roots.iter().find(|r| r.as_str() == sel) {
                    return Ok(r.clone());
                }
                // FB-3: the selector may be a worktree / subpath under a declared
                // root. Canonicalize it (this asserts it is an absolute, existing
                // directory) and accept it only if it lies within a declared root.
                let candidate = crate::run_env::RepoRoot::new(sel).map_err(|e| {
                    let choices: Vec<&str> = roots.iter().map(|r| r.as_str()).collect();
                    anyhow::anyhow!(
                        "REPO_ROOT_UNKNOWN: `{sel}` is neither a declared writable repo root nor \
                         a resolvable directory under one ({e}). Declared roots: {}",
                        choices.join(", ")
                    )
                })?;
                // `Path::starts_with` is component-wise, so `/repo-foo` does NOT
                // match declared `/repo` — no string-prefix false positives.
                if roots
                    .iter()
                    .any(|r| candidate.as_path().starts_with(r.as_path()))
                {
                    return Ok(candidate);
                }
                let choices: Vec<&str> = roots.iter().map(|r| r.as_str()).collect();
                anyhow::bail!(
                    "REPO_ROOT_OUTSIDE_ALLOWLIST: `{sel}` resolves to `{candidate}`, which is not \
                     under any declared writable repo root. Choose a path within one of: {}",
                    choices.join(", ")
                )
            }
            None if roots.len() == 1 => Ok(roots[0].clone()),
            None => {
                let choices: Vec<&str> = roots.iter().map(|r| r.as_str()).collect();
                anyhow::bail!(
                    "REPO_ROOT_AMBIGUOUS: {} writable repos are declared; name one via the \
                     `repoRoot` selector. Choices: {}",
                    roots.len(),
                    choices.join(", ")
                )
            }
        }
    }

    /// (1b) Enable auto-driving of skill-surfacing `actor: agent` states via the
    /// `kind: agent` executor, using `affinity` for the model binding,
    /// `tools` (MCP connection names) for the agent's tool access, and a
    /// `max_seconds` fail-fast bound (0 keeps the default 180s).
    pub fn with_auto_drive_agents(
        mut self,
        enabled: bool,
        affinity: impl Into<String>,
        tools: Vec<String>,
        max_seconds: u64,
    ) -> Self {
        self.auto_drive_agents = enabled;
        let a = affinity.into();
        if !a.is_empty() {
            self.auto_drive_affinity = a;
        }
        self.auto_drive_tools = tools;
        if max_seconds > 0 {
            self.auto_drive_max_seconds = max_seconds;
        }
        self
    }

    /// SPEC §8.4 — load a definition's current body by id. The read side of
    /// authoring/editing: the `praxec.query { definitionId }` surface returns
    /// this (+ its content hash) so an author can base an edit on it.
    pub async fn load_definition(&self, definition_id: &str) -> anyhow::Result<Value> {
        self.definitions.load(definition_id).await
    }

    /// SPEC §33 D3 — override the LLM chain depth cap.
    ///
    /// The submit pipeline chains into another `dispatch_once` cycle each
    /// time an executor returns `ExecuteResult.next_transition`. This cap
    /// bounds how many such cycles a single `submit()` call may drive
    /// before failing with `LLM_CHAIN_DEPTH_EXCEEDED`.
    pub fn with_max_chained_llm_turns(mut self, max: u32) -> Self {
        self.max_chained_llm_turns = max;
        self
    }

    /// SPEC §30.10.4 — wire the live pending-subjects set. The MCP layer
    /// calls this with the subjects detected at config-load time; resolution
    /// handlers then remove entries from the shared Arc as subjects are defined.
    /// Wiring the same `Arc` into the runtime means the pre-start subject walk
    /// always reflects the current resolved state without needing a config reload.
    pub fn with_pending_subjects(mut self, pending: Arc<RwLock<HashSet<String>>>) -> Self {
        self.pending_subjects = Some(pending);
        self
    }

    /// Mark this runtime as draining. Subsequent `start` calls fail with a
    /// clean error; `submit`/`get` continue to work so in-flight workflows
    /// can complete.
    pub fn begin_drain(&self) {
        self.draining.store(true, Ordering::SeqCst);
    }

    /// True once `begin_drain` has been called.
    pub fn is_draining(&self) -> bool {
        self.draining.load(Ordering::SeqCst)
    }

    /// Attach an evidence store. Without one, `evidence` guards always pass
    /// (placeholder behavior). With one, accumulated evidence from each
    /// successful transition is persisted and queried by guards on later
    /// transitions.
    pub fn with_evidence(mut self, evidence: Arc<dyn EvidenceStore>) -> Self {
        self.evidence = Some(evidence);
        self
    }

    /// Wire the global repository lock space. With it, a file-owning
    /// transition acquires its `owned_files` before executing — durably
    /// suspending (`waiting_on_lock`) on contention — and releases after.
    pub fn with_repo_locks(mut self, locks: Arc<dyn crate::repo_locks::RepoLocks>) -> Self {
        self.repo_locks = Some(locks);
        self
    }

    /// The shared file-lock authority this runtime coordinates transition
    /// `owned_files` through. Exposed so binary overlays (e.g. untrusted-agent
    /// promotion) lock against the SAME authority rather than a separate space.
    pub fn repo_locks(&self) -> Option<Arc<dyn crate::repo_locks::RepoLocks>> {
        self.repo_locks.clone()
    }

    /// Wire the lock wait-queue. With it (and a repo-lock space), a workflow
    /// suspended on contention auto-resumes FIFO the moment its files free.
    pub fn with_lock_scheduler(
        mut self,
        scheduler: Arc<crate::lock_scheduler::LockScheduler>,
    ) -> Self {
        self.lock_scheduler = Some(scheduler);
        self
    }

    pub fn audit(&self) -> &Arc<dyn AuditSink> {
        &self.audit
    }

    /// Every live mission currently parked awaiting a human — the store-derived
    /// HITL queue the MCP-native `approvals` surface enumerates. Oldest-first
    /// (clear the longest-waiting gate first). Each carries the transition +
    /// `expected_version` a resolving `submit` needs.
    pub async fn list_pending_human(&self) -> anyhow::Result<Vec<crate::hitl::PendingHumanGate>> {
        let all = self.store.list_all().await?;
        Ok(crate::hitl::pending_gates(&all))
    }

    /// SPEC §30.10 — load a workflow instance by id without triggering
    /// timeout/cancellation checks. Used by handlers that need to inspect
    /// the instance's definition snapshot for lexicon embedding (describe,
    /// get, explain augmentation paths). Not a substitute for `get()` in
    /// normal workflow operations; that path includes the full timeout and
    /// cancellation pipeline.
    pub async fn load_instance(
        &self,
        workflow_id: &str,
    ) -> anyhow::Result<crate::model::WorkflowInstance> {
        self.store.load(workflow_id).await
    }

    /// T24 — cancel a running workflow. Sets `cancelled_at` +
    /// `cancelled_reason` on the instance (without changing `state`,
    /// so the operator can later recover by reading the original
    /// position). Subsequent `submit` calls return `WORKFLOW_CANCELLED`;
    /// `get` surfaces `result.status: "cancelled"`. Emits a
    /// `workflow.cancelled` audit event.
    ///
    /// Idempotent: re-cancelling an already-cancelled workflow refreshes
    /// the reason but does not double-emit the audit event (the second
    /// call returns Ok without writing).
    pub async fn cancel(&self, workflow_id: &str, reason: &str) -> anyhow::Result<()> {
        let instance = self.store.load(workflow_id).await?;
        if instance.cancelled_at.is_some() {
            // Already cancelled — idempotent no-op. Re-cancelling
            // shouldn't surprise callers (e.g. a retry loop). The
            // reason from the first cancel wins.
            return Ok(());
        }
        let expected_version = instance.version;
        let mut updated = instance.clone();
        updated.cancelled_at = Some(Utc::now());
        updated.cancelled_reason = Some(reason.to_string());
        // bump version so concurrent submits using stale `expected_version`
        // hit the version-conflict path rather than racing past cancel.
        updated.version = updated.version.saturating_add(1);
        let saved = self
            .store
            .save_if_version(updated, expected_version)
            .await?;

        let event = saved
            .audit_event("workflow.cancelled")
            .with_payload(serde_json::json!({
                "reason": reason,
                "state_at_cancel": saved.state,
                "version_at_cancel": saved.version,
            }));
        self.record_or_self_event(event).await;
        // P2 (Task C liveness) — a cancelled child is finalized even though its
        // `state` stays non-terminal. If it was spawned by a `kind: workflow`
        // transition, the suspended parent must be woken to fail-propagate (the
        // reuse path's `get(child)` short-circuits on `cancelled_at` and maps it
        // to `failed`). No re-entrancy: the parent's re-drive submit does not
        // re-cancel the child, and a re-cancel here would idempotent-no-op above.
        self.resume_parent_if_any(&saved).await;
        Ok(())
    }

    /// Reap orphaned runs at startup. An instance a driver/CLI left in a
    /// `running` position — the process died mid-step — is a durable zombie: no
    /// live owner will ever advance it, yet it isn't terminal. On a fresh
    /// process start there are no in-process drivers, so a non-terminal instance
    /// that is genuinely *running* (an executor/agent owns the next move) has no
    /// owner and is an orphan. This cancels those (idempotently, via
    /// [`cancel`](Self::cancel), so each reap emits `workflow.cancelled` and is
    /// auditable). Returns the number reaped.
    ///
    /// SAFETY — it deliberately leaves alone anything that legitimately persists
    /// across restarts, so it can never destroy recoverable work: cancelled or
    /// terminal instances; a human gate (`actor: human` — awaiting a person who
    /// may return days later); and engine-waits that self-resume on restart
    /// (`_lock_wait` / `_subworkflow_wait` / `_agent_await`). Only a
    /// `running`-classified instance is reaped, mirroring the `get`-time status
    /// derivation (a `waiting` mission is never touched).
    pub async fn reap_orphaned_runs(&self) -> anyhow::Result<usize> {
        let all = self.store.list_all().await?;
        let mut reaped = 0usize;
        for inst in all {
            if inst.cancelled_at.is_some() {
                continue; // already resolved as cancelled
            }
            // The definition snapshot travels with the instance — classify
            // against it, never the (possibly reloaded) live config.
            let def = &inst.definition;
            if is_terminal(def, &inst.state) {
                continue; // succeeded / failed — resolved, nothing to reap
            }
            // Livelock quarantine (FB-9): an instance that has already burned
            // its full CUMULATIVE hop budget without terminating is a livelock.
            // Quarantine it here — BEFORE the human/engine-wait skip below —
            // because a livelocking auto-driven agent loop persists an
            // `_agent_await` marker and would otherwise be SKIPPED and
            // auto-resumed straight back into the burn (the exact FB-9 hole).
            // The in-drive check in `run_deterministic_chain` normally trips
            // first; this is the boot-time backstop for an instance that
            // persisted over budget across a restart.
            let livelock_budget = def
                .get("livelockHopBudget")
                .and_then(Value::as_u64)
                .unwrap_or(crate::runtime::runtime_chain::DEFAULT_LIVELOCK_HOP_BUDGET);
            let hops_total = inst
                .context
                .get(crate::runtime::runtime_chain::CHAIN_HOPS_TOTAL_KEY)
                .and_then(Value::as_u64)
                .unwrap_or(0);
            if hops_total >= livelock_budget {
                self.cancel(
                    &inst.id,
                    "livelock_quarantine: exceeded cumulative chain-hop budget \
                     without terminating (reaped at startup)",
                )
                .await?;
                reaped += 1;
                continue;
            }
            // Legitimately WAITING → never reap (mirrors the `get` derivation).
            let state_def = def.pointer(&format!("/states/{}", pointer_escape(&inst.state)));
            let awaiting_human = state_def
                .and_then(|s| s.get("actor"))
                .and_then(Value::as_str)
                == Some("human");
            let engine_wait = inst.context.get("_lock_wait").is_some()
                || inst.context.get("_subworkflow_wait").is_some()
                || inst.context.get("_agent_await").is_some();
            if awaiting_human || engine_wait {
                continue;
            }
            // Non-terminal, non-waiting, no live driver at startup: an orphan.
            self.cancel(&inst.id, "orphaned run reaped at startup (no live driver)")
                .await?;
            reaped += 1;
        }
        Ok(reaped)
    }

    /// SPEC §5.8 non-critical-path audit pattern (FMECA FM-8 mitigation,
    /// audit-resolution plan C.1). Records `event` to the audit sink; on
    /// sink failure, emits an `audit.write_failed` self-event so the loss
    /// is observable. If the self-event ALSO fails, falls back to a
    /// tracing::warn — last-resort but never silent.
    ///
    /// Use this from non-critical paths where the workflow operation must
    /// continue regardless of audit outcome (e.g. `chain.step`,
    /// `chain.completed`, post-outcome notifications). The §7.3
    /// audit-before-commit pattern (e.g. transition records, definition
    /// publishes) must propagate errors via `?` instead.
    pub(crate) async fn record_or_self_event(&self, event: crate::audit::AuditEvent) {
        let event_type = event.event_type.clone();
        if let Err(primary_err) = self.audit.record(event).await {
            let self_event = crate::audit::AuditEvent::new("audit.write_failed").with_payload(
                serde_json::json!({
                    "originalEvent": event_type,
                    "error":         primary_err.to_string(),
                }),
            );
            if let Err(inner) = self.audit.record(self_event).await {
                tracing::warn!(
                    original = %event_type,
                    primary_err = %primary_err,
                    selfevt_err = %inner,
                    "non-critical audit write failed and self-event also failed"
                );
            }
        }
    }

    /// T25 — spawn a tokio watchdog that fires after the workflow's
    /// `timeoutMs` elapses and triggers the lazy-timeout path (which
    /// transitions to `onTimeout.target`, emits `workflow.timed_out`,
    /// and runs the existing deterministic-chain expansion). The
    /// watchdog is best-effort: if the workflow completes naturally
    /// before the timeout, the watchdog's `get()` call is cheap and
    /// observes the terminal state without re-firing. Lost watchdogs
    /// across process restarts are recovered on next get/submit via
    /// the existing lazy check — this active watchdog only matters
    /// for workflows that complete (or stall) without any caller
    /// touching them after `start`.
    ///
    /// Returns the spawned `JoinHandle` so callers can keep / abort
    /// it; the runtime itself doesn't track these (no Drop hook
    /// needed). For most callers — the gateway's MCP server, tests —
    /// the handle is dropped on the floor and the task self-cleans
    /// when it finishes.
    fn spawn_timeout_watchdog(
        &self,
        workflow_id: &str,
        timeout_ms: u64,
    ) -> tokio::task::JoinHandle<()> {
        let rt = self.clone();
        let wid = workflow_id.to_string();
        tokio::spawn(async move {
            tokio::time::sleep(std::time::Duration::from_millis(timeout_ms)).await;
            // Triggering get() runs the existing lazy timeout check. The
            // watchdog is observational, not assertive — it does not change
            // control flow on failure — but a watchdog that silently fails to
            // fire (e.g. the get errors) must be observable, so log it rather
            // than swallowing the error outright.
            if let Err(e) = rt
                .get(GetWorkflow {
                    workflow_id: wid,
                    principal: Principal::anonymous(),
                    trace_id: None,
                    run_id: None,
                })
                .await
            {
                tracing::warn!(error = %e, "timeout watchdog get failed");
            }
        })
    }

    pub async fn start(&self, request: StartWorkflow) -> anyhow::Result<Value> {
        if self.is_draining() {
            bail!("gateway is shutting down; please retry shortly");
        }

        // SPEC §32 — run_id uniqueness assertion. If the store indexes
        // run_id, reject duplicates with a structured error. Stores that
        // return Ok(None) by trait default opt out of the check; their
        // runtime sees no constraint (best-effort safety net).
        if let Some(run_id) = &request.run_env.run_id {
            if let Some(existing_workflow_id) = self.store.find_by_run_id(run_id).await? {
                return Err(RuntimeError::RunIdAlreadyRunning {
                    run_id: run_id.clone(),
                    existing_workflow_id,
                }
                .into());
            }
        }

        let definition = self.definitions.load(&request.definition_id).await?;

        // SPEC §30.10.4 — pre-start subject walk.
        //
        // Two-phase check against the workflow's `_lexiconLibrary`:
        //
        // Phase 1 (live set — wired path): when `pending_subjects` is `Some`,
        // the MCP server layer has shared its live set with the runtime. Use
        // this set as the exclusive source of truth. Resolution handlers
        // (define_new, link_as_alias, cancel) remove entries from this set,
        // and the next retry immediately sees the resolved state — no config
        // reload needed. A subject removed from the live set is treated as
        // resolved even though the baked-in snapshot still says PENDING_DEFINITION.
        //
        // Phase 2 (snapshot fallback — unwired path): when `pending_subjects`
        // is `None`, the runtime was constructed without wiring the set (direct
        // construction in benchmarks, integration tests, or call sites that don't
        // use the MCP server layer). Fall back to the baked-in `_lexiconLibrary`
        // snapshot. This preserves backward compatibility.
        if let Some(lib) = definition.get("_lexiconLibrary").and_then(Value::as_object) {
            match &self.pending_subjects {
                Some(pending_arc) => {
                    // Phase 1 — live set is the source of truth.
                    let pending = pending_arc
                        .read()
                        .expect("pending_subjects RwLock poisoned");
                    for (term, entry) in lib {
                        if pending.contains(term.as_str()) {
                            let bounded_context = entry
                                .get("bounded_context")
                                .and_then(Value::as_str)
                                .map(String::from);
                            return Err(RuntimeError::SubjectNeedsDefinition {
                                unknown_subject: term.clone(),
                                bounded_context,
                                workflow_id_context: format!("workflow:{}", request.definition_id),
                            }
                            .into());
                        }
                    }
                }
                None => {
                    // Phase 2 — snapshot fallback.
                    for (term, entry) in lib {
                        if entry.get("state").and_then(Value::as_str) == Some("PENDING_DEFINITION")
                        {
                            let bounded_context = entry
                                .get("bounded_context")
                                .and_then(Value::as_str)
                                .map(String::from);
                            return Err(RuntimeError::SubjectNeedsDefinition {
                                unknown_subject: term.clone(),
                                bounded_context,
                                workflow_id_context: format!("workflow:{}", request.definition_id),
                            }
                            .into());
                        }
                    }
                }
            }
        }

        let mut input = request.input;
        apply_schema_defaults(definition.pointer("/inputSchema"), &mut input);
        validate_schema(definition.pointer("/inputSchema"), &input, "workflow input")?;
        let request = StartWorkflow { input, ..request };

        let initial_state = required_str(&definition, "/initialState")?.to_owned();
        // CMP-051 — the instance carries its own resolved definition snapshot.
        // Source `definition_version` from that snapshot's `version`. The config
        // loader (config.rs step 5) stamps a string `"0"` default onto every
        // workflow definition that lacks an explicit `version`, so by load time
        // a string `version` is GUARANTEED. A missing/non-string `version`
        // reaching here means a definition bypassed the loader (corrupt
        // snapshot) — fail-fast rather than silently re-stamping "0" across the
        // whole audit trail and masking the corruption.
        let definition_version = required_str(&definition, "/version")
            .map_err(|_| {
                anyhow::anyhow!(
                    "CORRUPT_DEFINITION: workflow definition '{}' has no string `version`; \
                     the config loader guarantees one by load time, so this snapshot bypassed \
                     loading.",
                    request.definition_id
                )
            })?
            .to_owned();

        let mut initial_context = definition
            .get("initialContext")
            .cloned()
            .unwrap_or_else(|| json!({}));
        // Poka-yoke (input→context seeding): the slot table already treats every
        // declared `inputs:` entry as a reachable `$.context` slot
        // (SlotSource::Input), so V13 lets a state read `$.context.<input>`. Make
        // that true at runtime — seed each resolved input (schema defaults
        // already applied above) into the initial context for any key NOT
        // explicitly set by `initialContext` (initialContext wins → backward
        // compatible). This removes the whole class of "read `$.context.<input>`
        // → null because a manual seeding state forgot it" bugs, and the
        // error-prone manual seeding state itself.
        if let (Some(ctx), Some(inp)) = (initial_context.as_object_mut(), request.input.as_object())
        {
            for (k, v) in inp {
                ctx.entry(k.clone()).or_insert_with(|| v.clone());
            }
        }
        let instance = WorkflowInstance {
            id: format!("wf_{}", Uuid::new_v4().simple()),
            definition_id: request.definition_id.clone(),
            definition_version,
            definition: definition.clone(),
            state: initial_state,
            version: 0,
            input: request.input,
            context: initial_context,
            started_at: Utc::now(),
            // SPEC §20.2 — persist the run-ambient env (repo root + trace/run
            // correlation) on the instance so every downstream audit event and
            // every executor inherits them, and a sub-workflow spawn propagates
            // the same root/identity to the child.
            run_env: request.run_env,
            depth: request.depth,
            cancelled_at: None,
            cancelled_reason: None,
            // P2 — link a spawned child back to its parent transition so the
            // runtime re-drives the parent when this child terminates (Task C).
            parent: request.parent,
        };
        let correlation_id = format!("cor_{}", Uuid::new_v4().simple());

        let instance = self.store.create(instance).await?;

        // T25 — spawn the timeout watchdog as soon as the instance
        // exists in the store. Definitions without `timeoutMs` (the
        // common case) skip this; the lazy check still covers any
        // workflow that does get touched after a notional deadline.
        if let Some(timeout_ms) = definition.get("timeoutMs").and_then(Value::as_u64) {
            // Fire-and-forget: the watchdog self-cleans when its
            // sleep + get returns. The JoinHandle is detached
            // intentionally — no Drop hook on the runtime needs to
            // abort it, and the workflow itself doesn't outlive the
            // process.
            drop(self.spawn_timeout_watchdog(&instance.id, timeout_ms));
        }

        self.audit
            .record(
                instance
                    .audit_event("workflow.started")
                    .with_correlation(&correlation_id)
                    .with_actor(&request.principal.subject)
                    // L1 tree-linkage — stamp the tree edge from the persisted
                    // instance (its `parent` ParentLink + `depth`), NOT a
                    // task-local. `workflow.started` is the natural carrier of
                    // the parent→child edge; an observer reconstructs the
                    // execution tree from workflow_id + parent_workflow_id +
                    // depth without every executor emit site stamping.
                    .with_topology(
                        instance.parent.as_ref().map(|p| p.workflow_id.clone()),
                        instance.depth,
                    )
                    .with_payload(json!({
                        "definitionId": instance.definition_id,
                        "state": instance.state,
                        "version": instance.version,
                    })),
            )
            .await?;

        let instance = self
            .run_on_enter(definition.clone(), instance, &correlation_id)
            .await?;

        // Run deterministic chain from the initial state
        let max_depth = definition
            .get("maxChainDepth")
            .and_then(Value::as_u64)
            .unwrap_or(50);
        let livelock_budget = definition
            .get("livelockHopBudget")
            .and_then(Value::as_u64)
            .unwrap_or(crate::runtime::runtime_chain::DEFAULT_LIVELOCK_HOP_BUDGET);
        let chain_outcome = self
            .run_deterministic_chain(
                &definition,
                instance,
                &request.principal,
                &correlation_id,
                max_depth,
                livelock_budget,
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
                        StatusHint::Started,
                        &definition,
                        &result.instance,
                        &correlation_id,
                        &request.principal,
                    )
                    .await;
                    // NOTE (Task C): we deliberately do NOT re-drive the parent
                    // here. A child that is terminal at the end of its `start`
                    // chain is being spawned by a `kind: workflow` executor still
                    // on the call stack — that executor reads the `succeeded`
                    // status off this `start` response and advances the parent
                    // itself (no suspend, no re-drive). Re-driving here would
                    // re-enter the parent transition before its first dispatch
                    // committed, spawning a fresh child each time and recursing to
                    // a stack overflow. The re-drive that matters — a child that
                    // terminates LATER, after the parent suspended — is hooked in
                    // `dispatch_once`'s terminal path instead.
                }

                let mut response = self
                    .response(
                        &definition,
                        &result.instance,
                        StatusHint::Started,
                        None,
                        &request.principal,
                    )
                    .await;
                if !result.steps.is_empty() {
                    response["chain"] = serde_json::to_value(&result.steps)?;
                }
                if !result.evidence.is_empty() {
                    response["evidence"] = serde_json::to_value(&result.evidence)?;
                }
                Ok(response)
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
                if !partial.steps.is_empty() {
                    response["chain"] = serde_json::to_value(&partial.steps)?;
                }
                if !partial.evidence.is_empty() {
                    response["evidence"] = serde_json::to_value(&partial.evidence)?;
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
                Ok(response)
            }
            ChainOutcome::Suspended {
                partial,
                suspend,
                transition,
            } => {
                // Chain path during `start` — a chain leaf signalled a step
                // suspend (P2 sub-workflow or P12 agent await). Durably park
                // the parent and respond `waiting`, mirroring the
                // direct-submit suspend path. `partial.instance` carries the
                // context the PRIOR chain steps committed, at its current
                // version; the suspend save writes the wait marker and bumps
                // that version by exactly 1. A STALE_WORKFLOW_VERSION here is
                // a genuine rejection — propagate via `?`, never fake a
                // `waiting` response.
                match suspend {
                    crate::model::StepSuspend::Subworkflow(s) => {
                        self.suspend_on_subworkflow(
                            &definition,
                            &partial.instance,
                            &transition,
                            partial.instance.version,
                            &request.principal,
                            s,
                        )
                        .await
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
                        .await
                    }
                }
            }
            ChainOutcome::WaitingOnLock {
                partial,
                transition,
                files,
                conflict,
            } => {
                // An auto-driven leaf could not acquire its declared owned_files.
                // Park durably on the SAME routine the submit gate uses, so a
                // chain leaf and a submit leaf behave identically under
                // contention, and the LockScheduler re-drives us when it frees.
                self.park_on_lock(
                    &definition,
                    &partial.instance,
                    &transition,
                    partial.instance.version,
                    &request.principal,
                    &files,
                    &conflict,
                )
                .await
            }
            ChainOutcome::Quarantined { partial, reason } => {
                // Liveness backstop during `start` — the run exceeded its
                // cumulative hop budget without terminating. Cancel it
                // (idempotent; wakes any suspended parent) so it cannot re-drive
                // and burn the model, then surface the cancellation with the
                // livelock reason (mirrors how `get` reports a cancelled run).
                self.cancel(&partial.instance.id, &reason).await?;
                let cancelled = self.store.load(&partial.instance.id).await?;
                let mut response = self
                    .response(
                        &definition,
                        &cancelled,
                        StatusHint::Cancelled,
                        Some(json!({
                            "code": "LIVELOCK_QUARANTINE",
                            "message": reason,
                            "cancelled_reason": cancelled.cancelled_reason,
                        })),
                        &request.principal,
                    )
                    .await;
                if !partial.steps.is_empty() {
                    response["chain"] = serde_json::to_value(&partial.steps)?;
                }
                Ok(response)
            }
        }
    }

    pub async fn get(&self, request: GetWorkflow) -> anyhow::Result<Value> {
        let instance = self.store.load(&request.workflow_id).await?;
        // In-flight: resolve the definition from the instance's carried
        // snapshot, never from the live `DefinitionStore`. A config edit or
        // hot reload must not disturb a running instance (SPEC §8.3).
        let definition = instance.definition.clone();
        // T24 — cancellation takes precedence over timeout. The
        // original state is preserved on the instance; the response's
        // `result.status` carries the cancelled signal so callers
        // (interpreter, LLM resume) see the workflow is terminal even
        // though its `state` field still names the recoverable position.
        if instance.cancelled_at.is_some() {
            let cancelled_payload = serde_json::json!({
                "cancelled_at":  instance.cancelled_at,
                "cancelled_reason": instance.cancelled_reason,
            });
            return Ok(self
                .response(
                    &definition,
                    &instance,
                    StatusHint::Cancelled,
                    Some(cancelled_payload),
                    &request.principal,
                )
                .await);
        }
        if let Some(timed_out) = self
            .check_and_apply_timeout(&definition, instance.clone(), &request.principal)
            .await?
        {
            return Ok(self
                .response(
                    &definition,
                    &timed_out,
                    StatusHint::TimedOut,
                    None,
                    &request.principal,
                )
                .await);
        }
        Ok(self
            .response(
                &definition,
                &instance,
                StatusHint::WaitingForAction,
                None,
                &request.principal,
            )
            .await)
    }

    /// The `actor: deterministic` transitions legal at the instance's current
    /// state — the moves the server-side chain auto-fires. The §32 `links`
    /// projection deliberately hides these (engine-fired, not client-chosen),
    /// which is correct for the human/agent surface but blinds the headless
    /// `orchestrate` driver when the chain has halted (e.g. at a guard-gated
    /// branch). The driver reads them through this accessor so it can re-fire one
    /// and never strand a `running` instance that still holds a fireable move
    /// (poka-yoke). Empty at a terminal state.
    pub async fn deterministic_legal_now(&self, workflow_id: &str) -> anyhow::Result<Vec<String>> {
        let instance = self.store.load(workflow_id).await?;
        let definition = &instance.definition;
        if is_terminal(definition, &instance.state) {
            return Ok(Vec::new());
        }
        let path = format!("/states/{}/transitions", pointer_escape(&instance.state));
        let Some(transitions) = definition.pointer(&path).and_then(Value::as_object) else {
            return Ok(Vec::new());
        };
        Ok(transitions
            .iter()
            .filter(|(_, t)| t.get("actor").and_then(Value::as_str) == Some("deterministic"))
            .map(|(name, _)| name.clone())
            .collect())
    }

    /// SPEC §8.2 + §12 — resolve a guidance fragment's `{verb, body}` from the
    /// snapshot pinned to a specific workflow instance. Returns `None` if
    /// either the workflow id or the subject is unknown to the snapshot.
    /// Used by `gateway.describe { id, workflowId }` so an in-flight LLM
    /// receives the body that existed when the workflow was started — not
    /// whatever the operator has since edited the live config to say.
    pub async fn describe_guidance_for_workflow(
        &self,
        workflow_id: &str,
        subject: &str,
    ) -> anyhow::Result<Option<Value>> {
        let instance = self.store.load(workflow_id).await?;
        let Some(entry) = instance
            .definition
            .pointer("/_skillsLibrary")
            .and_then(Value::as_object)
            .and_then(|lib| lib.get(subject))
        else {
            return Ok(None);
        };
        // CMP-051 — a PRESENT-but-incomplete library entry must not emit
        // `unwrap_or_default()` empty strings. `verb` and `hash` feed
        // hash-flip acknowledgement guards (SPEC §12); an empty `hash`
        // silently makes every ack-guard compare against `""`, which a
        // forged/empty ack would satisfy. A present entry missing either is
        // a corrupt snapshot — fail-fast rather than emit a guard-poisoning
        // empty string. (An ABSENT subject is the legitimate `None` above.)
        let verb = required_str(entry, "/verb").map_err(|_| {
            anyhow::anyhow!(
                "CORRUPT_GUIDANCE_ENTRY: skills library entry for subject '{subject}' is \
                 present but has no string `verb`"
            )
        })?;
        let hash = required_str(entry, "/hash").map_err(|_| {
            anyhow::anyhow!(
                "CORRUPT_GUIDANCE_ENTRY: skills library entry for subject '{subject}' is \
                 present but has no string `hash` (required for hash-flip ack guards)"
            )
        })?;
        // `body` / `lifecycle` are descriptive, not guard inputs — a missing
        // value defaults harmlessly.
        let body = entry
            .get("body")
            .and_then(Value::as_str)
            .unwrap_or_default();
        let lifecycle = entry
            .get("lifecycle")
            .and_then(Value::as_str)
            .unwrap_or_default();
        Ok(Some(json!({
            "kind":      "guidance",
            "subject":   subject,
            "verb":      verb,
            "lifecycle": lifecycle,
            "hash":      hash,
            "body":      body,
        })))
    }

    /// SPEC §22 — mirror of [`describe_guidance_for_workflow`] but reads
    /// from the instance's `_scriptsLibrary` snapshot. Returns `None` when
    /// the subject isn't in the snapshot (caller can then fall back to
    /// the live discovery index, but typically a script subject either
    /// belongs to a workflow's library or isn't visible to that workflow).
    pub async fn describe_script_for_workflow(
        &self,
        workflow_id: &str,
        subject: &str,
    ) -> anyhow::Result<Option<Value>> {
        let instance = self.store.load(workflow_id).await?;
        let Some(entry) = instance
            .definition
            .pointer("/_scriptsLibrary")
            .and_then(Value::as_object)
            .and_then(|lib| lib.get(subject))
        else {
            return Ok(None);
        };
        let verb = entry
            .get("verb")
            .and_then(Value::as_str)
            .unwrap_or_default();
        let body = entry
            .get("body")
            .and_then(Value::as_str)
            .unwrap_or_default();
        let lifecycle = entry
            .get("lifecycle")
            .and_then(Value::as_str)
            .unwrap_or_default();
        let hash = entry
            .get("hash")
            .and_then(Value::as_str)
            .unwrap_or_default();
        Ok(Some(json!({
            "kind":      "script",
            "subject":   subject,
            "verb":      verb,
            "lifecycle": lifecycle,
            "hash":      hash,
            "body":      body,
        })))
    }

    pub async fn explain(&self, workflow_id: &str, transition: &str) -> anyhow::Result<Value> {
        let instance = self.store.load(workflow_id).await?;
        // In-flight: resolve the definition from the instance's carried
        // snapshot, never from the live `DefinitionStore` (SPEC §8.3).
        let definition = instance.definition.clone();

        let transition_def = transition_definition(&definition, &instance.state, transition);
        let allowed = transition_def.is_some();
        let actor = transition_def
            .and_then(|t| t.get("actor"))
            .and_then(Value::as_str)
            .unwrap_or("agent");
        let is_deterministic = actor == "deterministic";

        let legal_now: Vec<String> = definition
            .pointer(&format!(
                "/states/{}/transitions",
                pointer_escape(&instance.state)
            ))
            .and_then(Value::as_object)
            .map(|m| m.keys().cloned().collect())
            .unwrap_or_default();

        Ok(json!({
            "workflowId": instance.id,
            "currentState": instance.state,
            "transition": transition,
            "allowedFromCurrentState": allowed,
            "actor": actor,
            "deterministic": is_deterministic,
            "legalTransitionsNow": legal_now,
        }))
    }

    /// Emit the transition record for one applied transition, **record-first**:
    /// this writes the `workflow.transition` audit event and MUST be called
    /// *before* `save_if_version` commits the resulting snapshot.
    ///
    /// `seq` is the resulting `WorkflowInstance.version` (post-increment). The
    /// caller passes the to-be-committed instance so every required field can be
    /// sourced exactly.
    ///
    /// On `Err`, the caller MUST abort the transition and NOT commit the
    /// snapshot — propagating the [`RuntimeError::RecordWriteFailed`] is the
    /// whole point of the record-first ordering. The `Result` must never be
    /// swallowed.
    pub(crate) async fn emit_transition_record(
        &self,
        params: TransitionRecordParams<'_>,
    ) -> Result<(), RuntimeError> {
        let seq = params.instance.version;

        // SPEC §7.2 — executor descriptor: `{ kind, ok, durationMs }` when
        // the transition's executor actually ran. `kind` comes from the
        // declared executor on the transition; `ok` + `durationMs` come
        // from the caller's wall-clock around `execute_with_reliability`.
        // For transitions without an executor (or paths like onTimeout)
        // the descriptor is omitted entirely.
        //
        // SPEC §22.6 (v0.3) — for `kind: script` executors, the descriptor
        // additionally carries `subject` (the curated script subject) and
        // `hash` (the body hash from the workflow's pinned _scriptsLibrary
        // snapshot). Together they let audit replay pull the exact bytes
        // that ran by content-identity. Fields are additive + optional;
        // non-script executors get the legacy 3-field shape unchanged.
        let executor_cfg = params.transition_def.get("executor");
        let executor = executor_cfg
            .and_then(|e| e.get("kind").and_then(Value::as_str).map(|k| (k, e)))
            .map(|(kind, exec_cfg)| {
                let mut desc = json!({ "kind": kind });
                if let Some((ok, duration_ms)) = params.executor_outcome {
                    desc["ok"] = Value::Bool(ok);
                    desc["durationMs"] = json!(duration_ms);
                }
                if kind == "script" {
                    if let Some(subject) = exec_cfg.get("subject").and_then(Value::as_str) {
                        desc["subject"] = Value::String(subject.to_string());
                        // Snapshot lookup — JSON-pointer escape for `~` / `/`
                        // per RFC 6901. Subjects use `.` so escapes don't
                        // normally trigger; do it correctly anyway.
                        let escaped = subject.replace('~', "~0").replace('/', "~1");
                        if let Some(hash) = params
                            .instance
                            .definition
                            .pointer(&format!("/_scriptsLibrary/{escaped}/hash"))
                            .and_then(Value::as_str)
                        {
                            desc["hash"] = Value::String(hash.to_string());
                        }
                    }
                }
                desc
            });

        // SPEC §7.2 — `blackboardDelta` carries the per-transition diff of
        // `context` so cumulative replay (§7.5) can reconstruct the blackboard
        // at any past `seq`. Computed by the call site against pre/post-merge
        // contexts.
        //
        // SPEC §7.2 — `guards` carries each guard that was actually evaluated
        // on this transition, in declaration order, as `{kind, result}` pairs.
        // For deterministic chain hops and onTimeout (where guards aren't
        // evaluated), this is an empty vec. `childWorkflowId` is set when
        // the transition's executor was `kind: workflow` and reported the
        // sub-workflow id it spawned; null otherwise.
        let child = match params.child_workflow_id {
            Some(id) => Value::String(id),
            None => Value::Null,
        };
        let mut record = json!({
            "workflowId": params.instance.id,
            "definitionId": params.instance.definition_id,
            "definitionVersion": params.instance.definition_version,
            "seq": seq,
            "timestamp": Utc::now().to_rfc3339(),
            "fromState": params.from_state,
            "toState": params.instance.state,
            "transition": params.transition_name,
            "actor": params.actor,
            "principal": params.principal,
            "guards": params.guard_results,
            "arguments": params.arguments,
            "blackboardDelta": params.blackboard_delta,
            "childWorkflowId": child,
            "correlationId": params.correlation_id,
        });
        if let Some(executor) = executor {
            record["executor"] = executor;
        }

        // SPEC §29 — lightweight transitions emit a different event
        // type so consumers can separate state-change records from
        // interaction-style self-loops (e.g. `ask_human`). The
        // `purpose:` field (when declared) propagates into the audit
        // payload for downstream filtering.
        let lightweight = params
            .transition_def
            .get("lightweight")
            .and_then(Value::as_bool)
            .unwrap_or(false);
        if let Some(purpose) = params.transition_def.get("purpose").and_then(Value::as_str) {
            record["purpose"] = Value::String(purpose.to_string());
        }
        let event_type = if lightweight {
            "workflow.interaction"
        } else {
            "workflow.transition"
        };

        let mut event = params
            .instance
            .audit_event(event_type)
            .with_correlation(params.correlation_id)
            .with_payload(record);
        if let Some(principal) = params.principal {
            event = event.with_actor(principal);
        }

        self.audit
            .record(event)
            .await
            .map_err(|source| RuntimeError::RecordWriteFailed {
                workflow_id: params.instance.id.clone(),
                seq,
                source,
            })
    }
}

// ---------------------------------------------------------------------------
// Guidance refs (SPEC v2 §5.5)
// ---------------------------------------------------------------------------

// ---------------------------------------------------------------------------
// Guidance string templating (SPEC v2 §5.2)
// ---------------------------------------------------------------------------

#[cfg(test)]
mod reap_tests {
    use super::*;
    use crate::audit::{AuditSink, MemoryAuditSink};
    use crate::guards::DefaultGuardEvaluator;
    use crate::store::{ConfigDefinitionStore, InMemoryWorkflowStore};
    use serde_json::json;
    use std::sync::Arc;

    struct EmptyRegistry;
    impl ExecutorRegistry for EmptyRegistry {
        fn get(&self, _kind: &str) -> Option<Arc<dyn Executor>> {
            None
        }
    }

    fn inst(id: &str, definition: serde_json::Value, state: &str) -> WorkflowInstance {
        WorkflowInstance {
            id: id.into(),
            definition_id: "demo".into(),
            definition_version: "1.0.0".into(),
            definition,
            state: state.into(),
            version: 0,
            input: json!({}),
            context: json!({}),
            started_at: chrono::Utc::now(),
            run_env: crate::RunEnv::for_test(),
            cancelled_at: None,
            cancelled_reason: None,
            depth: 0,
            parent: None,
        }
    }

    fn runtime(store: Arc<InMemoryWorkflowStore>) -> WorkflowRuntime {
        // reap classifies against each instance's OWN definition snapshot, so the
        // definition store here can be empty.
        let defs = Arc::new(ConfigDefinitionStore::from_config(&json!({
            "version": "1.0.0", "workflows": {}
        })));
        WorkflowRuntime::new(
            defs,
            store,
            Arc::new(EmptyRegistry),
            Arc::new(DefaultGuardEvaluator::new()),
            Arc::new(MemoryAuditSink::new()) as Arc<dyn AuditSink>,
        )
    }

    fn running_def() -> serde_json::Value {
        json!({
            "initialState": "working",
            "states": {
                "working": { "transitions": { "go": { "target": "done", "actor": "agent" } } },
                "done": { "terminal": true }
            }
        })
    }

    #[tokio::test]
    async fn reap_cancels_only_orphaned_running_instances() {
        let store = Arc::new(InMemoryWorkflowStore::new());

        // (1) orphaned RUNNING (an agent owns the next move) → reaped.
        store
            .create(inst("wf_running", running_def(), "working"))
            .await
            .unwrap();

        // (2) parked at a human gate → left alone (a person may return later).
        let human = json!({
            "initialState": "gate",
            "states": {
                "gate": { "actor": "human", "transitions": { "ok": { "target": "done" } } },
                "done": { "terminal": true }
            }
        });
        store.create(inst("wf_human", human, "gate")).await.unwrap();

        // (3) suspended on a repo lock → self-resumes on release, left alone.
        let mut locked = inst("wf_lock", running_def(), "working");
        locked.context = json!({ "_lock_wait": { "files": ["a"] } });
        store.create(locked).await.unwrap();

        // (4) terminal → resolved, left alone.
        store
            .create(inst("wf_done", running_def(), "done"))
            .await
            .unwrap();

        // (5) already cancelled → left alone (idempotent).
        let mut cancelled = inst("wf_cancelled", running_def(), "working");
        cancelled.cancelled_at = Some(chrono::Utc::now());
        store.create(cancelled).await.unwrap();

        let rt = runtime(store.clone());
        let reaped = rt.reap_orphaned_runs().await.unwrap();
        assert_eq!(reaped, 1, "only the orphaned running instance is reaped");

        assert!(
            store
                .load("wf_running")
                .await
                .unwrap()
                .cancelled_at
                .is_some(),
            "the orphaned running instance must be cancelled"
        );
        assert!(store.load("wf_human").await.unwrap().cancelled_at.is_none());
        assert!(store.load("wf_lock").await.unwrap().cancelled_at.is_none());
        assert!(store.load("wf_done").await.unwrap().cancelled_at.is_none());
        assert!(
            store
                .load("wf_cancelled")
                .await
                .unwrap()
                .cancelled_at
                .is_some()
        );
    }

    /// FB-9: an instance that persisted OVER its cumulative hop budget while
    /// carrying an `_agent_await` marker is quarantined at startup — even though
    /// the engine-wait skip below would otherwise leave it alone. This is the
    /// exact hole: a livelocking auto-driven agent loop parks `_agent_await` and
    /// would auto-resume straight back into the burn across a restart. A
    /// SECOND `_agent_await` instance that is UNDER budget is left alone,
    /// proving only the over-budget one is reaped.
    #[tokio::test]
    async fn reap_quarantines_an_over_budget_agent_await_instance() {
        let store = Arc::new(InMemoryWorkflowStore::new());

        // Over budget (>= default 300) AND parked on `_agent_await` → quarantined.
        let mut wedged = inst("wf_livelock", running_def(), "working");
        wedged.context = json!({
            "_agent_await": { "correlation_id": "x" },
            "_chain_hops_total": 300
        });
        store.create(wedged).await.unwrap();

        // Same `_agent_await` marker but UNDER budget → a legitimate parked
        // agent session, left alone (the false-positive guard for reap).
        let mut healthy = inst("wf_await", running_def(), "working");
        healthy.context = json!({
            "_agent_await": { "correlation_id": "y" },
            "_chain_hops_total": 12
        });
        store.create(healthy).await.unwrap();

        let rt = runtime(store.clone());
        let reaped = rt.reap_orphaned_runs().await.unwrap();
        assert_eq!(reaped, 1, "only the over-budget livelock is quarantined");

        let livelock = store.load("wf_livelock").await.unwrap();
        assert!(
            livelock.cancelled_at.is_some(),
            "the over-budget _agent_await instance must be quarantined"
        );
        assert!(
            livelock
                .cancelled_reason
                .as_deref()
                .unwrap_or("")
                .contains("livelock_quarantine"),
            "the reap reason must name the livelock quarantine, got {:?}",
            livelock.cancelled_reason
        );
        assert!(
            store.load("wf_await").await.unwrap().cancelled_at.is_none(),
            "an under-budget parked agent session must be left alone"
        );
    }
}

#[cfg(test)]
mod writable_repo_tests {
    //! FB-1 (reload rewire + empty-set fail-fast) and FB-3 (worktree/subpath
    //! selectors) — the writable-repo set and how a `start` selector resolves it.
    use super::*;
    use crate::audit::{AuditSink, MemoryAuditSink};
    use crate::guards::DefaultGuardEvaluator;
    use crate::run_env::RepoRoot;
    use crate::store::{ConfigDefinitionStore, InMemoryWorkflowStore};
    use serde_json::json;
    use std::sync::Arc;

    struct EmptyRegistry;
    impl ExecutorRegistry for EmptyRegistry {
        fn get(&self, _kind: &str) -> Option<Arc<dyn Executor>> {
            None
        }
    }

    fn runtime_with_roots(roots: Vec<RepoRoot>) -> WorkflowRuntime {
        let defs = Arc::new(ConfigDefinitionStore::from_config(&json!({
            "version": "1.0.0", "workflows": {}
        })));
        WorkflowRuntime::new(
            defs,
            Arc::new(InMemoryWorkflowStore::new()),
            Arc::new(EmptyRegistry),
            Arc::new(DefaultGuardEvaluator::new()),
            Arc::new(MemoryAuditSink::new()) as Arc<dyn AuditSink>,
        )
        .with_writable_repo_roots(roots)
    }

    fn runtime() -> WorkflowRuntime {
        runtime_with_roots(vec![])
    }

    #[test]
    fn exact_declared_root_resolves() {
        let td = tempfile::TempDir::new().unwrap();
        let root = RepoRoot::new(td.path()).unwrap();
        let rt = runtime_with_roots(vec![root.clone()]);
        let got = rt.resolve_run_repo_root(Some(root.as_str())).unwrap();
        assert_eq!(got, root);
    }

    #[test]
    fn subpath_of_a_declared_root_resolves_to_that_subpath() {
        // A git-worktree-like subdirectory under a declared root. It is NOT
        // pre-declared, but resolves because it lies within the allowlist.
        let td = tempfile::TempDir::new().unwrap();
        let root = RepoRoot::new(td.path()).unwrap();
        let sub = td.path().join("worktrees").join("feature-x");
        std::fs::create_dir_all(&sub).unwrap();
        let rt = runtime_with_roots(vec![root.clone()]);
        let got = rt
            .resolve_run_repo_root(Some(sub.to_str().unwrap()))
            .expect("a subpath of a declared root resolves");
        // The resolved root is the subpath itself, canonicalized.
        assert_eq!(got.as_path(), std::fs::canonicalize(&sub).unwrap());
        // …and it is genuinely under the declared root.
        assert!(got.as_path().starts_with(root.as_path()));
    }

    #[test]
    fn path_outside_every_declared_root_is_rejected() {
        let declared = tempfile::TempDir::new().unwrap();
        let outside = tempfile::TempDir::new().unwrap(); // a sibling temp dir
        let root = RepoRoot::new(declared.path()).unwrap();
        let rt = runtime_with_roots(vec![root]);
        let err = rt
            .resolve_run_repo_root(Some(outside.path().to_str().unwrap()))
            .expect_err("an out-of-tree path must be rejected");
        let msg = format!("{err:#}");
        assert!(
            msg.contains("REPO_ROOT_OUTSIDE_ALLOWLIST"),
            "expected allowlist rejection, got: {msg}"
        );
    }

    #[test]
    fn nonexistent_selector_is_rejected_as_unknown() {
        let td = tempfile::TempDir::new().unwrap();
        let root = RepoRoot::new(td.path()).unwrap();
        let rt = runtime_with_roots(vec![root]);
        let err = rt
            .resolve_run_repo_root(Some("/definitely/not/here/praxec-fb3-xyz"))
            .expect_err("a non-existent selector must be rejected");
        let msg = format!("{err:#}");
        assert!(
            msg.contains("REPO_ROOT_UNKNOWN"),
            "expected unknown-root rejection, got: {msg}"
        );
    }

    #[test]
    fn sibling_with_shared_string_prefix_is_not_a_subpath() {
        // `/tmp/repo-foo` must NOT be treated as under declared `/tmp/repo`:
        // component-wise `starts_with`, not string-prefix.
        let base = tempfile::TempDir::new().unwrap();
        let declared = base.path().join("repo");
        let sibling = base.path().join("repo-foo");
        std::fs::create_dir_all(&declared).unwrap();
        std::fs::create_dir_all(&sibling).unwrap();
        let root = RepoRoot::new(&declared).unwrap();
        let rt = runtime_with_roots(vec![root]);
        let err = rt
            .resolve_run_repo_root(Some(sibling.to_str().unwrap()))
            .expect_err("a string-prefix sibling must not resolve");
        assert!(format!("{err:#}").contains("REPO_ROOT_OUTSIDE_ALLOWLIST"));
    }

    /// FB-1(b) — an empty writable set makes `start`'s repo_root resolution fail
    /// fast (REPO_ROOT_REQUIRED), not defer to a later file-leaf.
    #[test]
    fn resolve_fails_fast_on_empty_writable_set() {
        let err = runtime()
            .resolve_run_repo_root(None)
            .unwrap_err()
            .to_string();
        assert!(err.contains("REPO_ROOT_REQUIRED"), "{err}");
    }

    /// FB-1(a) — `set_writable_repo_roots` (what `reload_gated` calls) hot-swaps the
    /// live set: resolution goes from failing to returning the new root, and the
    /// snapshot (the reload response) reflects it — no restart.
    #[test]
    fn set_writable_repo_roots_rewires_resolution_and_snapshot() {
        let rt = runtime();
        assert!(rt.resolve_run_repo_root(None).is_err());
        assert!(rt.writable_repo_roots_snapshot().is_empty());

        let root = RepoRoot::for_test();
        rt.set_writable_repo_roots(vec![root.clone()]);

        assert_eq!(
            rt.resolve_run_repo_root(None).unwrap().as_str(),
            root.as_str()
        );
        assert_eq!(
            rt.writable_repo_roots_snapshot(),
            vec![root.as_str().to_string()]
        );
    }

    /// The swap is visible through a CLONE of the runtime (the shared `Arc<RwLock>`
    /// slot) — reload swaps once, every handle sees it.
    #[test]
    fn swap_is_visible_through_a_runtime_clone() {
        let rt = runtime();
        let handle = rt.clone();
        rt.set_writable_repo_roots(vec![RepoRoot::for_test()]);
        assert!(
            handle.resolve_run_repo_root(None).is_ok(),
            "a runtime clone must see the swap"
        );
    }
}
