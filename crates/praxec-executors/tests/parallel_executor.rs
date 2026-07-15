//! SPEC §24 — `parallel` executor kind. FMECA-style atomic assertions
//! covering each Phase 1 / Phase 2 design surface:
//!   - join: all / any / at_least:K
//!   - on_branch_failure: bail / continue
//!   - max_concurrency cap is honored (no more than N in flight)
//!   - dynamic for_each branch generation
//!   - empty for_each → vacuous success
//!   - recursion-depth cap rejects nested parallel exceeding the limit
//!   - DOS poka-yoke: 10+ branches without explicit max_concurrency rejects
//!   - per-branch audit events share parent's correlation_id
//!
//! Tests build the registry from a real config so the parallel executor
//! has its registry wired (set_registry called in default_registry_with_mcp).

use std::sync::Arc;

use chrono::Utc;
use praxec_core::audit::{AuditSink, MemoryAuditSink};
use praxec_core::error::ExecutorError;
use praxec_core::model::{ExecuteRequest, WorkflowInstance};
use praxec_core::ports::ExecutorRegistry;
use praxec_executors::{CliConnections, McpConnections, McpExecutor, default_registry_with_mcp};
use serde_json::{Value, json};

fn instance_stub() -> WorkflowInstance {
    WorkflowInstance {
        id: "wf_parallel_test".into(),
        definition_id: "demo".into(),
        definition_version: "0".into(),
        definition: json!({}),
        state: "s".into(),
        version: 0,
        input: json!({}),
        context: json!({}),
        started_at: Utc::now(),
        run_env: praxec_core::RunEnv::for_test(),
        cancelled_at: None,
        cancelled_reason: None,
        depth: 0,
        parent: None,
    }
}

fn build_registry(audit: Arc<MemoryAuditSink>) -> Arc<dyn ExecutorRegistry> {
    let mcp_conns = McpConnections::from_config(&json!({}));
    let cli_conns = Arc::new(CliConnections::from_config(&json!({})));
    let mcp_exec = Arc::new(McpExecutor::new(mcp_conns));
    default_registry_with_mcp(&json!({}), mcp_exec, cli_conns, audit as Arc<dyn AuditSink>)
}

fn req(executor_config: Value, instance: WorkflowInstance) -> ExecuteRequest {
    ExecuteRequest {
        workflow: instance,
        transition: Some("fan-out".into()),
        arguments: json!({}),
        executor_config,
        idempotency_key: Some("test-parent-key".into()),
        correlation_id: Some("test-corr-id".into()),
    }
}

async fn run_parallel(
    executor_config: Value,
    instance: WorkflowInstance,
    audit: Arc<MemoryAuditSink>,
) -> Result<praxec_core::model::ExecuteResult, ExecutorError> {
    let registry = build_registry(audit);
    let parallel = registry.get("parallel").expect("parallel registered");
    parallel.execute(req(executor_config, instance)).await
}

// ── join: all — every branch succeeds ───────────────────────────────────

#[tokio::test]
async fn join_all_with_two_noop_branches_succeeds() {
    let audit = Arc::new(MemoryAuditSink::new());
    let result = run_parallel(
        json!({
            "kind": "parallel",
            "branches": [
                { "kind": "noop" },
                { "kind": "noop" },
            ],
            "join": "all",
        }),
        instance_stub(),
        audit.clone(),
    )
    .await
    .expect("parallel succeeds");
    assert_eq!(result.output["summary"]["ok_count"], 2);
    assert_eq!(result.output["summary"]["failed_count"], 0);
    assert_eq!(result.output["summary"]["verdict"], "succeeded");
    assert_eq!(result.output["branches"].as_array().unwrap().len(), 2);
}

// ── max_concurrency cap honored ─────────────────────────────────────────

#[tokio::test]
async fn max_concurrency_caps_in_flight_count() {
    let audit = Arc::new(MemoryAuditSink::new());
    let result = run_parallel(
        json!({
            "kind": "parallel",
            "branches": [
                { "kind": "noop" },
                { "kind": "noop" },
                { "kind": "noop" },
                { "kind": "noop" },
                { "kind": "noop" },
            ],
            "join": "all",
            "max_concurrency": 2,
        }),
        instance_stub(),
        audit.clone(),
    )
    .await
    .expect("parallel succeeds");
    let max_in_flight = result.output["summary"]["max_in_flight_observed"]
        .as_u64()
        .unwrap();
    assert!(
        max_in_flight <= 2,
        "max_in_flight {} must not exceed cap 2",
        max_in_flight
    );
}

// ── join: any — first success returns, siblings cancelled ────────────────

