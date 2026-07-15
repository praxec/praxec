//! SPEC §24 — the `parallel` executor kind. Fans out N branches inside
//! ONE transition, aggregates their outputs/evidence, returns a single
//! `ExecuteResult`. The state machine stays singular — one state, one
//! transition, one version bump, one transition record — while the
//! executor internally runs branches concurrently.
//!
//! **Architecture invariant (SPEC §24.5):** fan-out happens inside one
//! executor invocation. Branches NEVER touch the WorkflowStore directly;
//! only the parent executor returns one `ExecuteResult` and the runtime
//! does exactly one `save_if_version` post-aggregation. Encapsulating
//! concurrency this way preserves every existing workflow invariant —
//! `WorkflowInstance::version` bumps once per submit, the audit log
//! shows exactly one `workflow.transition` event per submit, optimistic
//! locking still works.
//!
//! Config shape (full reference in SPEC §24.2):
//!
//! ```yaml
//! executor:
//!   kind: parallel
//!   branches:                          # static list OR { for_each, do }
//!     - { kind: script, subject: ... }
//!     - { kind: mcp,    connection: ..., tool: ... }
//!   join: all                          # all (default) | any | { at_least: K }
//!   max_concurrency: 4                 # required when branches.len() > 10
//!   on_branch_failure: bail            # bail (default) | continue
//!   total_timeout_ms: 60000            # optional
//!   max_recursion_depth: 3             # optional, default 3
//! ```
//!
//! Dynamic-branches form:
//!
//! ```yaml
//! branches:
//!   for_each: "$.context.queries"      # path resolves to a JSON array
//!   do:                                # template; $.branch.value, $.branch.index substituted
//!     kind: mcp
//!     connection: scip
//!     tool: lookup
//!     args: { symbol: "$.branch.value" }
//! ```

use std::sync::{Arc, OnceLock};
use std::time::{Duration, Instant};

use async_trait::async_trait;
use praxec_core::audit::{AuditEvent, AuditSink};
use praxec_core::error::ExecutorError;
use praxec_core::mapping::read_in_scopes;
use praxec_core::model::{Evidence, ExecuteRequest, ExecuteResult};
use praxec_core::ports::{Executor, ExecutorRegistry};
use praxec_core::reliability::{ReliabilityPolicy, execute_with_reliability};
use serde_json::{Map, Value, json};
use sha2::{Digest, Sha256};
use tokio::sync::Semaphore;
use tokio::task::JoinSet;
use tokio::time::timeout;

/// SPEC §24 — default `max_recursion_depth`. Three levels of nested
/// `parallel` is the deepest a sane architecture should need. Operators
/// override when they have a real need. The cap exists to catch
/// authoring bugs that produce 10^N branches accidentally.
pub const DEFAULT_MAX_RECURSION_DEPTH: u32 = 3;

/// SPEC §24 — at or above this count, `max_concurrency` is REQUIRED.
/// Speculative default; operators with strong opinions set
/// `max_concurrency` explicitly even for small fan-outs.
pub const UNGOVERNED_CAP_THRESHOLD: usize = 10;

tokio::task_local! {
    /// Per-task recursion counter. Incremented when entering a
    /// `ParallelExecutor::execute` invocation; checked against
    /// `max_recursion_depth` to reject nested parallel that exceeds the
    /// cap. Reset per top-level transition.
    static PARALLEL_DEPTH: u32;
}

pub struct ParallelExecutor {
    /// Set after registry construction (chicken-and-egg: the registry
    /// contains the parallel executor which needs the registry).
    /// `OnceLock` ensures it's wired exactly once, after which all
    /// invocations see the same `Arc`.
    pub(crate) executors: Arc<OnceLock<Arc<dyn ExecutorRegistry>>>,
    pub(crate) audit: Arc<dyn AuditSink>,
}

impl ParallelExecutor {
    pub fn new(audit: Arc<dyn AuditSink>) -> Self {
        Self {
            executors: Arc::new(OnceLock::new()),
            audit,
        }
    }

    /// Wire the executor registry after the registry itself is built.
    /// Must be called exactly once during construction.
    ///
    /// A second call is a construction bug: silently keeping the first
    /// registry (the old `let _ = set()` behavior) would route every parallel
    /// sub-step through a stale registry. Panic instead so the mistake can't
    /// hide.
    pub fn set_registry(&self, registry: Arc<dyn ExecutorRegistry>) {
        if self.executors.set(registry).is_err() {
            panic!(
                "PARALLEL_EXECUTOR_DOUBLE_WIRED: set_registry called more than once; \
                 the executor registry must be wired exactly once after construction."
            );
        }
    }

    fn registry(&self) -> Result<Arc<dyn ExecutorRegistry>, ExecutorError> {
        self.executors.get().cloned().ok_or_else(|| {
            ExecutorError::Permanent(
                "PARALLEL_EXECUTOR_NOT_WIRED: registry was not set after construction. \
                 Call ParallelExecutor::set_registry(registry) after building the \
                 registry that contains this executor."
                    .into(),
            )
        })
    }
}

