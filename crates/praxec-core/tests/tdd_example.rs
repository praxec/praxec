//! End-to-end tests for the hardened TDD example workflow.
//!
//! Loads `examples/tdd/gateway.yaml` and drives it with a stub runner
//! that returns `{passed, count, output}` — the same shape as
//! `examples/tdd/tdd-runner.sh` produces. The test queue lets us script
//! exactly the behaviors a real TDD session (or an attempted cheat)
//! would produce, then asserts the workflow either advances correctly
//! or routes to the `cheated` terminal state.
//!
//! Each scenario corresponds to a behavior the `gateway.yaml` README
//! claims is enforced. If the workflow's wording stops matching the
//! runtime's behavior, this file fails loud.

use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use praxec_core::audit::{AuditSink, MemoryAuditSink};
use praxec_core::config::load_resolved;
use praxec_core::error::ExecutorError;
use praxec_core::guards::DefaultGuardEvaluator;
use praxec_core::model::{
    ExecuteRequest, ExecuteResult, Principal, StartWorkflow, SubmitTransition,
};
use praxec_core::ports::{Executor, ExecutorRegistry};
use praxec_core::store::{ConfigDefinitionStore, InMemoryWorkflowStore};
use praxec_core::WorkflowRuntime;
use serde_json::{json, Value};

/// One result from a stubbed test runner: did the suite pass, and what
/// was the test count at that moment.
#[derive(Clone, Copy)]
struct RunnerResult {
    passed: bool,
    count: u64,
}

/// Stub runner: dequeues a `RunnerResult` per call and returns it as
/// `{passed, count, output}`, mirroring the shape `tdd-runner.sh`
/// produces. Lets each test script the exact runner behavior it needs.
struct ScriptedRunner {
    queue: Mutex<Vec<RunnerResult>>,
}
impl ScriptedRunner {
    fn new(queue: &[RunnerResult]) -> Self {
        Self {
            queue: Mutex::new(queue.iter().rev().copied().collect()),
        }
    }
}
#[async_trait]
impl Executor for ScriptedRunner {
    async fn execute(&self, _r: ExecuteRequest) -> Result<ExecuteResult, ExecutorError> {
        let next = self
            .queue
            .lock()
            .unwrap()
            .pop()
            .expect("scripted runner ran out of canned results");
        Ok(ExecuteResult {
            output: json!({
                "exitCode": if next.passed { 0 } else { 1 },
                "success":  next.passed,
                "stdout":   "(scripted)",
                "stderr":   "",
                // The hardened workflow reads from $.output.json.* —
                // imitate the wrapper script's output shape directly.
                "json":     { "passed": next.passed, "count": next.count, "output": "(scripted)" },
            }),
            evidence: vec![],
            child_workflow_id: None,
            next_transition: None,
            suspend: None,
            telemetry: None,
        })
    }
}

/// Registry that hands the scripted runner to `cli` (the only kind the
/// TDD workflow's checkpoint transitions use) and a real noop to
/// everything else (so the green.onEnter `noop` doesn't accidentally
/// consume a scripted runner result).
struct CliScripted {
    cli: Arc<dyn Executor>,
}
struct InertNoop;
#[async_trait]
impl Executor for InertNoop {
    async fn execute(&self, _r: ExecuteRequest) -> Result<ExecuteResult, ExecutorError> {
        Ok(ExecuteResult {
            output: json!({}),
            evidence: vec![],
            child_workflow_id: None,
            next_transition: None,
            suspend: None,
            telemetry: None,
        })
    }
}
impl ExecutorRegistry for CliScripted {
    fn get(&self, kind: &str) -> Option<Arc<dyn Executor>> {
        match kind {
            "cli" => Some(self.cli.clone()),
            _ => Some(Arc::new(InertNoop)),
        }
    }
}

