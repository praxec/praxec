//! M4 acceptance — walk each of cognitive-architectures v0.2's four
//! shipping flows (`flow.add-feature`, `flow.bugfix-from-error-log`,
//! `flow.safe-refactor`, `flow.triage-issue`) through their full lifecycle
//! to a terminal state.
//!
//! ## Fixture executor (NOT WorkflowExecutor)
//!
//! In production, the flow's `kind: workflow` transitions are
//! dispatched by `WorkflowExecutor`, which `runtime.start`s the cap
//! sub-workflow and polls until completion. Cognitive caps are
//! agent-driven (`kind: noop + actor: agent`); they'd block forever
//! without an LLM driver submitting per-cap arguments.
//!
//! Tests can't supply an LLM. So we register a fixture executor for
//! `kind: workflow` that short-circuits: receives the flow's
//! `executor_config` (including `_snippetOutputs` embedded by the
//! config-resolve pass), synthesizes valid outputs per the snippet
//! schema, returns them directly. The flow's projection layer
//! merges them as if the cap had really run.
//!
//! This proves the flow state machine + use-binding projection
//! work end-to-end. Cap-internal behavior is tested by each cap's own
//! integration tests (operator-owned, not part of M4).
//!
//! ## Test location
//!
//! Lives in praxec-executors (not -core) to avoid a build cycle:
//! the test uses helpers from this crate's tests/ folder pattern and
//! must NOT add a core→executors dev-dep.

use std::path::PathBuf;
use std::sync::Arc;

use async_trait::async_trait;
use praxec_core::audit::{AuditSink, MemoryAuditSink};
use praxec_core::config::load_resolved_with_repos;
use praxec_core::error::ExecutorError;
use praxec_core::guards::DefaultGuardEvaluator;
use praxec_core::model::{ExecuteRequest, ExecuteResult, Principal, StartWorkflow};
use praxec_core::ports::{Executor, ExecutorRegistry, WorkflowStore};
use praxec_core::runtime::WorkflowRuntime;
use praxec_core::store::{ConfigDefinitionStore, InMemoryEvidenceStore, InMemoryWorkflowStore};
use serde_json::{Value, json};
use tempfile::TempDir;

/// Fixture executor for `kind: workflow` — short-circuits cap invocation
/// by synthesizing outputs per the embedded `_snippetOutputs` schema.
/// Returns a result keyed by capability output name (matching what
/// `WorkflowExecutor` would have produced post-projection, so the
/// flow's synthesized transition output mapping projects to
/// host slots correctly).
struct CapShortCircuit;

#[async_trait]
impl Executor for CapShortCircuit {
    async fn execute(&self, request: ExecuteRequest) -> Result<ExecuteResult, ExecutorError> {
        // The expand_use_bindings pass embeds the target capability's
        // snippet.outputs as `_snippetOutputs` on the executor config.
        // We use that schema to synthesize a valid example value per
        // declared output.
        let snippet_outputs = request
            .executor_config
            .get("_snippetOutputs")
            .cloned()
            .unwrap_or_else(|| json!({}));
        let outputs = synthesize_outputs(&snippet_outputs);
        Ok(ExecuteResult {
            output: outputs,
            evidence: vec![],
            child_workflow_id: Some("fixture-cap-instance".to_string()),
            next_transition: None,
            suspend: None,
            telemetry: None,
        })
    }
}

/// Walk the snippet outputs schema; emit a valid example value for
/// each declared output. Keyed by cap output name (NOT host path) —
/// the synthesized transition output mapping in the flow
/// projects from `$.output.<cap_output_name>` to host slots.
fn synthesize_outputs(snippet_outputs: &Value) -> Value {
    let Some(obj) = snippet_outputs.as_object() else {
        return json!({});
    };
    let mut out = serde_json::Map::new();
    for (name, schema) in obj {
        out.insert(name.clone(), synthesize_one(schema));
    }
    Value::Object(out)
}

fn synthesize_one(schema: &Value) -> Value {
    let ty = schema
        .get("type")
        .and_then(Value::as_str)
        .unwrap_or("string");
    // Enum constraint wins — use first allowed value.
    if let Some(enum_vals) = schema.get("enum").and_then(Value::as_array) {
        if let Some(first) = enum_vals.first() {
            return first.clone();
        }
    }
    match ty {
        "string" => json!("fixture-value"),
        "integer" => json!(0),
        "number" => json!(0.0),
        "boolean" => json!(true),
        "array" => json!([]),
        "object" => json!({}),
        _ => Value::Null,
    }
}

struct NoopExecutor;
#[async_trait]
impl Executor for NoopExecutor {
    async fn execute(&self, _r: ExecuteRequest) -> Result<ExecuteResult, ExecutorError> {
        Ok(ExecuteResult::default())
    }
}