#[async_trait]
impl Executor for ParallelExecutor {
    async fn execute(&self, request: ExecuteRequest) -> Result<ExecuteResult, ExecutorError> {
        let cfg = ParallelConfig::from_value(&request.executor_config)?;

        // SPEC §24.5 — defensive snapshot-hash assert (F1).
        // Hash the snapshot bytes at fan-out start; re-hash at aggregation.
        // Mismatch = `PARALLEL_SNAPSHOT_MUTATED`. Rust's borrow rules make
        // mutation across threads structurally impossible (the snapshot
        // is in an Arc), so this catches future regressions only.
        let snapshot_hash_pre = hash_snapshot(&request.workflow.definition);

        // SPEC §24 — recursion depth check via task_local. Entering the
        // executor increments; the spawned branches inherit the current
        // depth. Exceeding `max_recursion_depth` rejects fail-fast.
        let current_depth = PARALLEL_DEPTH.try_with(|d| *d).unwrap_or(0);
        if current_depth >= cfg.max_recursion_depth {
            return Err(ExecutorError::Permanent(format!(
                "PARALLEL_DEPTH_EXCEEDED: nested parallel reached depth {} (cap {}). \
                 Override with `max_recursion_depth` on the parallel executor config \
                 if this nesting is intentional; default cap exists to catch \
                 authoring bugs that produce exponential fan-out.",
                current_depth + 1,
                cfg.max_recursion_depth
            )));
        }

        // Resolve branches: static literal OR dynamic for_each expansion.
        let branches = resolve_branches(&cfg, &request)?;
        let n = branches.len();

        // SPEC §24 F4 — DOS poka-yoke. At/above UNGOVERNED_CAP_THRESHOLD
        // branches, `max_concurrency` is required. Empty for_each → vacuous
        // success (no branches to bound).
        if n >= UNGOVERNED_CAP_THRESHOLD && cfg.max_concurrency.is_none() {
            return Err(ExecutorError::Permanent(format!(
                "INVALID_PARALLEL_CONFIG: {n} branches without `max_concurrency` cap. \
                 At {UNGOVERNED_CAP_THRESHOLD}+ branches, set `max_concurrency: N` \
                 explicitly to prevent runaway resource consumption against rate-limited \
                 downstreams (SPEC §24, F4 mitigation)."
            )));
        }

        // SPEC §24.5 / F9 — empty for_each is vacuous success.
        if n == 0 {
            self.audit
                .record(
                    AuditEvent::new("parallel.fanout.empty")
                        .with_workflow(&request.workflow.id)
                        .with_correlation(request.correlation_id.as_deref().unwrap_or("unset-corr"))
                        .with_payload(json!({
                            "transition": request.transition,
                            "for_each":   cfg.for_each_path,
                        })),
                )
                .await
                .unwrap_or_else(|e| tracing::warn!(error = %e, "audit emit failed; event dropped"));
            return Ok(ExecuteResult {
                output: empty_summary_output(&cfg),
                evidence: vec![],
                child_workflow_id: None,
                next_transition: None,
                suspend: None,
                telemetry: None,
            });
        }

        let registry = self.registry()?;
        let correlation_id = request
            .correlation_id
            .clone()
            .unwrap_or_else(|| "unset-corr".to_string());
        let parent_idem = request.idempotency_key.clone();
        let parent_workflow_id = request.workflow.id.clone();
        let transition = request.transition.clone();

        // Concurrency cap. None → unbounded (only safe for small n; we
        // already rejected n >= threshold without explicit cap above).
        let semaphore = Arc::new(Semaphore::new(cfg.max_concurrency.unwrap_or(n)));

        let start = Instant::now();
        let mut joinset: JoinSet<(usize, Result<ExecuteResult, ExecutorError>)> = JoinSet::new();
        let mut max_in_flight: usize = 0;
        let current_in_flight: Arc<std::sync::atomic::AtomicUsize> =
            Arc::new(std::sync::atomic::AtomicUsize::new(0));

        for (index, branch_cfg) in branches.iter().enumerate() {
            let permit_owner = semaphore.clone();
            let registry_owner = registry.clone();
            let audit_owner = self.audit.clone();
            let instance_owner = request.workflow.clone();
            let arguments_owner = request.arguments.clone();
            let cfg_owner = branch_cfg.clone();
            let correlation_owner = correlation_id.clone();
            let parent_idem_owner = parent_idem.clone();
            let transition_owner = transition.clone();
            let parent_workflow_id_owner = parent_workflow_id.clone();
            let in_flight_owner = current_in_flight.clone();
            let depth_for_branch = current_depth + 1;

            joinset.spawn(PARALLEL_DEPTH.scope(depth_for_branch, async move {
                // Permit acquire — bounded concurrency.
                let _permit = permit_owner
                    .acquire_owned()
                    .await
                    .expect("semaphore not closed");
                let in_flight_now =
                    in_flight_owner.fetch_add(1, std::sync::atomic::Ordering::SeqCst) + 1;
                let _ = (in_flight_now,); // captured for max-observed below

                // SPEC §24 — emit per-branch started event with the parent's
                // correlation_id + branch_index payload.
                audit_owner
                    .record(
                        AuditEvent::new("parallel.branch.started")
                            .with_workflow(&parent_workflow_id_owner)
                            .with_correlation(&correlation_owner)
                            .with_payload(json!({
                                "transition":    transition_owner,
                                "branch_index":  index,
                                "branch_executor_kind": cfg_owner.get("kind").and_then(Value::as_str),
                            })),
                    )
                    .await
                    .unwrap_or_else(|e| tracing::warn!(error = %e, "audit emit failed; event dropped"));

                // SPEC §24 F7 — idempotency-key segmentation. If the branch
                // declares `idempotencyKey: true`, swap the boolean for a
                // template that segments by branch index. `:branch:<N>`
                // suffix makes each branch's key unique while still stable
                // across retries of the SAME branch.
                let _ = parent_idem_owner; // unused — segmentation happens in-config below
                let branch_cfg_for_reliability =
                    segment_branch_idempotency_key(cfg_owner.clone(), index);

                // Wrap branch in its own ExecuteRequest at the same
                // workflow + transition + arguments; the branch's executor
                // config IS the per-branch executor spec.
                //
                // We invoke `execute_with_reliability` against the branch
                // config so each branch gets its own reliability envelope
                // (timeout/retry/fallback). Per-branch policies come from
                // the branch's own `reliability:` block.
                let branch_policy =
                    ReliabilityPolicy::from_value(branch_cfg_for_reliability.get("reliability"));

                let started_at = Instant::now();
                // A present-but-malformed `reliability:` block on a branch
                // fails that branch (rather than silently running it with
                // default reliability).
                let result = match branch_policy {
                    Ok(branch_policy) => {
                        execute_with_reliability(
                            registry_owner.as_ref(),
                            &audit_owner,
                            &instance_owner,
                            transition_owner.as_deref(),
                            &arguments_owner,
                            branch_cfg_for_reliability,
                            &branch_policy,
                            &correlation_owner,
                        )
                        .await
                    }
                    Err(e) => Err(ExecutorError::Permanent(e.to_string())),
                };
                let duration_ms = started_at.elapsed().as_millis() as u64;

                let event_kind = if result.is_ok() {
                    "parallel.branch.completed"
                } else {
                    "parallel.branch.failed"
                };
                audit_owner
                    .record(
                        AuditEvent::new(event_kind)
                            .with_workflow(&parent_workflow_id_owner)
                            .with_correlation(&correlation_owner)
                            .with_payload(json!({
                                "transition":   transition_owner,
                                "branch_index": index,
                                "durationMs":   duration_ms,
                                "error": result.as_ref().err().map(|e| e.to_string()),
                            })),
                    )
                    .await
                    .unwrap_or_else(|e| tracing::warn!(error = %e, "audit emit failed; event dropped"));

                in_flight_owner.fetch_sub(1, std::sync::atomic::Ordering::SeqCst);
                (index, result)
            }));
        }

        // Drain join condition. Apply on_branch_failure semantics.
        let join_result = match cfg.total_timeout {
            Some(d) => timeout(
                d,
                drive_joinset(
                    &mut joinset,
                    &cfg.join,
                    cfg.on_branch_failure,
                    n,
                    &current_in_flight,
                    &mut max_in_flight,
                ),
            )
            .await
            .map_err(|_| {
                ExecutorError::Timeout(cfg.total_timeout.map(|d| d.as_millis() as u64).unwrap_or(0))
            })?,
            None => {
                drive_joinset(
                    &mut joinset,
                    &cfg.join,
                    cfg.on_branch_failure,
                    n,
                    &current_in_flight,
                    &mut max_in_flight,
                )
                .await
            }
        };

        // Snapshot defensive assert post-aggregation (F1).
        let snapshot_hash_post = hash_snapshot(&request.workflow.definition);
        if snapshot_hash_pre != snapshot_hash_post {
            return Err(ExecutorError::Permanent(format!(
                "PARALLEL_SNAPSHOT_MUTATED: workflow snapshot hash diverged during \
                 parallel fan-out (pre={snapshot_hash_pre}, post={snapshot_hash_post}). \
                 This is a runtime invariant violation — branches must NEVER mutate \
                 the snapshot (SPEC §8.2)."
            )));
        }

        // Build aggregated output + evidence.
        let elapsed_ms = start.elapsed().as_millis() as u64;
        let JoinOutcome {
            branch_results,
            aggregated_evidence,
            ok_count,
            failed_count,
            cancelled_count,
            first_failure_index,
        } = join_result;

        // Compute verdict — closed shortcuts are cheap; aggregator
        // dispatches through the registry (kind: expression evaluates
        // inline, kind: script/mcp/rest/etc invokes the registered
        // executor with branches as input).
        let verdict = compute_verdict(
            &cfg.join,
            n,
            ok_count,
            failed_count,
            cancelled_count,
            &branch_results,
            registry.as_ref(),
            &request,
        )
        .await?;

        // SPEC §24.4 (GAP-G) — emit a `parallel.branch.cancelled` audit
        // event for each branch whose result slot ended as cancelled (the
        // stub error code emitted by `drive_joinset` when a JoinSet abort
        // dropped the in-flight future). Operators need per-branch
        // visibility into which branches were dropped vs which never ran.
        for branch in &branch_results {
            if branch.get("ok").and_then(Value::as_bool) == Some(false)
                && branch.pointer("/error/code").and_then(Value::as_str) == Some("cancelled")
            {
                let branch_index = branch.get("index").and_then(Value::as_u64);
                self.audit
                    .record(
                        AuditEvent::new("parallel.branch.cancelled")
                            .with_workflow(&request.workflow.id)
                            .with_correlation(&correlation_id)
                            .with_payload(json!({
                                "transition":    transition,
                                "branch_index":  branch_index,
                                "reason":        "cancelled-by-join-or-failure-mode",
                            })),
                    )
                    .await
                    .unwrap_or_else(
                        |e| tracing::warn!(error = %e, "audit emit failed; event dropped"),
                    );
            }
        }

        // Emit aggregate fan-out event.
        let summary = json!({
            "n":                       n,
            "ok_count":                ok_count,
            "failed_count":            failed_count,
            "cancelled_count":         cancelled_count,
            "durationMs":              elapsed_ms,
            "first_failure_index":     first_failure_index,
            "max_in_flight_observed":  max_in_flight,
            "join":                    cfg.join.as_token(),
            "verdict":                 verdict.as_token(),
        });
        self.audit
            .record(
                AuditEvent::new("parallel.fanout.completed")
                    .with_workflow(&request.workflow.id)
                    .with_correlation(&correlation_id)
                    .with_payload(json!({
                        "transition": transition,
                        "summary":    summary,
                    })),
            )
            .await
            .unwrap_or_else(|e| tracing::warn!(error = %e, "audit emit failed; event dropped"));

        let output = json!({
            "branches": branch_results,
            "summary":  summary,
        });

        match verdict {
            Verdict::Succeeded => Ok(ExecuteResult {
                output,
                evidence: aggregated_evidence,
                child_workflow_id: None,
                next_transition: None,
                suspend: None,
                telemetry: None,
            }),
            Verdict::Failed => Err(ExecutorError::Permanent(format!(
                "parallel fan-out failed: join={}, ok={}/{}, first_failure_index={:?}",
                cfg.join.as_token(),
                ok_count,
                n,
                first_failure_index
            ))),
            Verdict::ThresholdNotMet => Err(ExecutorError::Permanent(format!(
                "JOIN_THRESHOLD_NOT_MET: join={} required threshold not reached \
                 (ok={}/{}, failed={}, cancelled={})",
                cfg.join.as_token(),
                ok_count,
                n,
                failed_count,
                cancelled_count
            ))),
        }
    }
}

