//! SPEC §8.4 — end-to-end of the reference EDIT authoring workflow
//! (`examples/authoring-edit-workflow.yaml`). Drives
//! `editing → diffing → reviewing_structure → reviewed_structure → validating
//! → ready` and asserts the `diff` step renders the change onto the blackboard
//! and the edited candidate reaches `ready`. The publish-side hash-guard
//! (CONFLICT_STALE) is covered by the store + executor unit tests.

use std::path::PathBuf;
use std::sync::Arc;

use praxec_core::WorkflowRuntime;
use praxec_core::audit::{AuditSink, MemoryAuditSink};
use praxec_core::config;
use praxec_core::guards::DefaultGuardEvaluator;
use praxec_core::ports::{DefinitionStore, ExecutorRegistry, GuidanceAcknowledgmentStore};
use praxec_core::store::{
    ConfigDefinitionStore, InMemoryGuidanceAcknowledgmentStore, InMemoryWorkflowStore,
};
use praxec_executors::{
    DryRunExecutor, HashMapExecutorRegistry, NoopExecutor, StructuralAnalysisExecutor,
    diff::DiffExecutor,
};
use praxec_mcp_server::{PraxecServer, TOOL_COMMAND, TOOL_QUERY};
use rmcp::model::{CallToolRequestParams, JsonObject};
use serde_json::{Value, json};

fn edit_workflow_yaml() -> Value {
    let mut path = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    path.pop();
    path.pop();
    path.push("examples");
    path.push("authoring-edit-workflow.yaml");
    let raw = std::fs::read_to_string(&path).expect("authoring-edit-workflow.yaml exists");
    serde_yaml::from_str(&raw).expect("yaml parses")
}

fn build_server() -> PraxecServer {
    let cfg = edit_workflow_yaml();
    let resolved = config::resolve(cfg).expect("edit workflow resolves");

    let audit = Arc::new(MemoryAuditSink::new());
    let ack: Arc<dyn GuidanceAcknowledgmentStore> =
        Arc::new(InMemoryGuidanceAcknowledgmentStore::new());

    let registry = HashMapExecutorRegistry::new()
        .with("noop", Arc::new(NoopExecutor))
        .with("diff", Arc::new(DiffExecutor))
        .with("structural_analysis", Arc::new(StructuralAnalysisExecutor))
        .with("dry_run", Arc::new(DryRunExecutor));
    let executors: Arc<dyn ExecutorRegistry> = Arc::new(registry);

    let definitions: Arc<dyn DefinitionStore> =
        Arc::new(ConfigDefinitionStore::from_config(&resolved));
    let guards = Arc::new(DefaultGuardEvaluator::new().with_ack_store(ack.clone()));
    let store = Arc::new(InMemoryWorkflowStore::new());
    let runtime = WorkflowRuntime::new(
        definitions,
        store,
        executors,
        guards,
        audit.clone() as Arc<dyn AuditSink>,
    );
    PraxecServer::new(runtime).with_ack_store(ack)
}

fn args(name: &'static str, args: Value) -> CallToolRequestParams {
    let m: JsonObject = args.as_object().cloned().expect("object");
    CallToolRequestParams::new(name).with_arguments(m)
}

#[tokio::test]
async fn edit_flow_diffs_and_reaches_ready() {
    let server = build_server();
    let start = server
        .dispatch_call(args(
            TOOL_COMMAND,
            json!({ "definitionId": "authoring-edit", "input": {} }),
        ))
        .await
        .expect("start");
    assert_eq!(start["workflow"]["state"].as_str(), Some("editing"));
    let workflow_id = start["workflow"]["id"].as_str().unwrap().to_string();
    let version = start["workflow"]["version"].as_u64().unwrap();

    // The definition currently on record, and a proposed edit of it.
    let base = json!({
        "initialState": "s",
        "states": { "s": { "transitions": { "go": { "target": "done" } } }, "done": { "terminal": true } }
    });
    let edited = json!({
        "initialState": "s",
        "states": { "s": { "transitions": { "go": { "target": "finished" } } }, "finished": { "terminal": true } }
    });

    let resp = server
        .dispatch_call(args(
            TOOL_COMMAND,
            json!({
                "workflowId":      workflow_id,
                "expectedVersion": version,
                "transition":      "propose_edit",
                "arguments": {
                    "definitionId":   "acme/flow.thing",
                    "definition":     &edited,
                    "baseDefinition": &base,
                },
            }),
        ))
        .await
        .expect("propose_edit");

    // The deterministic chain runs diff → structural_analysis → (clean) →
    // dry_run, landing on `ready`. The diff is on the blackboard.
    assert_eq!(resp["workflow"]["state"].as_str(), Some("ready"));
    let diff = resp["context"]["edit_diff"].as_str().unwrap_or_default();
    assert!(
        diff.contains("- ") && diff.contains("+ "),
        "diff shows the change: {diff}"
    );
    assert!(
        diff.contains("done") || diff.contains("finished"),
        "names the changed target: {diff}"
    );
    assert_eq!(resp["context"]["structural_issues_count"].as_i64(), Some(0));
}

#[tokio::test]
async fn read_definition_returns_body_and_hash_for_editing() {
    let server = build_server();
    // `praxec.query { definitionId }` reads the current body + its content
    // hash — the basis an author feeds back as `baseDefinition`.
    let resp = server
        .dispatch_call(args(
            TOOL_QUERY,
            json!({ "definitionId": "authoring-edit" }),
        ))
        .await
        .expect("read definition");
    assert_eq!(resp["definitionId"].as_str(), Some("authoring-edit"));
    assert_eq!(resp["definition"]["initialState"].as_str(), Some("editing"));
    let hash = resp["hash"].as_str().expect("hash present");
    assert!(hash.starts_with("sha256:"), "content hash: {hash}");
    // The hash matches the canonical hash of the returned body.
    let recomputed = praxec_core::config::compute_definition_hash(&resp["definition"]);
    assert_eq!(hash, recomputed);

    // An unknown id is a clean DEFINITION_NOT_FOUND, not a panic.
    let missing = server
        .dispatch_call(args(
            TOOL_QUERY,
            json!({ "definitionId": "nope/cap.ghost" }),
        ))
        .await;
    assert!(missing.is_err() || missing.unwrap().get("error").is_some());
}
