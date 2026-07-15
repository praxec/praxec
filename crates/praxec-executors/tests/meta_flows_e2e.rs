//! M4 acceptance — walk praxec-meta v0.1's four meta-authoring
//! flows (`meta/flow.author-capability`, `meta/flow.author-flow`,
//! `meta/flow.optimize-capability`, `meta/flow.optimize-flow`) through
//! their full lifecycle to a terminal state.
//!
//! Same fixture-executor pattern as `flows_e2e.rs` — see
//! that file's module doc for the rationale.

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

struct CapShortCircuit;
#[async_trait]
impl Executor for CapShortCircuit {
    async fn execute(&self, request: ExecuteRequest) -> Result<ExecuteResult, ExecutorError> {
        let snippet_outputs = request
            .executor_config
            .get("_snippetOutputs")
            .cloned()
            .unwrap_or_else(|| json!({}));
        Ok(ExecuteResult {
            output: synthesize_outputs(&snippet_outputs),
            evidence: vec![],
            child_workflow_id: Some("fixture-cap-instance".to_string()),
            next_transition: None,
            suspend: None,
            telemetry: None,
        })
    }
}
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
    if let Some(e) = schema.get("enum").and_then(Value::as_array) {
        if let Some(f) = e.first() {
            return f.clone();
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
    p.push("praxec-meta");
    p
}

fn write_host_config(td: &TempDir) -> PathBuf {
    // Lexicon entries for all subjects referenced by scripts, skills, and
    // capabilities in the praxec-meta fixture. Required so the pre-start
    // subject walk (SPEC §30.10.4) does not block workflow starts.
    let body = format!(
        "version: \"1.0.0\"\nrepos:\n  - path: \"{path}\"\n\
         lexicon:\n\
           # scripts\n\
           capability-harness:           {{ definition_short: \"Capability harness verification.\" }}\n\
           provider-model-inventory:     {{ definition_short: \"Provider model inventory fetch.\" }}\n\
           agents-config:                {{ definition_short: \"Agents configuration install.\" }}\n\
           mine-praxec-transitions:    {{ definition_short: \"Praxec transition mining.\" }}\n\
           praxec.check:           {{ definition_short: \"MCP praxec config check.\" }}\n\
           auth-only-smoke-test:         {{ definition_short: \"Auth-only smoke test.\" }}\n\
           # skills\n\
           emit-praxec-yaml:           {{ definition_short: \"Emit praxec YAML skill.\" }}\n\
           lexicon-extend:               {{ definition_short: \"Lexicon extension skill.\" }}\n\
           audit-mining:                 {{ definition_short: \"Audit mining research skill.\" }}\n\
           code.adversarial:             {{ definition_short: \"Adversarial code review skill.\" }}\n\
           compose-implementation:       {{ definition_short: \"Implementation composition skill.\" }}\n\
           suggest-bindings:             {{ definition_short: \"Binding suggestion skill.\" }}\n\
           tool-inventory.assemble:      {{ definition_short: \"Tool inventory assembly.\" }}\n\
           # capabilities\n\
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
    .with_writable_repo_roots(vec![praxec_core::RepoRoot::for_test()])
    .with_evidence(evidence)
}

async fn walk_to_terminal(definition_id: &str, input: Value, config: &Value) -> Value {
    let runtime = build_runtime(config).await;
    let resp = runtime
        .start(StartWorkflow {
            definition_id: definition_id.to_string(),
            input,
            principal: Principal::anonymous(),
            run_env: praxec_core::RunEnv::for_test(),
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
        "{definition_id} should walk to terminal 'done'; got state='{state}' status='{status}'. resp: {resp:#}"
    );
    assert_eq!(status, "succeeded", "{definition_id}: resp: {resp:#}");
    resp
}

#[tokio::test]
async fn meta_flow_author_capability_walks_to_terminal() {
    let td = TempDir::new().unwrap();
    let host_path = write_host_config(&td);
    let (config, _diags) = load_resolved_with_repos(&host_path).expect("config loads");
    let _ = walk_to_terminal(
        "meta/flow.author-capability",
        json!({ "goal": "Author cap.test.python-pytest", "namespace": "draft", "base_ref": "main" }),
        &config,
    )
    .await;
}

#[tokio::test]
async fn meta_flow_author_flow_walks_to_terminal() {
    let td = TempDir::new().unwrap();
    let host_path = write_host_config(&td);
    let (config, _diags) = load_resolved_with_repos(&host_path).expect("config loads");
    let _ = walk_to_terminal(
        "meta/flow.author-flow",
        json!({ "goal": "Author flow.deploy-helm-chart", "namespace": "draft", "base_ref": "main" }),
        &config,
    )
    .await;
}

#[tokio::test]
async fn meta_flow_optimize_capability_walks_to_terminal() {
    let td = TempDir::new().unwrap();
    let host_path = write_host_config(&td);
    let (config, _diags) = load_resolved_with_repos(&host_path).expect("config loads");
    let _ = walk_to_terminal(
        "meta/flow.optimize-capability",
        json!({ "target_definition_id": "cognitive/cap.implement.tdd-loop", "base_ref": "main" }),
        &config,
    )
    .await;
}

#[tokio::test]
async fn meta_flow_optimize_flow_walks_to_terminal() {
    let td = TempDir::new().unwrap();
    let host_path = write_host_config(&td);
    let (config, _diags) = load_resolved_with_repos(&host_path).expect("config loads");
    let _ = walk_to_terminal(
        "meta/flow.optimize-flow",
        json!({ "target_definition_id": "cognitive/flow.add-feature", "base_ref": "main" }),
        &config,
    )
    .await;
}

/// PR2 — `flow.configure-models` walks the full
/// inventory → plan → gate → write → smoke chain in `mode=auto` so
/// the deterministic auto_approve branch fires (no human gate
/// blocking). Each cap is short-circuited by `CapShortCircuit` via
/// its snippet outputs (FixtureRegistry pattern).
#[tokio::test]
async fn meta_flow_configure_models_walks_to_terminal_in_auto_mode() {
    let td = TempDir::new().unwrap();
    let host_path = write_host_config(&td);
    let (config, _diags) = load_resolved_with_repos(&host_path).expect("config loads");
    let _ = walk_to_terminal(
        "meta/flow.configure-models",
        json!({
            "providers": "anthropic,openai",
            "delegates": ["coding-frontier", "prose-standard"],
            "mode":      "auto",
            "target_path": ".praxec/models.yaml",
        }),
        &config,
    )
    .await;
}