fn build(test_results: &[RunnerResult]) -> (WorkflowRuntime, Arc<MemoryAuditSink>) {
    let config = load_resolved("../../examples/tdd/gateway.yaml")
        .expect("examples/tdd/gateway.yaml should load");
    let definitions = Arc::new(ConfigDefinitionStore::from_config(&config));
    let store = Arc::new(InMemoryWorkflowStore::new());
    let executors = Arc::new(CliScripted {
        cli: Arc::new(ScriptedRunner::new(test_results)),
    });
    let guards = Arc::new(DefaultGuardEvaluator::new());
    let audit = Arc::new(MemoryAuditSink::new());
    let runtime = WorkflowRuntime::new(
        definitions,
        store,
        executors,
        guards,
        audit.clone() as Arc<dyn AuditSink>,
    );
    (runtime, audit)
}

async fn start(runtime: &WorkflowRuntime) -> (String, u64, Value) {
    let resp = runtime
        .start(StartWorkflow {
            definition_id: "tdd".into(),
            input: json!({
                "test_cmd":  "echo passed",
                "count_cmd": "echo 0",
                "runner_path": "/dev/null",
            }),
            principal: Principal::anonymous(),
            trace_id: None,
            run_id: None,
            depth: 0,
            parent: None,
        })
        .await
        .unwrap();
    let id = resp["workflow"]["id"].as_str().unwrap().to_string();
    let v = resp["workflow"]["version"].as_u64().unwrap();
    (id, v, resp)
}

async fn submit(runtime: &WorkflowRuntime, id: &str, version: u64, transition: &str) -> Value {
    runtime
        .submit(SubmitTransition {
            workflow_id: id.to_string(),
            expected_version: version,
            transition: transition.to_string(),
            arguments: json!({}),
            principal: Principal::anonymous(),
            summary: None,
            trace_id: None,
            run_id: None,
        })
        .await
        .unwrap()
}

fn state(resp: &Value) -> &str {
    resp["workflow"]["state"].as_str().unwrap()
}

fn version(resp: &Value) -> u64 {
    resp["workflow"]["version"].as_u64().unwrap()
}

// Convenience for `RunnerResult` in tests.
fn r(passed: bool, count: u64) -> RunnerResult {
    RunnerResult { passed, count }
}

// ---------- happy path ---------------------------------------------------

#[tokio::test]
async fn happy_path_one_cycle_to_done() {
    // Sequence:
    //   start_cycle    → baseline=3, session_baseline=3
    //   confirm_red    → agent wrote failing test → count=4, fails  → red
    //   confirm_green  → impl passes → count=4, passes              → green
    //   finish (cycles_completed >= 1)                              → done
    let (runtime, _) = build(&[
        r(true, 3),  // baseline + session_baseline
        r(false, 4), // red confirm: count strictly increased AND tests fail
        r(true, 4),  // green confirm: tests pass, count > baseline (3) so green
    ]);
    let (id, v0, _started) = start(&runtime).await;

    let r1 = submit(&runtime, &id, v0, "start_cycle").await;
    assert_eq!(state(&r1), "red_pending");
    assert_eq!(r1["context"]["baseline_count"], 3);
    assert_eq!(r1["context"]["session_baseline_count"], 3);

    let r2 = submit(&runtime, &id, version(&r1), "confirm_red").await;
    assert_eq!(state(&r2), "red");

    let r3 = submit(&runtime, &id, version(&r2), "confirm_green").await;
    assert_eq!(state(&r3), "green");
    assert_eq!(r3["context"]["cycles_completed"], 1);
    // Per-cycle baseline rolled forward; session baseline pinned at 3.
    assert_eq!(r3["context"]["baseline_count"], 4);
    assert_eq!(r3["context"]["session_baseline_count"], 3);

    let r4 = submit(&runtime, &id, version(&r3), "finish").await;
    assert_eq!(state(&r4), "done");
    assert_eq!(r4["result"]["status"], "succeeded");
}

// ---------- discipline: confirm_red requires a NEW failing test ---------

