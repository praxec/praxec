//! Loads a resolved config and drives every definition N seeded times with the
//! smart mock registry + failure injection + fuzz chooser, classifying each run.

use std::collections::HashSet;
use std::path::Path;
use std::sync::{Arc, Mutex};

use praxec_agents::orchestrator::{
    HeadlessPolicy, MissionGateway, RuntimeMissionGateway, drive_mission, run_headless_consumer,
};
use praxec_core::WorkflowRuntime;
use praxec_core::audit::{AuditSink, MemoryAuditSink};
use praxec_core::bus::Bus;
use praxec_core::config;
use praxec_core::guards::DefaultGuardEvaluator;
use praxec_core::model::{Principal, StartWorkflow};
use praxec_core::ports::{DefinitionStore, ExecutorRegistry};
use praxec_core::store::{ConfigDefinitionStore, InMemoryWorkflowStore};
use serde_json::Value;

use crate::chooser::FuzzChooser;
use crate::oracle::classify_run;

pub use crate::oracle::RunVerdict;

const STEP_BUDGET: usize = 40;
// bounds livelock wall-time; flows that traverse under the mock need far fewer steps

// Percent of executor calls to inject as Permanent failures (0–100).
//
// When an executor returns Err(Permanent), the runtime records
// `transition.rejected` (EXECUTOR_FAILED) and responds with
// StatusHint::Failed → MissionStatus::Failed(Error) → status "failed".
// drive_mission sees `status == "failed"` → DriveOutcome::Resolved → Pass.
// A Permanent executor failure therefore does NOT produce an EngineError
// violation; it terminates the mission as "failed" (a resolved outcome the
// oracle accepts as Pass). Rate 15 is safe: it exercises failure paths while
// leaving the sound fixture's single-transition flow resolvable on every run.
const FAILURE_RATE: u8 = 15;

pub struct ScenarioResult {
    pub seed: u64,
    pub verdict: RunVerdict,
    pub final_status: String,
    pub states_visited: Vec<String>,
    pub outcomes_met: Vec<String>,
}

pub struct DefResult {
    pub definition_id: String,
    pub scenarios: Vec<ScenarioResult>,
}

pub struct FuzzReport {
    pub results: Vec<DefResult>,
    pub transitions_covered: usize,
    /// Count of definitions that had at least one violating scenario.
    pub definitions_with_violations: usize,
}

pub async fn fuzz_config(
    config_path: &Path,
    iterations: usize,
    base_seed: u64,
) -> anyhow::Result<FuzzReport> {
    let (resolved, _diags) = config::load_resolved_with_repos(config_path)?;
    let ids = ConfigDefinitionStore::from_config(&resolved).ids();
    let coverage = Arc::new(Mutex::new(HashSet::<String>::new()));

    let mut results = Vec::new();
    for id in ids {
        let scenarios =
            run_scenarios_for(&resolved, &id, iterations, base_seed, coverage.clone()).await?;
        results.push(DefResult {
            definition_id: id,
            scenarios,
        });
    }

    let transitions_covered = coverage.lock().expect("coverage lock").len();
    let definitions_with_violations = results
        .iter()
        .filter(|d| d.scenarios.iter().any(|s| s.verdict.is_violation()))
        .count();
    Ok(FuzzReport {
        results,
        transitions_covered,
        definitions_with_violations,
    })
}

/// Run `iterations` scenarios for a single definition, returning all results.
///
/// Uses its own fresh coverage set — suitable for targeted per-definition
/// fuzzing. `fuzz_config` uses a shared coverage set via `run_scenarios_for`
/// so the aggregate `transitions_covered` count is correct.
pub async fn fuzz_definition(
    resolved: &serde_json::Value,
    definition_id: &str,
    iterations: usize,
    base_seed: u64,
) -> anyhow::Result<Vec<ScenarioResult>> {
    let coverage = Arc::new(Mutex::new(HashSet::<String>::new()));
    run_scenarios_for(resolved, definition_id, iterations, base_seed, coverage).await
}

/// Inner loop shared by `fuzz_config` and `fuzz_definition`.
async fn run_scenarios_for(
    resolved: &serde_json::Value,
    definition_id: &str,
    iterations: usize,
    base_seed: u64,
    coverage: Arc<Mutex<HashSet<String>>>,
) -> anyhow::Result<Vec<ScenarioResult>> {
    let mut scenarios = Vec::new();
    for i in 0..iterations {
        let seed = base_seed.wrapping_add(i as u64);
        scenarios.push(run_one(resolved, definition_id, seed, coverage.clone()).await?);
    }
    Ok(scenarios)
}