/// SPEC §24.2 — compute the verdict after fan-out completes.
///
/// Closed shortcuts (All / Any / AtLeast / Percent) compute inline.
/// The `Aggregator` variant dispatches:
/// - `kind: expression` evaluates the expression in-process (no
///   registry dispatch — fastest path for the common case).
/// - Any other `kind:` invokes the registered executor with
///   `arguments = { branches, ok_count, failed_count, cancelled_count,
///   n }`. The executor must return an output containing
///   `verdict: "succeeded" | "failed" | "threshold_not_met"`;
///   missing / invalid verdict yields `AGGREGATOR_INVALID_VERDICT`.
#[allow(clippy::too_many_arguments)]
async fn compute_verdict(
    join: &JoinCondition,
    n: usize,
    ok_count: usize,
    failed_count: usize,
    cancelled_count: usize,
    branch_results: &[Value],
    registry: &dyn ExecutorRegistry,
    request: &ExecuteRequest,
) -> Result<Verdict, ExecutorError> {
    match join {
        JoinCondition::All => {
            if failed_count == 0 && cancelled_count == 0 && ok_count == n {
                Ok(Verdict::Succeeded)
            } else {
                Ok(Verdict::Failed)
            }
        }
        JoinCondition::Any => {
            if ok_count >= 1 {
                Ok(Verdict::Succeeded)
            } else {
                Ok(Verdict::Failed)
            }
        }
        JoinCondition::AtLeast(k) => {
            if ok_count >= *k {
                Ok(Verdict::Succeeded)
            } else {
                Ok(Verdict::ThresholdNotMet)
            }
        }
        JoinCondition::Percent(p) => {
            if n == 0 {
                Ok(Verdict::Succeeded)
            } else {
                let threshold = JoinCondition::percent_threshold(*p, n);
                if ok_count >= threshold {
                    Ok(Verdict::Succeeded)
                } else {
                    Ok(Verdict::ThresholdNotMet)
                }
            }
        }
        JoinCondition::Aggregator(cfg) => {
            let aggregator_input = json!({
                "branches":        branch_results,
                "ok_count":        ok_count,
                "failed_count":    failed_count,
                "cancelled_count": cancelled_count,
                "n":               n,
            });
            let kind = cfg.get("kind").and_then(Value::as_str).unwrap_or("");

            // Fast path: inline expression evaluation (no registry).
            if kind == "expression" {
                let expr = match cfg.get("expr").and_then(Value::as_str) {
                    Some(e) => e,
                    None => {
                        tracing::warn!("aggregator kind=expression missing `expr` field");
                        return Ok(Verdict::Failed);
                    }
                };
                return match praxec_core::guards::evaluate_join_expression(expr, &aggregator_input)
                {
                    Ok(true) => Ok(Verdict::Succeeded),
                    Ok(false) => Ok(Verdict::Failed),
                    Err(e) => {
                        tracing::warn!(
                            join_expression = %expr,
                            error = %e,
                            "join expression evaluation errored — treating as failed"
                        );
                        Ok(Verdict::Failed)
                    }
                };
            }

            // General path: dispatch through the executor registry.
            let executor = match registry.get(kind) {
                Some(e) => e,
                None => {
                    tracing::warn!(
                        aggregator_kind = %kind,
                        "aggregator kind not registered — treating verdict as failed"
                    );
                    return Ok(Verdict::Failed);
                }
            };
            let agg_request = ExecuteRequest {
                workflow: request.workflow.clone(),
                transition: request.transition.clone(),
                arguments: aggregator_input,
                executor_config: cfg.clone(),
                idempotency_key: request
                    .idempotency_key
                    .as_ref()
                    .map(|k| format!("{k}:aggregator")),
                correlation_id: request.correlation_id.clone(),
            };
            match executor.execute(agg_request).await {
                Ok(res) => {
                    let v = res
                        .output
                        .get("verdict")
                        .and_then(Value::as_str)
                        .unwrap_or("");
                    match v {
                        "succeeded" => Ok(Verdict::Succeeded),
                        "threshold_not_met" => Ok(Verdict::ThresholdNotMet),
                        "failed" | "" => Ok(Verdict::Failed),
                        other => {
                            tracing::warn!(
                                aggregator_verdict = %other,
                                "AGGREGATOR_INVALID_VERDICT: aggregator returned unknown verdict; treating as failed"
                            );
                            Ok(Verdict::Failed)
                        }
                    }
                }
                // The aggregator EXECUTOR itself errored (e.g. a `rest`
                // aggregator hit a 503, a `script` aggregator crashed) — this
                // is an infrastructure failure, NOT a business "the join
                // failed" verdict. Propagate the original error so its
                // Transient/Permanent classification (and root-cause detail)
                // survive instead of being flattened into a permanent failed
                // verdict that loses retryability.
                Err(e) => Err(e),
            }
        }
    }
}