/// Registry: `kind: workflow` short-circuits via CapShortCircuit;
/// everything else returns NoopExecutor.
struct FixtureRegistry;
impl ExecutorRegistry for FixtureRegistry {
    fn get(&self, kind: &str) -> Option<Arc<dyn Executor>> {
        match kind {
            "workflow" => Some(Arc::new(CapShortCircuit)),
            _ => Some(Arc::new(NoopExecutor)),
        }
    }
}

fn fixture_path() -> PathBuf {
    let mut p = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    p.pop();
    p.pop();
    p.push("crates");
    p.push("praxec-core");
    p.push("tests");
    p.push("fixtures");
    p.push("cognitive-architectures");
    p
}

fn write_host_config(td: &TempDir) -> PathBuf {
    // Lexicon entries for all subjects referenced by scripts, skills, and
    // capabilities in the cognitive-architectures fixture. Required so the
    // pre-start subject walk (SPEC §30.10.4) does not block workflow starts.
    // Entries are intentionally minimal — definition_short is the only
    // required field for a resolved lexicon entry.
    let body = format!(
        "version: \"1.0.0\"\nrepos:\n  - path: \"{path}\"\n\
         lexicon:\n\
           # scripts\n\
           cargo.release:             {{ definition_short: \"Cargo release build.\" }}\n\
           full-sweep:                {{ definition_short: \"Full CI sweep.\" }}\n\
           cargo-install:             {{ definition_short: \"Cargo install step.\" }}\n\
           rust.check:                {{ definition_short: \"Rust format check.\" }}\n\
           cargo.dependency-tree:     {{ definition_short: \"Cargo dependency analysis.\" }}\n\
           rust.clippy-strict:        {{ definition_short: \"Strict Clippy lint.\" }}\n\
           codebase.ripgrep:          {{ definition_short: \"Ripgrep codebase search.\" }}\n\
           baseline.compare:          {{ definition_short: \"Baseline comparison test.\" }}\n\
           baseline.snapshot:         {{ definition_short: \"Baseline snapshot capture.\" }}\n\
           cargo.workspace:           {{ definition_short: \"Cargo workspace test run.\" }}\n\
           workspace.green:           {{ definition_short: \"Workspace green verification.\" }}\n\
           # skills\n\
           skill.script-shape:        {{ definition_short: \"Script shape authoring rubric.\" }}\n\
           integration:               {{ definition_short: \"Integration composition skill.\" }}\n\
           plan.vet:                  {{ definition_short: \"Plan vetting skill.\" }}\n\
           safety.checklist:          {{ definition_short: \"Deploy safety checklist.\" }}\n\
           codebase.search:           {{ definition_short: \"Codebase search skill.\" }}\n\
           error-trace.parse:         {{ definition_short: \"Error trace parsing skill.\" }}\n\
           reproduction:              {{ definition_short: \"Bug reproduction skill.\" }}\n\
           edit.constrained:          {{ definition_short: \"Scope-constrained edit skill.\" }}\n\
           tdd.discipline:            {{ definition_short: \"TDD discipline skill.\" }}\n\
           fix.scope-bounded:         {{ definition_short: \"Scope-bounded fix plan.\" }}\n\
           gap-reconciliation:        {{ definition_short: \"Plan gap reconciliation.\" }}\n\
           specify.change-request:    {{ definition_short: \"Change request specification.\" }}\n\
           scope-bounded:             {{ definition_short: \"Scope-bounded refactor.\" }}\n\
           context.assemble:          {{ definition_short: \"Context assembly skill.\" }}\n\
           code.adversarial:          {{ definition_short: \"Adversarial code review.\" }}\n\
           code.final-approval:       {{ definition_short: \"Final code approval review.\" }}\n\
           session.delta:             {{ definition_short: \"Session delta summarization.\" }}\n\
           issue.routing:             {{ definition_short: \"Issue routing triage.\" }}\n\
           # capabilities\n\
           coordinate.label-and-route: {{ definition_short: \"Label and route coordination.\" }}\n\
           coordinate.pr-open:        {{ definition_short: \"Open a pull request.\" }}\n\
           diagnose.localize:         {{ definition_short: \"Localize defect diagnosis.\" }}\n\
           diagnose.parse-error:      {{ definition_short: \"Parse error diagnosis.\" }}\n\
           diagnose.reproduce:        {{ definition_short: \"Reproduce defect diagnosis.\" }}\n\
           gate.human-disambiguate:   {{ definition_short: \"Human disambiguation gate.\" }}\n\
           gate.human-signoff:        {{ definition_short: \"Human signoff gate.\" }}\n\
           implement.scope-bounded:   {{ definition_short: \"Scope-bounded implementation.\" }}\n\
           implement.tdd-loop:        {{ definition_short: \"TDD implementation loop.\" }}\n\
           plan.draft:                {{ definition_short: \"Plan drafting capability.\" }}\n\
           plan.fix:                  {{ definition_short: \"Fix plan capability.\" }}\n\
           plan.track-gaps:           {{ definition_short: \"Gap tracking capability.\" }}\n\
           refactor.draft:            {{ definition_short: \"Refactor draft capability.\" }}\n\
           research.context-assemble: {{ definition_short: \"Context assembly research.\" }}\n\
           review.adversarial:        {{ definition_short: \"Adversarial review capability.\" }}\n\
           test.baseline-snapshot:    {{ definition_short: \"Baseline snapshot test.\" }}\n\
           test.compare-baseline:     {{ definition_short: \"Baseline comparison test.\" }}\n\
           triage.classify-severity:  {{ definition_short: \"Severity classification triage.\" }}\n\
           triage.route-component:    {{ definition_short: \"Component routing triage.\" }}\n\
           verify.regression-tests:   {{ definition_short: \"Regression test verification.\" }}\n\
           verify.workspace-green:    {{ definition_short: \"Workspace green verification.\" }}\n",
        path = fixture_path().display()
    );
    let p = td.path().join("praxec.yaml");
    std::fs::write(&p, body).unwrap();
    p
}

