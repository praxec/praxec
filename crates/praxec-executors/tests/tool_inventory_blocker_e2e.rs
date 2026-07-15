//! Regression for praxec #01 — "meta authoring pack blocked."
//!
//! The consumer's repro: `meta/flow.author-capability` parked at state
//! `surveying_tools`, whose `survey` transition invokes the
//! `meta/cap.research.tool-inventory` sub-workflow. That cap used an `actor:
//! agent` step with no tool to enumerate the gateway, so it burned the 900s step
//! budget (`AGENT_STEP_BUDGET_EXHAUSTED`) / timed out and the whole flow failed
//! with `CHAIN_FAILED` — making every authoring flow unusable.
//!
//! The fix makes the cap deterministic (`kind: inventory`). This test drives the
//! ACTUAL shipped fixture pack (not a synthetic config) through the real
//! `InventoryExecutor` and asserts:
//!   1. the cap completes deterministically in-process — no agent, no budget;
//!   2. it accepts the `filter` input the parent maps in (`use.inputs.filter`),
//!      even though the deterministic cap no longer declares it;
//!   3. the parent flow advances PAST `surveying_tools` instead of CHAIN_FAILED.

use std::path::PathBuf;
use std::sync::Arc;

use praxec_core::audit::{AuditSink, MemoryAuditSink};
use praxec_core::config::load_resolved_with_repos;
use praxec_core::discovery::{DiscoveryIndex, InMemoryDiscoveryIndex};
use praxec_core::guards::DefaultGuardEvaluator;
use praxec_core::model::{Principal, StartWorkflow};
use praxec_core::overlay::SingleKindOverlay;
use praxec_core::ports::{ExecutorRegistry, WorkflowStore};
use praxec_core::runtime::WorkflowRuntime;
use praxec_core::store::{ConfigDefinitionStore, InMemoryEvidenceStore, InMemoryWorkflowStore};
use praxec_executors::{
    CliConnections, InventoryExecutor, McpConnections, McpExecutor,
    default_registry_with_late_workflow,
};
use serde_json::{Value, json};
use tempfile::TempDir;

fn fixture_path() -> PathBuf {
    let mut p = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    p.pop();
    p.pop();
    p.push("crates/praxec-core/tests/fixtures/praxec-meta");
    p
}

/// Minimal host config: point at the fixture pack + the lexicon entries its
/// subject-walk requires (mirrors `meta_flows_e2e`).
fn write_host_config(td: &TempDir) -> PathBuf {
    let body = format!(
        "version: \"1.0.0\"\nrepos:\n  - path: \"{path}\"\n\
         lexicon:\n\
           capability-harness:           {{ definition_short: \"Capability harness verification.\" }}\n\
           provider-model-inventory:     {{ definition_short: \"Provider model inventory fetch.\" }}\n\
           agents-config:                {{ definition_short: \"Agents configuration install.\" }}\n\
           mine-praxec-transitions:      {{ definition_short: \"Praxec transition mining.\" }}\n\
           praxec.check:                 {{ definition_short: \"MCP praxec config check.\" }}\n\
           auth-only-smoke-test:         {{ definition_short: \"Auth-only smoke test.\" }}\n\
           emit-praxec-yaml:             {{ definition_short: \"Emit praxec YAML skill.\" }}\n\
           lexicon-extend:               {{ definition_short: \"Lexicon extension skill.\" }}\n\
           audit-mining:                 {{ definition_short: \"Audit mining research skill.\" }}\n\
           code.adversarial:             {{ definition_short: \"Adversarial code review skill.\" }}\n\
           compose-implementation:       {{ definition_short: \"Implementation composition skill.\" }}\n\
           suggest-bindings:             {{ definition_short: \"Binding suggestion skill.\" }}\n\
           tool-inventory.assemble:      {{ definition_short: \"Tool inventory assembly.\" }}\n\
           audit.mine-transitions:       {{ definition_short: \"Mine praxec transitions.\" }}\n\
           review.adversarial:           {{ definition_short: \"Adversarial review capability.\" }}\n\
           gate.human-approve-plan:      {{ definition_short: \"Human plan approval gate.\" }}\n\
           summarize.lexicon-define:     {{ definition_short: \"Lexicon define summarization.\" }}\n\
           implement.write-agents-config: {{ definition_short: \"Write agents config.\" }}\n\
           coordinate.pr-open:           {{ definition_short: \"Open a pull request.\" }}\n\
           gate.human-pick-shape:        {{ definition_short: \"Human shape picker gate.\" }}\n\
           verify.capability-harness:    {{ definition_short: \"Capability harness check.\" }}\n\
           verify.auth-only-smoke-test:  {{ definition_short: \"Auth smoke test verify.\" }}\n\
           verify.check-config:          {{ definition_short: \"Config check verify.\" }}\n\
           research.tool-inventory:      {{ definition_short: \"Tool inventory research.\" }}\n\
           plan.suggest-bindings:        {{ definition_short: \"Binding suggestion plan.\" }}\n\
           implement.emit-yaml:          {{ definition_short: \"Emit YAML implementation.\" }}\n\
           research.model-inventory:     {{ definition_short: \"Model inventory research.\" }}\n\
           plan.compose-implementation:  {{ definition_short: \"Implementation composition plan.\" }}\n\
           research.lexicon-lookup:      {{ definition_short: \"Lexicon lookup research.\" }}\n",
        path = fixture_path().display()
    );
    let p = td.path().join("praxec.yaml");
    std::fs::write(&p, body).unwrap();
    p
}