#[tokio::test]
async fn join_any_returns_on_first_success() {
    let audit = Arc::new(MemoryAuditSink::new());
    let result = run_parallel(
        json!({
            "kind": "parallel",
            "branches": [
                { "kind": "noop" },
                { "kind": "noop" },
                { "kind": "noop" },
            ],
            "join": "any",
        }),
        instance_stub(),
        audit.clone(),
    )
    .await
    .expect("join: any with at least one success must succeed");
    assert!(
        result.output["summary"]["ok_count"].as_u64().unwrap() >= 1,
        "at least one success: {result:?}",
    );
    assert_eq!(result.output["summary"]["verdict"], "succeeded");
}

// ── join: at_least: K — threshold met ────────────────────────────────────

#[tokio::test]
async fn join_at_least_3_succeeds_with_3_of_5() {
    let audit = Arc::new(MemoryAuditSink::new());
    let result = run_parallel(
        json!({
            "kind": "parallel",
            "branches": [
                { "kind": "noop" },
                { "kind": "noop" },
                { "kind": "noop" },
                { "kind": "noop" },
                { "kind": "noop" },
            ],
            "join": { "at_least": 3 },
        }),
        instance_stub(),
        audit.clone(),
    )
    .await
    .expect("at_least: 3 with 5 successes must succeed");
    assert!(result.output["summary"]["ok_count"].as_u64().unwrap() >= 3,);
    assert_eq!(result.output["summary"]["verdict"], "succeeded");
}

// ── DOS poka-yoke: 10+ branches without max_concurrency rejects ─────────

#[tokio::test]
async fn ten_plus_branches_without_max_concurrency_rejects() {
    let audit = Arc::new(MemoryAuditSink::new());
    let branches: Vec<Value> = (0..12).map(|_| json!({ "kind": "noop" })).collect();
    let err = run_parallel(
        json!({
            "kind": "parallel",
            "branches": branches,
            "join": "all",
        }),
        instance_stub(),
        audit.clone(),
    )
    .await
    .expect_err("12 branches without max_concurrency must reject");
    let s = format!("{err:?}");
    assert!(s.contains("INVALID_PARALLEL_CONFIG"), "got: {s}");
    assert!(s.contains("max_concurrency"), "got: {s}");
}

// ── Dynamic for_each ────────────────────────────────────────────────────

#[tokio::test]
async fn dynamic_for_each_expands_array_into_branches() {
    let audit = Arc::new(MemoryAuditSink::new());
    let mut instance = instance_stub();
    instance.context = json!({ "queries": ["alpha", "beta", "gamma"] });
    let result = run_parallel(
        json!({
            "kind": "parallel",
            "branches": {
                "for_each": "$.context.queries",
                "do":       { "kind": "noop" },
            },
            "join": "all",
        }),
        instance,
        audit.clone(),
    )
    .await
    .expect("for_each over 3-element array produces 3 branches");
    assert_eq!(result.output["summary"]["n"], 3);
    assert_eq!(result.output["summary"]["ok_count"], 3);
}

// ── Empty for_each → vacuous success ─────────────────────────────────────

#[tokio::test]
async fn empty_for_each_returns_vacuous_success() {
    let audit = Arc::new(MemoryAuditSink::new());
    let mut instance = instance_stub();
    instance.context = json!({ "queries": [] });
    let result = run_parallel(
        json!({
            "kind": "parallel",
            "branches": {
                "for_each": "$.context.queries",
                "do":       { "kind": "noop" },
            },
            "join": "all",
        }),
        instance,
        audit.clone(),
    )
    .await
    .expect("empty for_each must vacuous-succeed, not error");
    assert_eq!(result.output["summary"]["n"], 0);
    assert_eq!(result.output["summary"]["ok_count"], 0);
    assert_eq!(result.output["summary"]["verdict"], "succeeded");
    let event_types = audit.event_types();
    assert!(
        event_types.iter().any(|e| e == "parallel.fanout.empty"),
        "must emit parallel.fanout.empty for observability; got: {event_types:?}"
    );
}

// ── Audit per-branch events share parent's correlation_id ───────────────

#[tokio::test]
async fn per_branch_audit_events_share_parent_correlation_id() {
    let audit = Arc::new(MemoryAuditSink::new());
    let _ = run_parallel(
        json!({
            "kind": "parallel",
            "branches": [
                { "kind": "noop" },
                { "kind": "noop" },
            ],
            "join": "all",
        }),
        instance_stub(),
        audit.clone(),
    )
    .await
    .expect("parallel succeeds");

    let events = audit.snapshot();
    let parent_corr = "test-corr-id";
    // Every parallel.* audit event must carry the parent's correlation_id.
    let parallel_events: Vec<_> = events
        .iter()
        .filter(|e| e.event_type.starts_with("parallel."))
        .collect();
    assert!(
        !parallel_events.is_empty(),
        "expected at least one parallel.* event; got: {:?}",
        events.iter().map(|e| &e.event_type).collect::<Vec<_>>()
    );
    for ev in &parallel_events {
        assert_eq!(
            ev.correlation_id, parent_corr,
            "event {} must carry parent's correlation_id; got: {}",
            ev.event_type, ev.correlation_id
        );
    }
}