#[tokio::test]
async fn confirm_red_loops_when_count_didnt_strictly_increase() {
    // The agent runs confirm_red without writing a new test. Even though
    // the suite has unrelated failures, count == baseline → no advance.
    // Previously a `>=` check would have rubber-stamped the advance.
    let (runtime, _) = build(&[
        r(true, 5),  // baseline=5, session_baseline=5
        r(false, 5), // confirm_red: tests fail BUT count unchanged → loop
        r(false, 6), // retry: agent actually wrote a test → red
    ]);
    let (id, v0, _) = start(&runtime).await;
    let r1 = submit(&runtime, &id, v0, "start_cycle").await;

    let r2 = submit(&runtime, &id, version(&r1), "confirm_red").await;
    assert_eq!(state(&r2), "red_pending"); // strict: must add a test
    assert_eq!(r2["context"]["last_count"], 5);

    let r3 = submit(&runtime, &id, version(&r2), "confirm_red").await;
    assert_eq!(state(&r3), "red");
}

// ---------- cheating: tests pass without count going up ------------------

#[tokio::test]
async fn cheated_when_test_deleted_during_implementation_phase() {
    // The "delete-then-claim-green" attack:
    //   start_cycle: baseline=3, session_baseline=3
    //   confirm_red: agent wrote a real failing test → count=4, fail → red ✓
    //   (between red and confirm_green: agent deletes the new failing test)
    //   confirm_green: count=3 (deletion brought count back to baseline),
    //                  passed=true (suite passes because the failing test is
    //                  gone). 3 <= baseline(3) → cheated.
    let (runtime, audit) = build(&[
        r(true, 3),
        r(false, 4),
        r(true, 3), // count dropped between red and green
    ]);
    let (id, v0, _) = start(&runtime).await;
    let r1 = submit(&runtime, &id, v0, "start_cycle").await;
    let r2 = submit(&runtime, &id, version(&r1), "confirm_red").await;
    assert_eq!(state(&r2), "red");

    let r3 = submit(&runtime, &id, version(&r2), "confirm_green").await;
    assert_eq!(state(&r3), "cheated");
    assert_eq!(r3["result"]["status"], "succeeded"); // terminal status

    let events = audit.snapshot();
    assert!(events.iter().any(|e| e.event_type == "transition.branched"));
}

/// Note on what counts CAN'T catch: if the agent writes a real failing
/// test (count strictly increases), then in the implementation phase
/// changes the assertion to `assert True` (count is unchanged), the
/// confirm_green cheating branch sees count > baseline and routes to
/// green. This is mutation-testing territory — the count guard cannot
/// distinguish "I wrote real impl" from "I weakened the test."
///
/// The workflow's defense is layered:
///   - count guards catch deletions and "no new test" attacks
///   - mutation testing (external) is the only declarative answer to
///     trivialization
///   - human review of the diff is the universal fallback

// ---------- cheating: count drops between baseline and red --------------

#[tokio::test]
async fn cheated_when_test_deleted_during_red_phase() {
    // Sequence:
    //   baseline: count=5 (session_baseline=5)
    //   confirm_red: count dropped to 4 → cheated
    //   (count < session_baseline triggers; test failure status irrelevant)
    let (runtime, _) = build(&[
        r(true, 5),
        r(false, 4), // count went DOWN → tests were deleted
    ]);
    let (id, v0, _) = start(&runtime).await;
    let r1 = submit(&runtime, &id, v0, "start_cycle").await;
    let r2 = submit(&runtime, &id, version(&r1), "confirm_red").await;
    assert_eq!(state(&r2), "cheated");
}

// ---------- cross-cycle attack: slow deletion across multiple cycles ----