// ── join / failure / output types ─────────────────────────────────────────

/// Join condition. Closed shortcuts (`All`, `Any`, `AtLeast`,
/// `Percent`) are the ergonomic surface for the common cases.
/// `Aggregator` is the **general form** — any executor-shaped value
/// invoked post-fan-out that consumes `{branches[], ok_count, ...}`
/// and returns a verdict. Aggregator subsumes the `expression` join:
/// `expression: "<expr>"` is sugar for `aggregator: { kind: expression,
/// expr: "<expr>" }`.
#[derive(Debug, Clone)]
enum JoinCondition {
    /// Every branch must succeed.
    All,
    /// First success wins; cancel siblings.
    Any,
    /// At least K branches must succeed.
    AtLeast(usize),
    /// At least P percent of branches must succeed (`0..=100`).
    /// Vacuous fan-out (n=0) succeeds. Early exit symmetric to AtLeast.
    Percent(u8),
    /// General aggregator. Holds an executor-shaped config; verdict is
    /// computed post-fan-out by invoking the configured aggregator with
    /// the branches map as input.
    ///
    /// Built-in `kind: expression` evaluates inline (no registry
    /// dispatch). Every other `kind:` goes through the executor
    /// registry, so operators can write aggregators as scripts, MCP
    /// tools, REST calls, or nested workflows.
    Aggregator(Value),
}