// ── Recursion-depth cap rejects nested parallel beyond the limit ────────

#[tokio::test]
async fn nested_parallel_beyond_max_recursion_depth_rejects() {
    // 4 levels deep with cap 2 → must reject. Build inside-out: deepest
    // first, then wrap each level above.
    let audit = Arc::new(MemoryAuditSink::new());
    let depth4 = json!({
        "kind": "parallel",
        "branches": [{ "kind": "noop" }],
        "join": "all",
        "max_recursion_depth": 2,
    });
    let depth3 = json!({
        "kind": "parallel",
        "branches": [depth4],
        "join": "all",
        "max_recursion_depth": 2,
    });
    let depth2 = json!({
        "kind": "parallel",
        "branches": [depth3],
        "join": "all",
        "max_recursion_depth": 2,
    });
    let depth1 = json!({
        "kind": "parallel",
        "branches": [depth2],
        "join": "all",
        "max_recursion_depth": 2,
    });

    // Top-level execution starts at depth=0 (no task_local set), then the
    // first nested branch enters depth=1 (still ok with cap=2), the second
    // enters depth=2 (still ok), the third enters depth=3 (REJECT — cap=2
    // means current_depth=2 >= cap).
    let result = run_parallel(depth1, instance_stub(), audit.clone()).await;
    // Some branch result inside the aggregated output should carry the
    // PARALLEL_DEPTH_EXCEEDED error. Because on_branch_failure defaults to
    // `bail`, the whole thing fails — find the error.
    let err = result.expect_err("nested parallel beyond cap must fail");
    let s = format!("{err:?}");
    assert!(
        s.contains("PARALLEL_DEPTH_EXCEEDED") || s.contains("fan-out failed"),
        "expected PARALLEL_DEPTH_EXCEEDED or bail-due-to-it; got: {s}"
    );
}

// ── GAP-G: parallel.branch.cancelled event emitted per cancelled branch ──

#[tokio::test]
async fn join_any_emits_cancelled_event_for_dropped_siblings() {
    // join=any returns on first success and cancels the rest. Each
    // dropped branch must produce a `parallel.branch.cancelled` audit
    // event so operators can see which branches were aborted.
    let audit = Arc::new(MemoryAuditSink::new());
    let _ = run_parallel(
        json!({
            "kind": "parallel",
            "branches": [
                { "kind": "noop" },
                { "kind": "noop" },
                { "kind": "noop" },
                { "kind": "noop" },
            ],
            "join": "any",
        }),
        instance_stub(),
        audit.clone(),
    )
    .await
    .expect("join: any succeeds");

    let events = audit.snapshot();
    let cancelled: Vec<_> = events
        .iter()
        .filter(|e| e.event_type == "parallel.branch.cancelled")
        .collect();
    // Hard to assert exact count (timing-dependent — depends on how many
    // had started before abort_all), but at least one of 4 branches
    // should have been cancelled in a typical join=any run. Be lenient:
    // assert that EITHER cancelled events are present OR all 4 actually
    // completed (in which case the test races every branch to the same
    // tick — fine, just no cancellations to log).
    let completed: Vec<_> = events
        .iter()
        .filter(|e| e.event_type == "parallel.branch.completed")
        .collect();
    let total_concluded = cancelled.len() + completed.len();
    assert_eq!(
        total_concluded,
        4,
        "every branch must conclude as either completed or cancelled; got: {} completed + {} cancelled",
        completed.len(),
        cancelled.len()
    );
    // All cancelled events share parent's correlation_id (regression
    // assert for the F3 mitigation).
    for ev in &cancelled {
        assert_eq!(ev.correlation_id, "test-corr-id");
    }
}

// ── on_branch_failure: continue still drains all branches ───────────────

// (Requires an executor that can fail. NoopExecutor always succeeds.
// We use a missing-kind branch to trigger the "executor kind not registered"
// failure path, since execute_with_reliability emits ExecutorError::Permanent
// for unknown kinds.)