#[tokio::test]
async fn cheated_when_slow_cross_cycle_deletion_returns_below_session_baseline() {
    // The "slow rot" attack: each individual cycle looks legit (per-cycle
    // baseline is satisfied), but the cumulative effect deletes tests
    // that were there at session start.
    //
    //   start: session_baseline = baseline = 10
    //   cycle 1: add 1 test (count → 11), legitimate green     → baseline=11
    //   cycle 2: agent deletes 2 old tests + adds 1 new test
    //            confirm_red sees count=10 (=11-2+1)
    //            → 10 < session_baseline(10)? NO; 10 == session_baseline.
    //            → 10 < baseline(11)? YES → cheated
    //
    // (If we only used the per-cycle baseline, the attack would still be
    //  caught here. The session baseline closes a different hole: agent
    //  runs many cycles, slowly deleting one per cycle while adding
    //  one — per-cycle math is fine but cumulative count drops.)
    let (runtime, _) = build(&[
        r(true, 10),  // session_baseline=10, baseline=10
        r(false, 11), // cycle 1 confirm_red: legitimate
        r(true, 11),  // cycle 1 confirm_green: → green; baseline rolls to 11
        r(false, 10), // cycle 2 confirm_red: count fell to 10 < baseline(11)
    ]);
    let (id, v0, _) = start(&runtime).await;
    let r1 = submit(&runtime, &id, v0, "start_cycle").await;
    let r2 = submit(&runtime, &id, version(&r1), "confirm_red").await;
    let r3 = submit(&runtime, &id, version(&r2), "confirm_green").await;
    assert_eq!(state(&r3), "green");
    let r4 = submit(&runtime, &id, version(&r3), "start_new_cycle").await;
    assert_eq!(state(&r4), "red_pending");
    let r5 = submit(&runtime, &id, version(&r4), "confirm_red").await;
    assert_eq!(state(&r5), "cheated");
}

#[tokio::test]
async fn cheated_when_count_below_session_baseline_even_above_cycle_baseline() {
    // The case the per-cycle baseline DOESN'T catch but session does:
    //   start: session_baseline=5, baseline=5
    //   confirm_red: agent adds 1 test, deletes 2 → count=4
    //
    // 4 < session_baseline(5) → cheated. Per-cycle baseline check would
    // ALSO catch this (4 < 5), but the session check is what matters when
    // multi-cycle scenarios push baseline upward — it's the floor.
    let (runtime, _) = build(&[r(true, 5), r(false, 4)]);
    let (id, v0, _) = start(&runtime).await;
    let r1 = submit(&runtime, &id, v0, "start_cycle").await;
    let r2 = submit(&runtime, &id, version(&r1), "confirm_red").await;
    assert_eq!(state(&r2), "cheated");
    assert_eq!(r2["context"]["session_baseline_count"], 5);
    assert_eq!(r2["context"]["last_count"], 4);
}

// ---------- cheating: count drops during refactor -----------------------

#[tokio::test]
async fn cheated_when_test_deleted_during_refactor() {
    let (runtime, _) = build(&[
        r(true, 3),  // baseline=3, session_baseline=3
        r(false, 4), // red: count strictly increased AND tests fail
        r(true, 4),  // green: legitimate; baseline rolls to 4
        r(true, 3),  // refactor "confirm": tests pass but count dropped → cheated
    ]);
    let (id, v0, _) = start(&runtime).await;
    let r1 = submit(&runtime, &id, v0, "start_cycle").await;
    let r2 = submit(&runtime, &id, version(&r1), "confirm_red").await;
    let r3 = submit(&runtime, &id, version(&r2), "confirm_green").await;
    assert_eq!(state(&r3), "green");

    let r4 = submit(&runtime, &id, version(&r3), "start_refactor").await;
    assert_eq!(state(&r4), "refactoring");

    let r5 = submit(&runtime, &id, version(&r4), "confirm_refactor").await;
    assert_eq!(state(&r5), "cheated");
}

// ---------- discipline: caller didn't actually write a failing test -----