impl JoinCondition {
    fn as_token(&self) -> &'static str {
        match self {
            JoinCondition::All => "all",
            JoinCondition::Any => "any",
            JoinCondition::AtLeast(_) => "at_least",
            JoinCondition::Percent(_) => "percent",
            JoinCondition::Aggregator(cfg) => cfg
                .get("kind")
                .and_then(Value::as_str)
                .map(|k| match k {
                    "expression" => "expression",
                    "script" => "aggregator_script",
                    "mcp" => "aggregator_mcp",
                    "rest" => "aggregator_rest",
                    "workflow" => "aggregator_workflow",
                    "cli" => "aggregator_cli",
                    _ => "aggregator",
                })
                .unwrap_or("aggregator"),
        }
    }

    /// Compute the integer success threshold for percent. Uses
    /// ceiling division to avoid silently rounding 51% of 3 (=1.53)
    /// down to 1 when the operator clearly meant "more than half".
    fn percent_threshold(p: u8, n: usize) -> usize {
        let p = p as usize;
        // ceil(p * n / 100) via integer div_ceil.
        (p * n).div_ceil(100)
    }
}

#[derive(Debug, Clone, Copy)]
enum OnBranchFailure {
    /// First failure cancels in-flight siblings; whole executor fails fast.
    Bail,
    /// All branches run regardless; verdict is per-join-condition.
    Continue,
}

#[derive(Debug, Clone, Copy)]
enum Verdict {
    Succeeded,
    Failed,
    ThresholdNotMet,
}

impl Verdict {
    fn as_token(self) -> &'static str {
        match self {
            Verdict::Succeeded => "succeeded",
            Verdict::Failed => "failed",
            Verdict::ThresholdNotMet => "threshold_not_met",
        }
    }
}

/// What `drive_joinset` returns — counts + raw branch results. The
/// final verdict is computed by [`compute_verdict`] which dispatches
/// to the aggregator pattern when configured.
struct JoinOutcome {
    branch_results: Vec<Value>,
    aggregated_evidence: Vec<Evidence>,
    ok_count: usize,
    failed_count: usize,
    cancelled_count: usize,
    first_failure_index: Option<usize>,
}

async fn drive_joinset(
    joinset: &mut JoinSet<(usize, Result<ExecuteResult, ExecutorError>)>,
    join: &JoinCondition,
    on_failure: OnBranchFailure,
    n: usize,
    in_flight: &Arc<std::sync::atomic::AtomicUsize>,
    max_in_flight: &mut usize,
) -> JoinOutcome {
    let mut branch_results: Vec<Option<Value>> = vec![None; n];
    let mut aggregated_evidence: Vec<Evidence> = Vec::new();
    let mut ok_count = 0usize;
    let mut failed_count = 0usize;
    let mut first_failure_index: Option<usize> = None;
    let mut early_exit = false;

    while let Some(joined) = joinset.join_next().await {
        // Track high-water mark for in-flight branches (observability for F4).
        let live = in_flight.load(std::sync::atomic::Ordering::SeqCst);
        if live > *max_in_flight {
            *max_in_flight = live;
        }

        let (index, result) = match joined {
            Ok(pair) => pair,
            Err(join_err) => {
                // Task panicked or was cancelled. Treat as failure for that index.
                tracing::warn!(error = ?join_err, "parallel branch task join error");
                failed_count += 1;
                continue;
            }
        };
        match result {
            Ok(res) => {
                ok_count += 1;
                aggregated_evidence.extend(res.evidence);
                branch_results[index] = Some(json!({
                    "ok":     true,
                    "index":  index,
                    "output": res.output,
                }));
                // Join: any → first success returns; cancel siblings.
                if matches!(join, JoinCondition::Any) {
                    joinset.abort_all();
                    early_exit = true;
                    break;
                }
                // Join: at_least K → if reached, succeed early.
                if let JoinCondition::AtLeast(k) = join {
                    if ok_count >= *k {
                        joinset.abort_all();
                        early_exit = true;
                        break;
                    }
                }
                // Join: percent → if threshold reached, succeed early.
                if let JoinCondition::Percent(p) = join {
                    let threshold = JoinCondition::percent_threshold(*p, n);
                    if ok_count >= threshold {
                        joinset.abort_all();
                        early_exit = true;
                        break;
                    }
                }
                // Join: expression → NO early exit. Expression evaluates
                // post-completion to keep semantics structurally clean
                // (no mid-flight branch reads — SPEC §24.8).
            }
            Err(err) => {
                if first_failure_index.is_none() {
                    first_failure_index = Some(index);
                }
                failed_count += 1;
                branch_results[index] = Some(json!({
                    "ok":    false,
                    "index": index,
                    "error": {
                        "code":    err.class().token(),
                        "message": err.to_string(),
                    },
                }));
                if matches!(on_failure, OnBranchFailure::Bail) {
                    joinset.abort_all();
                    early_exit = true;
                    break;
                }
                // join: at_least:K + a failure that makes K unreachable → bail early.
                if let JoinCondition::AtLeast(k) = join {
                    let remaining = n - ok_count - failed_count;
                    if ok_count + remaining < *k {
                        joinset.abort_all();
                        early_exit = true;
                        break;
                    }
                }
                // join: percent + a failure that makes the threshold unreachable → bail.
                if let JoinCondition::Percent(p) = join {
                    let threshold = JoinCondition::percent_threshold(*p, n);
                    let remaining = n - ok_count - failed_count;
                    if ok_count + remaining < threshold {
                        joinset.abort_all();
                        early_exit = true;
                        break;
                    }
                }
                // join: expression → no early exit (see Ok-branch comment).
            }
        }
    }
    if early_exit {
        // Drain remaining tasks. A task may have already completed
        // (its result is queued in the JoinSet's channel waiting to be
        // received) — capture those results into branch_results so the
        // aggregated output reflects what actually happened. Only TRUE
        // cancellations (JoinError, meaning the future was dropped
        // before it produced a value) leave the slot as None, which the
        // outer code stubs as "cancelled".
        while let Some(joined) = joinset.join_next().await {
            match joined {
                Ok((index, Ok(res))) => {
                    // Already completed; preserve the real outcome.
                    if branch_results[index].is_none() {
                        ok_count += 1;
                        aggregated_evidence.extend(res.evidence);
                        branch_results[index] = Some(json!({
                            "ok":     true,
                            "index":  index,
                            "output": res.output,
                        }));
                    }
                }
                Ok((index, Err(err))) => {
                    if branch_results[index].is_none() {
                        failed_count += 1;
                        if first_failure_index.is_none() {
                            first_failure_index = Some(index);
                        }
                        branch_results[index] = Some(json!({
                            "ok":    false,
                            "index": index,
                            "error": {
                                "code":    err.class().token(),
                                "message": err.to_string(),
                            },
                        }));
                    }
                }
                Err(_join_err) => {
                    // True cancellation — task was aborted before producing
                    // a value. Slot stays None; the outer fill loop stubs
                    // it as cancelled, and the outer event-emission loop
                    // picks it up.
                }
            }
        }
    }

    let cancelled_count = n.saturating_sub(ok_count).saturating_sub(failed_count);

    // Fill any unfilled (cancelled-before-completion) slots with a stub.
    let branch_results: Vec<Value> = branch_results
        .into_iter()
        .enumerate()
        .map(|(i, slot)| {
            slot.unwrap_or_else(|| {
                json!({
                    "ok":    false,
                    "index": i,
                    "error": { "code": "cancelled", "message": "branch cancelled before completion" },
                })
            })
        })
        .collect();

    JoinOutcome {
        branch_results,
        aggregated_evidence,
        ok_count,
        failed_count,
        cancelled_count,
        first_failure_index,
    }
}