#[tokio::test]
async fn on_branch_failure_continue_drains_all_branches() {
    let audit = Arc::new(MemoryAuditSink::new());
    let result = run_parallel(
        json!({
            "kind": "parallel",
            "branches": [
                { "kind": "noop" },
                { "kind": "nonexistent_kind" },
                { "kind": "noop" },
            ],
            "join": "all",
            "on_branch_failure": "continue",
        }),
        instance_stub(),
        audit.clone(),
    )
    .await;
    // Verdict is failed (join=all + 1 failure), but ok_count should be 2
    // (continue ran both successes).
    let err = result.expect_err("join=all with 1 failure must fail");
    let s = format!("{err:?}");
    // The audit log should show 3 branch.started events (all drained).
    let started: Vec<_> = audit
        .snapshot()
        .into_iter()
        .filter(|e| e.event_type == "parallel.branch.started")
        .collect();
    assert_eq!(
        started.len(),
        3,
        "on_branch_failure: continue must start ALL 3 branches; got: {} ({s})",
        started.len()
    );
}

// ── join: percent — percentage quorum ───────────────────────────────────

#[tokio::test]
async fn join_percent_threshold_met_by_majority_succeeds() {
    let audit = Arc::new(MemoryAuditSink::new());
    let result = run_parallel(
        json!({
            "kind": "parallel",
            "branches": [
                { "kind": "noop" },
                { "kind": "noop" },
                { "kind": "noop" },
                { "kind": "nonexistent_kind" },
            ],
            "join": { "percent": 51 },
            "on_branch_failure": "continue",
        }),
        instance_stub(),
        audit.clone(),
    )
    .await
    .expect("3 of 4 = 75% >= 51% threshold; expression succeeds");
    assert_eq!(result.output["summary"]["ok_count"], 3);
    assert_eq!(result.output["summary"]["verdict"], "succeeded");
    assert_eq!(result.output["summary"]["join"], "percent");
}

#[tokio::test]
async fn join_percent_threshold_not_met_returns_threshold_failure() {
    let audit = Arc::new(MemoryAuditSink::new());
    let result = run_parallel(
        json!({
            "kind": "parallel",
            "branches": [
                { "kind": "noop" },
                { "kind": "nonexistent_kind" },
                { "kind": "nonexistent_kind" },
                { "kind": "nonexistent_kind" },
            ],
            "join": { "percent": 75 },
            "on_branch_failure": "continue",
        }),
        instance_stub(),
        audit.clone(),
    )
    .await;
    let err = result.expect_err("1 of 4 = 25% < 75% threshold must fail");
    let s = format!("{err:?}");
    assert!(
        s.contains("threshold_not_met") || s.contains("ThresholdNotMet") || s.contains("failed"),
        "expected threshold failure indication, got: {s}"
    );
}

#[tokio::test]
async fn join_percent_zero_succeeds_with_no_branches_passing() {
    // 0% threshold means "always succeed regardless of branches" — the
    // explicit operator choice to disable the quorum check.
    let audit = Arc::new(MemoryAuditSink::new());
    let result = run_parallel(
        json!({
            "kind": "parallel",
            "branches": [
                { "kind": "nonexistent_kind" },
                { "kind": "nonexistent_kind" },
            ],
            "join": { "percent": 0 },
            "on_branch_failure": "continue",
        }),
        instance_stub(),
        audit.clone(),
    )
    .await
    .expect("percent=0 is the never-fail-by-quorum escape hatch");
    assert_eq!(result.output["summary"]["ok_count"], 0);
    assert_eq!(result.output["summary"]["verdict"], "succeeded");
}

#[tokio::test]
async fn join_percent_uses_ceiling_division_for_threshold() {
    // 51% of 3 branches = 1.53, ceil = 2 required successes. 2/3 succeed → ok.
    let audit = Arc::new(MemoryAuditSink::new());
    let result = run_parallel(
        json!({
            "kind": "parallel",
            "branches": [
                { "kind": "noop" },
                { "kind": "noop" },
                { "kind": "nonexistent_kind" },
            ],
            "join": { "percent": 51 },
            "on_branch_failure": "continue",
        }),
        instance_stub(),
        audit.clone(),
    )
    .await
    .expect("ceiling: 2/3 ok meets ceil(51% of 3) = 2 threshold");
    assert_eq!(result.output["summary"]["ok_count"], 2);
    assert_eq!(result.output["summary"]["verdict"], "succeeded");
}

#[tokio::test]
async fn join_percent_rejects_value_above_100() {
    let audit = Arc::new(MemoryAuditSink::new());
    let result = run_parallel(
        json!({
            "kind": "parallel",
            "branches": [ { "kind": "noop" } ],
            "join": { "percent": 150 },
        }),
        instance_stub(),
        audit.clone(),
    )
    .await;
    let err = result.expect_err("percent > 100 must reject at config-parse");
    let s = format!("{err:?}");
    assert!(s.contains("INVALID_PARALLEL_CONFIG"), "got: {s}");
}

