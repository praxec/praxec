//! Evidence loop, last hop — `praxec.query { query }` workflow search hits
//! carry the intent index's `(task_class, template)` track record so a model
//! picking a workflow chooses by evidence, not blind.
//!
//! Pins the three contract points:
//! - (a) a workflow with ≥ `intent.min_runs` recorded outcomes shows
//!   `evidence: {runs, success_rate, mean_cost_usd}` on its search hit;
//! - (b) below the threshold, `evidence` is omitted entirely (a thin sample is
//!   no evidence, not noisy evidence);
//! - (c) no recorded outcomes / a non-file sink (events not stored) leaves the
//!   search fully working with `evidence` omitted — missing evidence is the
//!   normal fresh-system state, never an error.

use std::sync::Arc;

use praxec_core::WorkflowRuntime;
use praxec_core::audit::{AuditEvent, AuditSink, FileAuditSink, MemoryAuditSink, RotationInterval};
use praxec_core::cost_report::AGENT_COMPLETED;
use praxec_core::discovery::{
    DiscoveryItem, DiscoveryKind, InMemoryDiscoveryIndex, PROCESS_TAG_PREFIX,
};
use praxec_core::guards::DefaultGuardEvaluator;
use praxec_core::intent_index::{IntentParams, OUTCOME_RECORDED, outcome_recorded_payload};
use praxec_core::ports::ExecutorRegistry;
use praxec_core::store::{ConfigDefinitionStore, InMemoryWorkflowStore};
use praxec_mcp_server::PraxecServer;
use rmcp::model::{CallToolRequestParams, JsonObject};
use serde_json::{Value, json};

struct NoopRegistry;
impl ExecutorRegistry for NoopRegistry {
    fn get(&self, _kind: &str) -> Option<Arc<dyn praxec_core::Executor>> {
        None
    }
}

fn workflow_item(id: &str, task_class: Option<&str>) -> DiscoveryItem {
    let mut tags = Vec::new();
    if let Some(tc) = task_class {
        tags.push(format!("{PROCESS_TAG_PREFIX}{tc}"));
    }
    DiscoveryItem {
        id: id.into(),
        kind: DiscoveryKind::Workflow,
        title: id.into(),
        description: String::new(),
        tags,
        examples: vec![],
        aliases: vec![],
        text: String::new(),
        links: vec![],
        verb: None,
        body: None,
        source: None,
    }
}

/// Minimal server over the given audit sink + discovery items (mirrors
/// observe_query.rs).
fn server_with(audit: Arc<dyn AuditSink>, items: Vec<DiscoveryItem>) -> PraxecServer {
    let cfg = json!({
        "version": "1.0.0",
        "workflows": {
            "test_wf": {
                "initialState": "open",
                "states": {
                    "open": { "transitions": { "close": { "target": "done" } } },
                    "done": { "terminal": true }
                }
            }
        }
    });
    let resolved = praxec_core::config::resolve(cfg).expect("resolve");
    let defs = Arc::new(ConfigDefinitionStore::from_config(&resolved));
    let store = Arc::new(InMemoryWorkflowStore::new());
    let guards = Arc::new(DefaultGuardEvaluator::new());
    let runtime = WorkflowRuntime::new(defs, store, Arc::new(NoopRegistry), guards, audit);
    PraxecServer::new(runtime).with_discovery(Arc::new(InMemoryDiscoveryIndex::new(items)))
}

async fn search(server: &PraxecServer, query: &str) -> Value {
    let m: JsonObject = json!({ "query": query })
        .as_object()
        .cloned()
        .unwrap_or_default();
    server
        .dispatch_call(CallToolRequestParams::new("praxec.query").with_arguments(m))
        .await
        .expect("dispatch ok")
}

fn hit<'a>(resp: &'a Value, id: &str) -> &'a Value {
    resp["items"]
        .as_array()
        .expect("items array")
        .iter()
        .find(|h| h["item"]["id"] == id)
        .unwrap_or_else(|| panic!("hit '{id}' present; got: {resp}"))
}

/// Record one terminated mission (`outcome.recorded`) for `template`, plus a
/// priced `agent.completed` step so the mission joins a realized cost.
async fn record_mission(sink: &FileAuditSink, wf: &str, template: &str, met: bool, cost: f64) {
    let status = if met { "succeeded" } else { "failed" };
    sink.record(
        AuditEvent::new(OUTCOME_RECORDED)
            .with_workflow(wf)
            .with_payload(outcome_recorded_payload(
                template,
                Some("engineering"),
                met,
                1,
                status,
                None,
            )),
    )
    .await
    .expect("record outcome");
    sink.record(
        AuditEvent::new(AGENT_COMPLETED)
            .with_workflow(wf)
            .with_payload(json!({
                "model": "openrouter:z-ai/glm-5.2",
                "prompt_tokens": 1000,
                "completion_tokens": 200,
                "cost_usd": cost,
            })),
    )
    .await
    .expect("record cost");
}