/// Real runtime with the default registry + the `inventory` executor overlaid
/// (backed by a discovery index over the loaded pack) + a late-bound `workflow`
/// executor — exactly the production wiring, minus the model-backed executors.
fn build_runtime(config: &Value) -> WorkflowRuntime {
    let audit = Arc::new(MemoryAuditSink::new());
    let definitions = Arc::new(ConfigDefinitionStore::from_config(config));
    let store: Arc<dyn WorkflowStore> = Arc::new(InMemoryWorkflowStore::new());
    let evidence = Arc::new(InMemoryEvidenceStore::new());
    let guards = Arc::new(DefaultGuardEvaluator::with_evidence(evidence.clone()));

    let discovery: Arc<dyn DiscoveryIndex> = Arc::new(
        InMemoryDiscoveryIndex::from_config(config).expect("discovery index builds from pack"),
    );

    let (registry, workflow_handle) = default_registry_with_late_workflow(
        config,
        Arc::new(McpExecutor::new(McpConnections::from_config(config))),
        Arc::new(CliConnections::from_config(config)),
        audit.clone() as Arc<dyn AuditSink>,
    );
    let registry: Arc<dyn ExecutorRegistry> = Arc::new(SingleKindOverlay::new(
        registry,
        "inventory",
        Arc::new(InventoryExecutor::new(discovery)),
    ));

    let runtime = WorkflowRuntime::new(
        definitions,
        store,
        registry,
        guards,
        audit as Arc<dyn AuditSink>,
    )
    .with_writable_repo_roots(vec![praxec_core::RepoRoot::for_test()])
    .with_evidence(evidence);
    workflow_handle.set_runtime(runtime.clone());
    runtime
}

async fn start(runtime: &WorkflowRuntime, definition_id: &str, input: Value) -> Value {
    runtime
        .start(StartWorkflow {
            definition_id: definition_id.to_string(),
            input,
            principal: Principal::anonymous(),
            run_env: praxec_core::RunEnv::for_test(),
            depth: 0,
            parent: None,
        })
        .await
        .unwrap_or_else(|e| panic!("start({definition_id}): {e}"))
}

/// Criterion 1 + the `filter`-input concern: the shipped cap runs the real
/// deterministic survey to terminal, accepting the `filter` input the parent
/// maps in, and produces the typed inventory — instantly, no agent, no budget.
#[tokio::test]
async fn tool_inventory_cap_completes_deterministically_with_parent_filter_input() {
    let td = TempDir::new().unwrap();
    let host = write_host_config(&td);
    let (config, _diags) = load_resolved_with_repos(&host).expect("config loads");
    let runtime = build_runtime(&config);

    // The parent maps `use.inputs.filter: "$.context.goal"` — a free-text goal.
    // Pass exactly that shape to prove the cap accepts it post-reshape.
    let resp = start(
        &runtime,
        "meta/cap.research.tool-inventory",
        json!({ "filter": "cap.verify.dotnet: run dotnet build then dotnet test; emit verifyOut; fail-closed." }),
    )
    .await;

    assert_eq!(
        resp["result"]["status"], "succeeded",
        "survey must complete, not fail on the parent's filter input: {resp:#}"
    );
    assert_eq!(resp["workflow"]["state"], "done", "resp: {resp:#}");
    assert!(
        resp.pointer("/context/inventory/counts").is_some(),
        "survey must emit the typed inventory: {resp:#}"
    );
}

/// V30 — the shipped meta pack must be contract-clean: no `use`-binding drift
/// between any `kind: workflow` step and the definition it references. This is
/// the guard that keeps `filter`-style dead bindings (and any missing-required
/// or unknown-output drift) out of the pack.
#[tokio::test]
async fn meta_pack_has_no_use_binding_contract_drift() {
    let td = TempDir::new().unwrap();
    let host = write_host_config(&td);
    let (config, _diags) = load_resolved_with_repos(&host).expect("config loads");
    let drift: Vec<String> = praxec_core::validate::validate_workflows(&config)
        .into_iter()
        .filter_map(|d| match d {
            praxec_core::validate::Diagnostic::Error(m)
            | praxec_core::validate::Diagnostic::Warning(m)
                if m.contains("USE_BINDING_CONTRACT_DRIFT") =>
            {
                Some(m)
            }
            _ => None,
        })
        .collect();
    assert!(
        drift.is_empty(),
        "meta pack has {} use-binding contract drift(s):\n  - {}",
        drift.len(),
        drift.join("\n  - ")
    );
}

/// Criterion 3: the parent flow advances PAST `surveying_tools` instead of the
/// consumer's `CHAIN_FAILED` at that state. Downstream steps are model-backed
/// (not wired here), so the flow parks further along — the point is only that
/// the survey leg no longer blocks it.
#[tokio::test]
async fn author_capability_advances_past_surveying_tools() {
    let td = TempDir::new().unwrap();
    let host = write_host_config(&td);
    let (config, _diags) = load_resolved_with_repos(&host).expect("config loads");
    let runtime = build_runtime(&config);

    let resp = start(
        &runtime,
        "meta/flow.author-capability",
        json!({ "goal": "cap.verify.dotnet: dotnet build then test; fail-closed.", "namespace": "draft" }),
    )
    .await;

    let state = resp
        .pointer("/workflow/state")
        .and_then(Value::as_str)
        .unwrap_or("?");
    let status = resp
        .pointer("/result/status")
        .and_then(Value::as_str)
        .unwrap_or("?");
    assert_ne!(
        state, "surveying_tools",
        "flow must advance past the survey leg; still stuck: {resp:#}"
    );
    assert_ne!(
        status, "failed",
        "survey leg must not CHAIN_FAILED: {resp:#}"
    );
}