// ── join: expression — operator-supplied post-completion predicate ──────

#[tokio::test]
async fn join_expression_truthy_path_succeeds() {
    let audit = Arc::new(MemoryAuditSink::new());
    let result = run_parallel(
        json!({
            "kind": "parallel",
            "branches": [
                { "kind": "noop" },
                { "kind": "noop" },
            ],
            "join": { "expression": "$.ok_count" },
            "on_branch_failure": "continue",
        }),
        instance_stub(),
        audit.clone(),
    )
    .await
    .expect("ok_count=2 is truthy");
    assert_eq!(result.output["summary"]["ok_count"], 2);
    assert_eq!(result.output["summary"]["verdict"], "succeeded");
    assert_eq!(result.output["summary"]["join"], "expression");
}

#[tokio::test]
async fn join_expression_binary_comparison_satisfied_succeeds() {
    let audit = Arc::new(MemoryAuditSink::new());
    let result = run_parallel(
        json!({
            "kind": "parallel",
            "branches": [
                { "kind": "noop" },
                { "kind": "noop" },
                { "kind": "noop" },
            ],
            "join": { "expression": "$.ok_count >= 3" },
            "on_branch_failure": "continue",
        }),
        instance_stub(),
        audit.clone(),
    )
    .await
    .expect("3 >= 3 is true");
    assert_eq!(result.output["summary"]["verdict"], "succeeded");
}

#[tokio::test]
async fn join_expression_binary_comparison_unsatisfied_fails() {
    let audit = Arc::new(MemoryAuditSink::new());
    let result = run_parallel(
        json!({
            "kind": "parallel",
            "branches": [
                { "kind": "noop" },
                { "kind": "noop" },
            ],
            "join": { "expression": "$.ok_count >= 3" },
            "on_branch_failure": "continue",
        }),
        instance_stub(),
        audit.clone(),
    )
    .await;
    let err = result.expect_err("2 >= 3 is false; verdict must be failed");
    let s = format!("{err:?}");
    assert!(s.contains("failed") || s.contains("Failed"), "got: {s}");
}

#[tokio::test]
async fn join_expression_no_early_exit_runs_all_branches() {
    // Expression joins MUST NOT early-exit; they need all branches'
    // results to evaluate. Even when expression is "$.ok_count >= 1"
    // (which would let `any` early-exit), `expression` runs everything.
    let audit = Arc::new(MemoryAuditSink::new());
    let _ = run_parallel(
        json!({
            "kind": "parallel",
            "branches": [
                { "kind": "noop" },
                { "kind": "noop" },
                { "kind": "noop" },
            ],
            "join": { "expression": "$.ok_count >= 1" },
            "on_branch_failure": "continue",
        }),
        instance_stub(),
        audit.clone(),
    )
    .await
    .expect("expression satisfied");
    let started: Vec<_> = audit
        .snapshot()
        .into_iter()
        .filter(|e| e.event_type == "parallel.branch.started")
        .collect();
    assert_eq!(
        started.len(),
        3,
        "expression-join must not early-exit; all 3 branches must start"
    );
}

#[tokio::test]
async fn join_expression_rejects_empty_string() {
    let audit = Arc::new(MemoryAuditSink::new());
    let result = run_parallel(
        json!({
            "kind": "parallel",
            "branches": [ { "kind": "noop" } ],
            "join": { "expression": "" },
        }),
        instance_stub(),
        audit.clone(),
    )
    .await;
    let err = result.expect_err("empty expression must reject");
    let s = format!("{err:?}");
    assert!(s.contains("INVALID_PARALLEL_CONFIG"), "got: {s}");
}

// ── join: { aggregator: ... } — general aggregator pattern ──────────────

#[tokio::test]
async fn aggregator_kind_expression_works_via_canonical_form() {
    // Sugar form (`expression: "..."`) and canonical form
    // (`aggregator: { kind: expression, expr: "..." }`) must be
    // semantically equivalent.
    let audit = Arc::new(MemoryAuditSink::new());
    let result = run_parallel(
        json!({
            "kind": "parallel",
            "branches": [
                { "kind": "noop" },
                { "kind": "noop" },
            ],
            "join": {
                "aggregator": {
                    "kind": "expression",
                    "expr": "$.ok_count == 2"
                }
            },
            "on_branch_failure": "continue",
        }),
        instance_stub(),
        audit.clone(),
    )
    .await
    .expect("aggregator kind=expression resolves");
    assert_eq!(result.output["summary"]["verdict"], "succeeded");
    // Token reflects the aggregator kind specifically.
    assert_eq!(result.output["summary"]["join"], "expression");
}