// ── config parsing ────────────────────────────────────────────────────────

struct ParallelConfig {
    join: JoinCondition,
    max_concurrency: Option<usize>,
    on_branch_failure: OnBranchFailure,
    total_timeout: Option<Duration>,
    max_recursion_depth: u32,
    branches_spec: BranchesSpec,
    /// Surface the for_each path string (when dynamic) for the empty-fanout audit event.
    for_each_path: Option<String>,
}

enum BranchesSpec {
    Literal(Vec<Value>),
    ForEach {
        for_each: String,
        do_template: Value,
        /// SPEC §24.2 — optional pre-fan-out filter. When `Some(expr)`,
        /// each element of `for_each` is evaluated by
        /// [`praxec_core::guards::evaluate_join_expression`] with
        /// the element's `{value, index, ...}` projection as the root
        /// value. Falsy elements are dropped BEFORE branches spawn.
        /// Avoids the "add a state just to filter" antipattern.
        where_clause: Option<String>,
        /// Spec A §7.1 — the **map boundary** typed contract. When the `do:`
        /// worker template declares an `inputSchema`, every item that survives
        /// the `where:` filter (i.e. is actually handed to a worker) is
        /// validated against it BEFORE the branch spawns. Registry-aware, so a
        /// `{ "$ref": "praxec://hop#/$defs/<slot>In" }` contract resolves
        /// against the shipped HOP vocabulary. The schema is lifted OFF the
        /// worker template here so it is a boundary contract only, never passed
        /// down as a config field the worker executor would see.
        item_input_schema: Option<Value>,
    },
}