/// (a) + (b) — at/above the tuning `intent.min_runs` threshold the hit carries
/// `evidence: {runs, success_rate, mean_cost_usd}`; below it the key is absent.
#[tokio::test]
async fn search_hits_carry_evidence_at_threshold_and_omit_below() {
    let min_runs = IntentParams::from_tuning().min_runs.max(1);
    let dir = tempfile::tempdir().expect("tempdir");
    let sink = Arc::new(FileAuditSink::new(dir.path(), RotationInterval::Daily));

    // flow.proven: exactly min_runs missions, one failed → rate (n-1)/n when
    // n > 1 (rate 1.0 for the degenerate min_runs == 1), each priced 0.01.
    for i in 0..min_runs {
        let met = min_runs == 1 || i > 0;
        record_mission(&sink, &format!("wf_proven_{i}"), "flow.proven", met, 0.01).await;
    }
    // flow.thin: one fewer mission than the trust bar.
    for i in 0..min_runs - 1 {
        record_mission(&sink, &format!("wf_thin_{i}"), "flow.thin", true, 0.01).await;
    }

    let server = server_with(
        sink,
        vec![
            workflow_item("flow.proven", Some("engineering")),
            workflow_item("flow.thin", Some("engineering")),
        ],
    );
    let resp = search(&server, "flow").await;

    let proven = hit(&resp, "flow.proven");
    let ev = &proven["evidence"];
    assert!(ev.is_object(), "evidence attached at min_runs; got: {resp}");
    assert_eq!(ev["runs"], min_runs, "got: {ev}");
    let expected_rate = if min_runs == 1 {
        1.0
    } else {
        (min_runs - 1) as f64 / min_runs as f64
    };
    let rate = ev["success_rate"].as_f64().expect("success_rate");
    assert!((rate - expected_rate).abs() < 1e-9, "got: {ev}");
    let mean = ev["mean_cost_usd"].as_f64().expect("mean_cost_usd");
    assert!((mean - 0.01).abs() < 1e-9, "got: {ev}");

    let thin = hit(&resp, "flow.thin");
    assert!(
        thin.get("evidence").is_none(),
        "a thin sample is omitted, not shown as noise; got: {thin}"
    );
}

/// (c) — no recorded outcomes (empty file sink) and a non-file sink (events
/// not stored at all): search works, evidence is simply absent, no error.
#[tokio::test]
async fn search_without_history_or_store_omits_evidence_and_still_works() {
    let items = || vec![workflow_item("flow.fresh", Some("engineering"))];

    // Fresh system: file sink, zero events.
    let dir = tempfile::tempdir().expect("tempdir");
    let empty = Arc::new(FileAuditSink::new(dir.path(), RotationInterval::Daily));
    let resp = search(&server_with(empty, items()), "flow").await;
    assert!(
        hit(&resp, "flow.fresh").get("evidence").is_none(),
        "no history ⇒ no evidence; got: {resp}"
    );

    // Non-file sink: events are not stored — degrade to an unannotated search,
    // NOT the observe-style fail-fast (evidence is an annotation, not a read
    // the caller asked for).
    let resp = search(
        &server_with(Arc::new(MemoryAuditSink::new()), items()),
        "flow",
    )
    .await;
    let h = hit(&resp, "flow.fresh");
    assert!(h.get("evidence").is_none(), "got: {resp}");
    assert!(resp.get("error").is_none(), "search must not fail: {resp}");
}

/// Evidence keys on `(task_class, template)` — history under another
/// task-class, or an untagged workflow, never borrows evidence.
#[tokio::test]
async fn evidence_requires_the_matching_task_class() {
    let min_runs = IntentParams::from_tuning().min_runs.max(1);
    let dir = tempfile::tempdir().expect("tempdir");
    let sink = Arc::new(FileAuditSink::new(dir.path(), RotationInterval::Daily));
    for i in 0..min_runs {
        // History recorded under task_class "engineering"...
        record_mission(&sink, &format!("wf_{i}"), "flow.mismatch", true, 0.01).await;
    }
    let server = server_with(
        sink,
        vec![
            // ...but this catalog entry declares a different class,
            workflow_item("flow.mismatch", Some("research")),
            // and this one declares none.
            workflow_item("flow.untagged", None),
        ],
    );
    let resp = search(&server, "flow").await;
    assert!(
        hit(&resp, "flow.mismatch").get("evidence").is_none(),
        "other-class history is not this class's evidence; got: {resp}"
    );
    assert!(
        hit(&resp, "flow.untagged").get("evidence").is_none(),
        "an unclassified workflow has no evidence key; got: {resp}"
    );
}