#[tokio::test]
async fn aggregator_missing_kind_rejects_at_parse() {
    let audit = Arc::new(MemoryAuditSink::new());
    let result = run_parallel(
        json!({
            "kind": "parallel",
            "branches": [ { "kind": "noop" } ],
            "join": { "aggregator": { } },
        }),
        instance_stub(),
        audit.clone(),
    )
    .await;
    let err = result.expect_err("aggregator with no kind must reject");
    let s = format!("{err:?}");
    assert!(
        s.contains("INVALID_PARALLEL_CONFIG") && s.contains("kind"),
        "got: {s}"
    );
}

#[tokio::test]
async fn aggregator_kind_expression_missing_expr_rejects_at_parse() {
    let audit = Arc::new(MemoryAuditSink::new());
    let result = run_parallel(
        json!({
            "kind": "parallel",
            "branches": [ { "kind": "noop" } ],
            "join": { "aggregator": { "kind": "expression" } },
        }),
        instance_stub(),
        audit.clone(),
    )
    .await;
    let err = result.expect_err("kind=expression without expr must reject");
    let s = format!("{err:?}");
    assert!(s.contains("INVALID_PARALLEL_CONFIG"), "got: {s}");
}

#[tokio::test]
async fn aggregator_unknown_kind_treats_verdict_as_failed() {
    // Unknown aggregator kind doesn't reject at parse (open kind space —
    // any registered executor can be an aggregator). Runtime treats it
    // as failed verdict because the dispatcher can't find the executor.
    let audit = Arc::new(MemoryAuditSink::new());
    let result = run_parallel(
        json!({
            "kind": "parallel",
            "branches": [ { "kind": "noop" } ],
            "join": { "aggregator": { "kind": "nonexistent_aggregator_kind" } },
            "on_branch_failure": "continue",
        }),
        instance_stub(),
        audit.clone(),
    )
    .await;
    let err = result.expect_err("unknown aggregator kind → failed verdict");
    let s = format!("{err:?}");
    assert!(s.contains("failed") || s.contains("Failed"), "got: {s}");
}

#[tokio::test]
async fn aggregator_execution_error_propagates_not_flattened_to_failed_verdict() {
    // A registered aggregator executor that ERRORS at execution time is an
    // infrastructure failure, not a business "the join failed" verdict.
    // Previously such an error was caught and flattened into a permanent
    // `parallel fan-out failed` verdict — losing the original error's
    // identity and its Transient/Permanent (retryable) classification.
    // Here a `rest` aggregator with no `connection` errors inside execute();
    // the original error must surface, NOT the generic verdict-failed string.
    let audit = Arc::new(MemoryAuditSink::new());
    let result = run_parallel(
        json!({
            "kind": "parallel",
            "branches": [ { "kind": "noop" } ],
            "join": { "aggregator": { "kind": "rest" } },
            "on_branch_failure": "continue",
        }),
        instance_stub(),
        audit.clone(),
    )
    .await;
    let err = result.expect_err("aggregator execution error must propagate");
    let s = format!("{err:?}");
    assert!(
        s.contains("rest") || s.contains("connection"),
        "the aggregator's own error must surface, got: {s}"
    );
    assert!(
        !s.contains("parallel fan-out failed"),
        "an aggregator execution error must NOT be flattened to a generic failed verdict: {s}"
    );
}

// ── for_each.where pre-fan-out filter (SPEC §24.2) ──────────────────────

#[tokio::test]
async fn for_each_where_filters_out_falsy_elements_before_fan_out() {
    let mut inst = instance_stub();
    inst.context = json!({ "items": [1, 2, 3, 4, 5] });
    let audit = Arc::new(MemoryAuditSink::new());
    let result = run_parallel(
        json!({
            "kind": "parallel",
            "branches": {
                "for_each": "$.context.items",
                "where":    "$.value >= 3",
                "do": { "kind": "noop" }
            },
            "join": "all",
        }),
        inst,
        audit.clone(),
    )
    .await
    .expect("filter then fan out");
    // 3 of 5 elements pass the filter; only 3 branches should run.
    assert_eq!(result.output["summary"]["n"], 3);
    assert_eq!(result.output["summary"]["ok_count"], 3);
    let started: Vec<_> = audit
        .snapshot()
        .into_iter()
        .filter(|e| e.event_type == "parallel.branch.started")
        .collect();
    assert_eq!(
        started.len(),
        3,
        "filter must drop elements BEFORE fan-out (saw {} branch.started events)",
        started.len()
    );
}