impl ParallelConfig {
    fn from_value(cfg: &Value) -> Result<Self, ExecutorError> {
        let branches_raw = cfg.get("branches").ok_or_else(|| {
            ExecutorError::Permanent(
                "INVALID_PARALLEL_CONFIG: missing `branches` (required: literal array OR \
                 `{for_each: <path>, do: <executor config>}`)"
                    .into(),
            )
        })?;
        let (branches_spec, for_each_path) = if let Some(arr) = branches_raw.as_array() {
            (BranchesSpec::Literal(arr.clone()), None)
        } else if let Some(obj) = branches_raw.as_object() {
            let for_each = obj
                .get("for_each")
                .and_then(Value::as_str)
                .ok_or_else(|| {
                    ExecutorError::Permanent(
                        "INVALID_PARALLEL_CONFIG: dynamic `branches` requires `for_each: <path>` \
                         (string path resolving to a JSON array)"
                            .into(),
                    )
                })?
                .to_string();
            let mut do_template = obj
                .get("do")
                .ok_or_else(|| {
                    ExecutorError::Permanent(
                        "INVALID_PARALLEL_CONFIG: dynamic `branches` requires `do: <executor config>` \
                         (per-branch executor template)"
                            .into(),
                    )
                })?
                .clone();
            // Spec A §7.1 — lift the map-boundary `inputSchema` OFF the worker
            // template so it is a boundary contract only, never a config field
            // the worker executor sees. A non-object schema is a config error
            // (a JSON Schema is always an object or a bool; we require object).
            let item_input_schema = match do_template.as_object_mut() {
                Some(do_obj) => match do_obj.remove("inputSchema") {
                    None => None,
                    Some(s @ Value::Object(_)) => Some(s),
                    Some(_) => {
                        return Err(ExecutorError::Permanent(
                            "INVALID_PARALLEL_CONFIG: dynamic `branches.do.inputSchema` must be a \
                             JSON Schema object (the map-boundary per-item input contract)"
                                .into(),
                        ));
                    }
                },
                None => None,
            };
            let where_clause = obj
                .get("where")
                .map(|w| {
                    w.as_str().ok_or_else(|| {
                        ExecutorError::Permanent(
                            "INVALID_PARALLEL_CONFIG: dynamic `branches.where` must be a \
                             string expression (paths `$.value`, `$.index`, plus literals \
                             and binary comparisons)"
                                .into(),
                        )
                    })
                })
                .transpose()?
                .map(|s| s.to_string());
            if let Some(w) = &where_clause {
                if w.trim().is_empty() {
                    return Err(ExecutorError::Permanent(
                        "INVALID_PARALLEL_CONFIG: dynamic `branches.where` must be non-empty when present".into(),
                    ));
                }
            }
            (
                BranchesSpec::ForEach {
                    for_each: for_each.clone(),
                    do_template,
                    where_clause,
                    item_input_schema,
                },
                Some(for_each),
            )
        } else {
            return Err(ExecutorError::Permanent(
                "INVALID_PARALLEL_CONFIG: `branches` must be either an array of executor configs \
                 OR an object with `for_each` + `do`"
                    .into(),
            ));
        };

        let join = match cfg.get("join") {
            None => JoinCondition::All,
            Some(Value::String(s)) if s == "all" => JoinCondition::All,
            Some(Value::String(s)) if s == "any" => JoinCondition::Any,
            Some(Value::Object(o)) if o.contains_key("at_least") => {
                let k = o.get("at_least").and_then(Value::as_u64).ok_or_else(|| {
                    ExecutorError::Permanent(
                        "INVALID_PARALLEL_CONFIG: `join.at_least` must be a positive integer"
                            .into(),
                    )
                })?;
                if k == 0 {
                    return Err(ExecutorError::Permanent(
                        "INVALID_PARALLEL_CONFIG: `join.at_least` must be > 0 (got 0)".into(),
                    ));
                }
                JoinCondition::AtLeast(k as usize)
            }
            Some(Value::Object(o)) if o.contains_key("percent") => {
                let p = o.get("percent").and_then(Value::as_u64).ok_or_else(|| {
                    ExecutorError::Permanent(
                        "INVALID_PARALLEL_CONFIG: `join.percent` must be an integer in 0..=100"
                            .into(),
                    )
                })?;
                if p > 100 {
                    return Err(ExecutorError::Permanent(format!(
                        "INVALID_PARALLEL_CONFIG: `join.percent` must be in 0..=100 (got {p})"
                    )));
                }
                JoinCondition::Percent(p as u8)
            }
            Some(Value::Object(o)) if o.contains_key("expression") => {
                // Backward-compat sugar — `expression: "..."` desugars
                // to `aggregator: { kind: expression, expr: "..." }`.
                let e = o.get("expression").and_then(Value::as_str).ok_or_else(|| {
                    ExecutorError::Permanent(
                        "INVALID_PARALLEL_CONFIG: `join.expression` must be a string".into(),
                    )
                })?;
                if e.trim().is_empty() {
                    return Err(ExecutorError::Permanent(
                        "INVALID_PARALLEL_CONFIG: `join.expression` must be non-empty".into(),
                    ));
                }
                JoinCondition::Aggregator(json!({
                    "kind": "expression",
                    "expr": e,
                }))
            }
            Some(Value::Object(o)) if o.contains_key("aggregator") => {
                let agg = o
                    .get("aggregator")
                    .ok_or_else(|| {
                        ExecutorError::Permanent(
                            "INVALID_PARALLEL_CONFIG: `join.aggregator` must be an object".into(),
                        )
                    })?
                    .clone();
                if !agg.is_object() {
                    return Err(ExecutorError::Permanent(
                        "INVALID_PARALLEL_CONFIG: `join.aggregator` must be an executor-shaped \
                         object with at least `kind:`"
                            .into(),
                    ));
                }
                let kind = agg.get("kind").and_then(Value::as_str).unwrap_or("");
                if kind.is_empty() {
                    return Err(ExecutorError::Permanent(
                        "INVALID_PARALLEL_CONFIG: `join.aggregator.kind` is required (e.g. \
                         `expression`, `script`, `mcp`, `rest`, `workflow`, `cli`)"
                            .into(),
                    ));
                }
                if kind == "expression" {
                    let expr = agg.get("expr").and_then(Value::as_str).unwrap_or("");
                    if expr.trim().is_empty() {
                        return Err(ExecutorError::Permanent(
                            "INVALID_PARALLEL_CONFIG: `join.aggregator.kind = expression` \
                             requires non-empty `expr:` field"
                                .into(),
                        ));
                    }
                }
                JoinCondition::Aggregator(agg)
            }
            Some(other) => {
                return Err(ExecutorError::Permanent(format!(
                    "INVALID_PARALLEL_CONFIG: unknown `join` value: {other}. Allowed: \"all\", \
                     \"any\", {{at_least: K}}, {{percent: 0..=100}}, \
                     {{aggregator: {{kind: ..., ...}}}}, or sugar {{expression: \"<expr>\"}}"
                )));
            }
        };

        let max_concurrency = cfg
            .get("max_concurrency")
            .and_then(Value::as_u64)
            .map(|n| n as usize);
        if let Some(0) = max_concurrency {
            return Err(ExecutorError::Permanent(
                "INVALID_PARALLEL_CONFIG: `max_concurrency` must be positive (got 0)".into(),
            ));
        }

        let on_branch_failure = match cfg
            .get("on_branch_failure")
            .and_then(Value::as_str)
            .unwrap_or("bail")
        {
            "bail" => OnBranchFailure::Bail,
            "continue" => OnBranchFailure::Continue,
            other => {
                return Err(ExecutorError::Permanent(format!(
                    "INVALID_PARALLEL_CONFIG: `on_branch_failure` must be \"bail\" or \"continue\" \
                     (got \"{other}\")"
                )));
            }
        };

        let total_timeout = cfg
            .get("total_timeout_ms")
            .and_then(Value::as_u64)
            .map(Duration::from_millis);

        let max_recursion_depth = cfg
            .get("max_recursion_depth")
            .and_then(Value::as_u64)
            .map(|n| n as u32)
            .unwrap_or(DEFAULT_MAX_RECURSION_DEPTH);

        Ok(ParallelConfig {
            join,
            max_concurrency,
            on_branch_failure,
            total_timeout,
            max_recursion_depth,
            branches_spec,
            for_each_path,
        })
    }
}