#[tokio::test]
async fn red_pending_loops_when_tests_pass_unexpectedly() {
    // Tests passed at red-confirm time AND count didn't drop → not
    // deletion, just no new failing test → loop in red_pending.
    let (runtime, _) = build(&[
        r(true, 3),  // baseline=3, session_baseline=3
        r(true, 3),  // confirm_red: tests pass (suspicious) → loop
        r(false, 4), // confirm_red retry: failing test added → red
        r(true, 4),  // green legitimate
    ]);
    let (id, v0, _) = start(&runtime).await;
    let r1 = submit(&runtime, &id, v0, "start_cycle").await;
    let r2 = submit(&runtime, &id, version(&r1), "confirm_red").await;
    assert_eq!(state(&r2), "red_pending"); // looped

    let r3 = submit(&runtime, &id, version(&r2), "confirm_red").await;
    assert_eq!(state(&r3), "red");

    let r4 = submit(&runtime, &id, version(&r3), "confirm_green").await;
    assert_eq!(state(&r4), "green");
}

// ---------- discipline: implementation didn't actually pass -------------

#[tokio::test]
async fn red_loops_when_implementation_doesnt_pass() {
    let (runtime, _) = build(&[
        r(true, 3),
        r(false, 4), // red: count strictly increased AND tests fail
        r(false, 4), // green confirm: still failing → stay red
        r(true, 4),  // retry: passes; count(4) > baseline(3) → green
    ]);
    let (id, v0, _) = start(&runtime).await;
    let r1 = submit(&runtime, &id, v0, "start_cycle").await;
    let r2 = submit(&runtime, &id, version(&r1), "confirm_red").await;
    let r3 = submit(&runtime, &id, version(&r2), "confirm_green").await;
    assert_eq!(state(&r3), "red");

    let r4 = submit(&runtime, &id, version(&r3), "confirm_green").await;
    assert_eq!(state(&r4), "green");
}

// ---------- multi-cycle: baseline rolls forward each cycle --------------

#[tokio::test]
async fn baseline_rolls_forward_so_each_cycle_must_add_a_test() {
    // After cycle 1 ends, green.onEnter rolls baseline_count forward to
    // the latest count. start_new_cycle is pure navigation, so baseline
    // stays at the rolled-forward value. The agent must add another
    // strictly-failing test before confirm_red advances again.
    //
    // session_baseline_count never moves — pinned at 3 from start_cycle.
    let (runtime, _) = build(&[
        r(true, 3),  // start_cycle 1: baseline=3, session_baseline=3
        r(false, 4), // confirm_red 1: count strictly up, fail → red
        r(true, 4),  // confirm_green 1: pass; baseline → 4
        // start_new_cycle (no executor)
        r(false, 5), // confirm_red 2: count up from baseline 4 → red
        r(true, 5),  // confirm_green 2: pass; baseline → 5
    ]);
    let (id, v0, _) = start(&runtime).await;
    let r1 = submit(&runtime, &id, v0, "start_cycle").await;
    let r2 = submit(&runtime, &id, version(&r1), "confirm_red").await;
    let r3 = submit(&runtime, &id, version(&r2), "confirm_green").await;
    assert_eq!(state(&r3), "green");
    assert_eq!(r3["context"]["cycles_completed"], 1);
    assert_eq!(r3["context"]["baseline_count"], 4);
    assert_eq!(r3["context"]["session_baseline_count"], 3); // never moves

    let r4 = submit(&runtime, &id, version(&r3), "start_new_cycle").await;
    assert_eq!(state(&r4), "red_pending");
    assert_eq!(r4["context"]["baseline_count"], 4); // rolled forward, not re-baselined

    let r5 = submit(&runtime, &id, version(&r4), "confirm_red").await;
    assert_eq!(state(&r5), "red");
    let r6 = submit(&runtime, &id, version(&r5), "confirm_green").await;
    assert_eq!(state(&r6), "green");
    assert_eq!(r6["context"]["cycles_completed"], 2);
    assert_eq!(r6["context"]["baseline_count"], 5);
    assert_eq!(r6["context"]["session_baseline_count"], 3); // still pinned
}