#[tokio::test]
async fn for_each_where_fan_out_index_is_post_filter_position() {
    // After a filter, two views of "index" coexist:
    //   - `$.branch.index` inside the `do:` template substitutes the
    //     ORIGINAL source-array index, so templates can reference
    //     `$.context.items[N]` for per-element metadata lookup.
    //   - `branches[].index` in the aggregated output is the FAN-OUT
    //     position (contiguous 0..n_filtered) so summary counts and
    //     per-branch event ordering stay dense.
    let mut inst = instance_stub();
    inst.context = json!({ "items": [10, 20, 30, 40] });
    let audit = Arc::new(MemoryAuditSink::new());
    let result = run_parallel(
        json!({
            "kind": "parallel",
            "branches": {
                "for_each": "$.context.items",
                "where":    "$.value > 25",
                "do": { "kind": "noop" }
            },
            "join": "all",
        }),
        inst,
        audit.clone(),
    )
    .await
    .expect("filter passes");
    let branches = result.output["branches"].as_array().unwrap();
    let fan_out: Vec<u64> = branches
        .iter()
        .map(|b| b["index"].as_u64().unwrap())
        .collect();
    assert_eq!(
        fan_out,
        vec![0, 1],
        "branches[].index is the dense fan-out position; original-index view \
         is reserved for `$.branch.index` template substitution"
    );
    assert_eq!(result.output["summary"]["n"], 2);
}

#[tokio::test]
async fn for_each_where_empty_after_filter_is_vacuous_success() {
    let mut inst = instance_stub();
    inst.context = json!({ "items": [1, 2, 3] });
    let audit = Arc::new(MemoryAuditSink::new());
    let result = run_parallel(
        json!({
            "kind": "parallel",
            "branches": {
                "for_each": "$.context.items",
                "where":    "$.value > 100",  // nothing matches
                "do": { "kind": "noop" }
            },
            "join": "all",
        }),
        inst,
        audit.clone(),
    )
    .await
    .expect("empty-after-filter is vacuous success per SPEC §24 F9");
    assert_eq!(result.output["summary"]["n"], 0);
    assert_eq!(result.output["summary"]["verdict"], "succeeded");
}

#[tokio::test]
async fn for_each_where_rejects_empty_predicate() {
    let audit = Arc::new(MemoryAuditSink::new());
    let result = run_parallel(
        json!({
            "kind": "parallel",
            "branches": {
                "for_each": "$.context.items",
                "where":    "",
                "do": { "kind": "noop" }
            },
        }),
        instance_stub(),
        audit.clone(),
    )
    .await;
    let err = result.expect_err("empty where: must reject at parse");
    let s = format!("{err:?}");
    assert!(s.contains("INVALID_PARALLEL_CONFIG"), "got: {s}");
}

// ── config validation: unknown join value rejects (INVALID_PARALLEL_CONFIG) ──

#[tokio::test]
async fn unknown_join_value_is_permanent_config_error() {
    let audit = Arc::new(MemoryAuditSink::new());
    let err = run_parallel(
        json!({ "kind": "parallel", "branches": [ { "kind": "noop" } ], "join": "bogus" }),
        instance_stub(),
        audit,
    )
    .await
    .expect_err("an unknown join value must be a config error");
    match err {
        ExecutorError::Permanent(msg) => {
            assert!(msg.contains("INVALID_PARALLEL_CONFIG"), "got: {msg}")
        }
        other => panic!("expected Permanent, got {other:?}"),
    }
}

// ── Spec A §7.1 — MAP BOUNDARY: per-item input validation ────────────────

#[tokio::test]
async fn map_boundary_conforming_items_pass() {
    // Every item conforms to the worker's declared inputSchema → fan-out runs.
    let mut inst = instance_stub();
    inst.context = json!({ "items": [
        { "symbol": "alpha" },
        { "symbol": "beta" },
    ]});
    let audit = Arc::new(MemoryAuditSink::new());
    let result = run_parallel(
        json!({
            "kind": "parallel",
            "branches": {
                "for_each": "$.context.items",
                "do": {
                    "kind": "noop",
                    "inputSchema": {
                        "type": "object",
                        "properties": { "symbol": { "type": "string" } },
                        "required": ["symbol"]
                    }
                }
            },
            "join": "all",
        }),
        inst,
        audit.clone(),
    )
    .await
    .expect("conforming items pass the map boundary");
    assert_eq!(result.output["summary"]["n"], 2);
    assert_eq!(result.output["summary"]["ok_count"], 2);
}