/// Resolve the branches into a Vec of executor configs. Static literal:
/// just clone. Dynamic for_each: resolve the path against
/// `{$.context, $.workflow.input, $.arguments}` scopes; expect an array;
/// for each element produce one branch by substituting `$.branch.value`
/// and `$.branch.index` into the `do:` template.
fn resolve_branches(
    cfg: &ParallelConfig,
    request: &ExecuteRequest,
) -> Result<Vec<Value>, ExecutorError> {
    match &cfg.branches_spec {
        BranchesSpec::Literal(arr) => Ok(arr.clone()),
        BranchesSpec::ForEach {
            for_each,
            do_template,
            where_clause,
            item_input_schema,
        } => {
            let resolved = read_in_scopes(
                for_each,
                &request.arguments,
                &request.workflow.context,
                &request.workflow.input,
                None,
                Some(&request.workflow.run_env),
            )
            .ok_or_else(|| {
                ExecutorError::Permanent(format!(
                    "INVALID_PARALLEL_CONFIG: `for_each: {for_each}` did not resolve to any value \
                     in scopes ($.context, $.workflow.input, $.arguments)"
                ))
            })?;
            let arr = resolved.as_array().ok_or_else(|| {
                ExecutorError::Permanent(format!(
                    "INVALID_PARALLEL_CONFIG: `for_each: {for_each}` resolved to a non-array \
                     value ({}); for_each requires an array source",
                    short_kind(&resolved)
                ))
            })?;
            // Pre-fan-out filter (SPEC §24.2). Drop elements where the
            // `where:` predicate evaluates falsy. Filter runs BEFORE
            // branches spawn so we never pay the fan-out cost for
            // elements the operator already knows to skip.
            let filtered: Vec<(usize, &Value)> = if let Some(expr) = where_clause {
                arr.iter()
                    .enumerate()
                    .filter(|(index, value)| {
                        let probe = json!({ "value": value, "index": index });
                        match praxec_core::guards::evaluate_join_expression(expr, &probe) {
                            Ok(keep) => keep,
                            // A malformed `where:` predicate would otherwise
                            // silently drop the branch and could produce an
                            // empty fan-out with no signal. Warn so it's
                            // observable (mirrors compute_verdict's Err arm).
                            Err(e) => {
                                tracing::warn!(
                                    where_expression = %expr,
                                    branch_index = *index,
                                    error = %e,
                                    "for_each `where:` predicate errored — dropping element"
                                );
                                false
                            }
                        }
                    })
                    .collect()
            } else {
                arr.iter().enumerate().collect()
            };
            // Spec A §7.1 — MAP BOUNDARY. Each item that survives the filter is
            // about to be handed to a worker; validate it against the worker's
            // declared `<slot>In` (registry-aware, so a `praxec://hop` `$ref`
            // resolves) BEFORE the branch spawns. Fail-fast on the first
            // off-shape item, naming the source-array index. This is the typed
            // fan-out edge: an off-shape item can never reach a worker.
            if let Some(schema) = item_input_schema {
                for (index, value) in &filtered {
                    praxec_core::hop::validate_against_schema(
                        schema,
                        value,
                        "parallel map-boundary item input",
                    )
                    .map_err(|e| {
                        ExecutorError::Permanent(format!(
                            "PARALLEL_MAP_INPUT_VIOLATION: for_each item at index {index} does not \
                             conform to the worker's `do.inputSchema` — {e}"
                        ))
                    })?;
                }
            }
            // NB: branches keep the original element index (not the
            // post-filter position) so audit logs map back to the
            // source-array index unambiguously.
            let branches: Vec<Value> = filtered
                .into_iter()
                .map(|(index, value)| substitute_branch_template(do_template, index, value))
                .collect();
            Ok(branches)
        }
    }
}

/// Recursively walk `template`, substituting `"$.branch.index"` and
/// `"$.branch.value"` with `index` and `value` respectively. Other strings
/// pass through; non-strings recurse into objects/arrays.
fn substitute_branch_template(template: &Value, index: usize, value: &Value) -> Value {
    match template {
        Value::String(s) if s == "$.branch.index" => json!(index),
        Value::String(s) if s == "$.branch.value" => value.clone(),
        Value::Object(obj) => {
            let mut out = Map::new();
            for (k, v) in obj {
                out.insert(k.clone(), substitute_branch_template(v, index, value));
            }
            Value::Object(out)
        }
        Value::Array(arr) => Value::Array(
            arr.iter()
                .map(|v| substitute_branch_template(v, index, value))
                .collect(),
        ),
        other => other.clone(),
    }
}

fn empty_summary_output(cfg: &ParallelConfig) -> Value {
    json!({
        "branches": [],
        "summary": {
            "n":                       0,
            "ok_count":                0,
            "failed_count":            0,
            "cancelled_count":         0,
            "durationMs":              0,
            "first_failure_index":     null,
            "max_in_flight_observed":  0,
            "join":                    cfg.join.as_token(),
            "verdict":                 "succeeded",
        }
    })
}

fn hash_snapshot(def: &Value) -> String {
    // Cheap content hash for the snapshot. NOT collision-resistant for
    // adversarial inputs (we trust our own snapshot bytes); fast for the
    // defensive assert use case.
    let bytes = serde_json::to_vec(def).unwrap_or_default();
    let digest = Sha256::digest(&bytes);
    format!("sha256:{:x}", digest)
}

/// SPEC §24 F7 — segment a branch's idempotency-key directive by branch
/// index so parallel branches don't dedupe against each other downstream.
///
/// - `idempotencyKey: true` → rewritten to template
///   `"{workflowId}.{transition}.{correlationId}:branch:<index>"`
/// - `idempotencyKey: "<template>"` → suffix `:branch:<index>` appended
/// - missing / `false` → no change (no key generated anyway)
///
/// All branches retain the SAME key across THEIR OWN retries (correlation_id
/// is stable across reliability stack attempts), so downstream dedup still
/// works correctly per branch.
fn segment_branch_idempotency_key(mut cfg: Value, index: usize) -> Value {
    let Some(obj) = cfg.as_object_mut() else {
        return cfg;
    };
    let Some(spec) = obj.get("idempotencyKey").cloned() else {
        return cfg;
    };
    let segmented = match spec {
        Value::Bool(true) => Value::String(format!(
            "{{workflowId}}.{{transition}}.{{correlationId}}:branch:{index}"
        )),
        Value::String(template) => Value::String(format!("{template}:branch:{index}")),
        // Bool(false) or anything weird: leave alone.
        other => other,
    };
    obj.insert("idempotencyKey".to_string(), segmented);
    cfg
}

fn short_kind(v: &Value) -> &'static str {
    match v {
        Value::Null => "null",
        Value::Bool(_) => "boolean",
        Value::Number(_) => "number",
        Value::String(_) => "string",
        Value::Array(_) => "array",
        Value::Object(_) => "object",
    }
}