async fn run_one(
    resolved: &Value,
    definition_id: &str,
    seed: u64,
    coverage: Arc<Mutex<HashSet<String>>>,
) -> anyhow::Result<ScenarioResult> {
    let definitions: Arc<dyn DefinitionStore> =
        Arc::new(ConfigDefinitionStore::from_config(resolved));
    let store = Arc::new(InMemoryWorkflowStore::new());

    // Build the smart mock from the definition's plan so guard-gated flows
    // traverse, with seeded failure injection to exercise error-handling paths.
    // Index by literal key — definition IDs can contain `/` (namespaced includes
    // like `cognitive/flow.add-feature`), which JSON-Pointer would mis-split into
    // nested path segments, yielding an empty def (no inputSchema, no mock plan).
    let def_json = resolved
        .get("workflows")
        .and_then(|w| w.get(definition_id))
        .cloned()
        .unwrap_or(serde_json::json!({}));
    let mut plan = crate::analysis::plan::derive_plan(&def_json);
    crate::analysis::plan::add_capability_outputs(&mut plan, &def_json, resolved);
    let executors: Arc<dyn ExecutorRegistry> = Arc::new(
        crate::smartmock::SmartMockRegistry::with_injection(plan, seed, FAILURE_RATE),
    );

    let guards = Arc::new(DefaultGuardEvaluator::new());
    let audit = Arc::new(MemoryAuditSink::new());

    let runtime = Arc::new(WorkflowRuntime::new(
        definitions,
        store,
        executors,
        guards,
        audit.clone() as Arc<dyn AuditSink>,
    ));

    // Seed the start input from the workflow's `inputSchema` so flows with
    // required inputs actually start (an empty `{}` would fail validation and be
    // mis-reported as an EngineError "can't execute" — a harness gap, not a defect).
    let start_input = def_json
        .get("inputSchema")
        .map(crate::analysis::dummy::dummy_all_properties)
        .unwrap_or_else(|| serde_json::json!({}));

    let resp = match runtime
        .start(StartWorkflow {
            definition_id: definition_id.to_string(),
            input: start_input,
            principal: Principal::anonymous(),
            trace_id: None,
            run_id: None,
            depth: 0,
            parent: None,
        })
        .await
    {
        Ok(r) => r,
        Err(e) => {
            return Ok(ScenarioResult {
                seed,
                verdict: crate::oracle::RunVerdict::EngineError(format!("start failed: {e}")),
                final_status: "failed".to_string(),
                states_visited: vec![],
                outcomes_met: vec![],
            });
        }
    };
    let mission_id = resp
        .pointer("/workflow/id")
        .and_then(Value::as_str)
        .ok_or_else(|| {
            anyhow::anyhow!("started '{definition_id}' but the response carried no /workflow/id")
        })?
        .to_string();

    let gateway = RuntimeMissionGateway::new(runtime, Principal::anonymous());
    let chooser = FuzzChooser::new(seed, coverage);

    let bus = Bus::new();
    let events = bus.subscribe();
    let consumer = tokio::spawn(run_headless_consumer(
        events,
        bus.clone(),
        HeadlessPolicy::AutoApprove,
    ));
    let outcome = drive_mission(&gateway, &chooser, &bus, &mission_id, STEP_BUDGET).await;
    consumer.abort();

    // Snapshot audit BEFORE the final state query so we capture all events.
    let events = audit.snapshot();

    let final_state = gateway
        .query(&mission_id)
        .await
        .unwrap_or_else(|_| crate::oracle::oracle_unknown_state(&mission_id));
    let final_status = final_state.status.clone();
    let verdict = classify_run(&outcome, &final_state);

    // Collect distinct non-empty state names in first-seen order.
    let mut seen = HashSet::<String>::new();
    let mut states_visited = Vec::new();
    for event in &events {
        if let Some(state) = event
            .payload
            .get("state")
            .and_then(serde_json::Value::as_str)
        {
            if !state.is_empty() && seen.insert(state.to_string()) {
                states_visited.push(state.to_string());
            }
        }
    }

    // Collect ids of outcomes that were met in the final state.
    let outcomes_met = final_state
        .outcomes
        .iter()
        .filter(|o| o.met)
        .map(|o| o.id.clone())
        .collect();

    Ok(ScenarioResult {
        seed,
        verdict,
        final_status,
        states_visited,
        outcomes_met,
    })
}