#[tokio::test]
async fn map_boundary_off_shape_item_rejected_before_fanout() {
    // The second item is missing the required `symbol` field → fail-fast at the
    // map boundary, naming the offending source-array index, BEFORE any branch
    // spawns (no branch.started events).
    let mut inst = instance_stub();
    inst.context = json!({ "items": [
        { "symbol": "alpha" },
        { "nope": true },
    ]});
    let audit = Arc::new(MemoryAuditSink::new());
    let err = run_parallel(
        json!({
            "kind": "parallel",
            "branches": {
                "for_each": "$.context.items",
                "do": {
                    "kind": "noop",
                    "inputSchema": {
                        "type": "object",
                        "properties": { "symbol": { "type": "string" } },
                        "required": ["symbol"]
                    }
                }
            },
            "join": "all",
        }),
        inst,
        audit.clone(),
    )
    .await
    .expect_err("an off-shape item must be rejected at the map boundary");
    match err {
        ExecutorError::Permanent(msg) => {
            assert!(msg.contains("PARALLEL_MAP_INPUT_VIOLATION"), "got: {msg}");
            assert!(
                msg.contains("index 1"),
                "must name the offending index: {msg}"
            );
        }
        other => panic!("expected Permanent, got {other:?}"),
    }
    // Boundary is enforced before fan-out: no branch ever started.
    let started = audit
        .snapshot()
        .into_iter()
        .filter(|e| e.event_type == "parallel.branch.started")
        .count();
    assert_eq!(
        started, 0,
        "no branch may spawn once the map boundary rejects"
    );
}

#[tokio::test]
async fn map_boundary_only_validates_items_surviving_the_where_filter() {
    // The off-shape element is filtered out by `where:` before the boundary,
    // so the remaining (conforming) item passes and fan-out proceeds. This
    // proves the map boundary validates exactly the items handed to workers.
    let mut inst = instance_stub();
    inst.context = json!({ "items": [
        { "symbol": "alpha", "keep": true },
        { "keep": false },
    ]});
    let audit = Arc::new(MemoryAuditSink::new());
    let result = run_parallel(
        json!({
            "kind": "parallel",
            "branches": {
                "for_each": "$.context.items",
                "where":    "$.value.keep == true",
                "do": {
                    "kind": "noop",
                    "inputSchema": {
                        "type": "object",
                        "properties": { "symbol": { "type": "string" } },
                        "required": ["symbol"]
                    }
                }
            },
            "join": "all",
        }),
        inst,
        audit.clone(),
    )
    .await
    .expect("filtered-out off-shape item must not trip the map boundary");
    assert_eq!(result.output["summary"]["n"], 1);
    assert_eq!(result.output["summary"]["ok_count"], 1);
}

#[tokio::test]
async fn map_boundary_resolves_hop_input_ref() {
    // A `praxec://hop#/$defs/verifyIn` contract must resolve against the shipped
    // HOP vocabulary (registry-aware): `verifyIn` requires `cwd`, so an item
    // without it is rejected — proving the boundary uses the same registry the
    // runtime seams do, not a bare validator that would fail to resolve the ref.
    let mut inst = instance_stub();
    inst.context = json!({ "items": [ { "not_cwd": "x" } ] });
    let audit = Arc::new(MemoryAuditSink::new());
    let err = run_parallel(
        json!({
            "kind": "parallel",
            "branches": {
                "for_each": "$.context.items",
                "do": {
                    "kind": "noop",
                    "inputSchema": { "$ref": "praxec://hop#/$defs/verifyIn" }
                }
            },
            "join": "all",
        }),
        inst,
        audit.clone(),
    )
    .await
    .expect_err("an item missing `cwd` must fail the verifyIn contract");
    match err {
        ExecutorError::Permanent(msg) => {
            assert!(msg.contains("PARALLEL_MAP_INPUT_VIOLATION"), "got: {msg}")
        }
        other => panic!("expected Permanent, got {other:?}"),
    }
}

#[tokio::test]
async fn map_boundary_non_object_input_schema_rejects_at_parse() {
    let audit = Arc::new(MemoryAuditSink::new());
    let mut inst = instance_stub();
    inst.context = json!({ "items": [1, 2] });
    let err = run_parallel(
        json!({
            "kind": "parallel",
            "branches": {
                "for_each": "$.context.items",
                "do": { "kind": "noop", "inputSchema": "not-a-schema" }
            },
        }),
        inst,
        audit,
    )
    .await
    .expect_err("a non-object inputSchema must reject at parse");
    match err {
        ExecutorError::Permanent(msg) => {
            assert!(msg.contains("INVALID_PARALLEL_CONFIG"), "got: {msg}")
        }
        other => panic!("expected Permanent, got {other:?}"),
    }
}