async fn build_runtime(config: &Value) -> WorkflowRuntime {
    let audit = Arc::new(MemoryAuditSink::new());
    let definitions = Arc::new(ConfigDefinitionStore::from_config(config));
    let store: Arc<dyn WorkflowStore> = Arc::new(InMemoryWorkflowStore::new());
    let evidence = Arc::new(InMemoryEvidenceStore::new());
    let guards = Arc::new(DefaultGuardEvaluator::with_evidence(evidence.clone()));
    let registry = Arc::new(FixtureRegistry) as Arc<dyn ExecutorRegistry>;
    WorkflowRuntime::new(
        definitions,
        store,
        registry,
        guards,
        audit as Arc<dyn AuditSink>,
    )
    .with_evidence(evidence)
}

async fn walk_to_terminal(definition_id: &str, input: Value, config: &Value) -> Value {
    let runtime = build_runtime(config).await;
    let resp = runtime
        .start(StartWorkflow {
            definition_id: definition_id.to_string(),
            input,
            principal: Principal::anonymous(),
            trace_id: None,
            run_id: None,
            depth: 0,
            parent: None,
        })
        .await
        .unwrap_or_else(|e| panic!("start({definition_id}): {e}"));

    let status = resp
        .pointer("/result/status")
        .and_then(Value::as_str)
        .unwrap_or("?");
    let state = resp
        .pointer("/workflow/state")
        .and_then(Value::as_str)
        .unwrap_or("?");
    assert_eq!(
        state, "done",
        "{definition_id} should walk to terminal 'done'; got state='{state}' status='{status}'. \
         resp: {resp:#}"
    );
    assert_eq!(
        status, "succeeded",
        "{definition_id} status; resp: {resp:#}"
    );
    resp
}

#[tokio::test]
async fn flow_add_feature_walks_to_terminal() {
    let td = TempDir::new().unwrap();
    let host_path = write_host_config(&td);
    let (config, _diags) = load_resolved_with_repos(&host_path).expect("config loads");
    let resp = walk_to_terminal(
        "cognitive/flow.add-feature",
        json!({
            "feature_brief": "Add a /status endpoint",
            "base_ref":      "main",
            "lexicon":       {}
        }),
        &config,
    )
    .await;
    // pr_url projected onto host context proves the full chain wired up
    // through every preceding cap invocation.
    let pr_url = resp
        .pointer("/context/pr_url")
        .and_then(Value::as_str)
        .unwrap_or("");
    assert!(
        !pr_url.is_empty(),
        "pr_url should be projected; got {resp:#}"
    );
}

#[tokio::test]
async fn flow_bugfix_from_error_log_walks_to_terminal() {
    let td = TempDir::new().unwrap();
    let host_path = write_host_config(&td);
    let (config, _diags) = load_resolved_with_repos(&host_path).expect("config loads");
    let _ = walk_to_terminal(
        "cognitive/flow.bugfix-from-error-log",
        json!({
            "error_log": "panicked at 'index out of bounds'",
            "base_ref":  "main"
        }),
        &config,
    )
    .await;
}

#[tokio::test]
async fn flow_safe_refactor_walks_to_terminal() {
    let td = TempDir::new().unwrap();
    let host_path = write_host_config(&td);
    let (config, _diags) = load_resolved_with_repos(&host_path).expect("config loads");
    let _ = walk_to_terminal(
        "cognitive/flow.safe-refactor",
        json!({
            "scope_description": { "paths": ["src/foo"] },
            "base_ref":           "main"
        }),
        &config,
    )
    .await;
}

#[tokio::test]
async fn flow_triage_issue_walks_to_terminal() {
    let td = TempDir::new().unwrap();
    let host_path = write_host_config(&td);
    let (config, _diags) = load_resolved_with_repos(&host_path).expect("config loads");
    let _ = walk_to_terminal(
        "cognitive/flow.triage-issue",
        json!({ "issue": { "title": "Login button broken", "body": "..." } }),
        &config,
    )
    .await;
}
