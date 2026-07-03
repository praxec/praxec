//! Phase 2 (intent-driven loom) — a flow's `process`/`taskClass` tag is threaded
//! into the discovery index so the catalog is filterable by task-class (via the
//! existing `tags` search) and the Phase-3 selector can read it back.

use praxec_core::discovery::PROCESS_TAG_PREFIX;
use praxec_core::discovery::discovery_indexer::index_from_config;
use serde_json::json;

fn ship_flow(process: Option<&str>) -> serde_json::Value {
    let mut def = json!({
        "inputs": {},
        "initialState": "s0",
        "outcomes": [{ "id": "ok", "statement": "done", "check": "$.context.x == true" }],
        "states": {
            "s0": { "transitions": { "go": { "target": "end", "executor": { "kind": "noop" } } } },
            "end": { "terminal": true, "outcome": "success" }
        }
    });
    if let Some(p) = process {
        def["process"] = json!(p);
    }
    json!({ "workflows": { "flow.ship": def } })
}

#[test]
fn a_process_tagged_flow_is_discoverable_by_task_class() {
    let items = index_from_config(&ship_flow(Some("engineering"))).expect("indexes");
    let flow = items
        .iter()
        .find(|i| i.id == "flow.ship")
        .expect("flow indexed");
    assert_eq!(flow.task_class(), Some("engineering"));
    assert!(
        flow.tags
            .iter()
            .any(|t| t == &format!("{PROCESS_TAG_PREFIX}engineering")),
        "process tag present for catalog filtering: {:?}",
        flow.tags
    );
    assert!(
        flow.text.contains("engineering"),
        "task-class token folded into searchable text: {:?}",
        flow.text
    );
}

#[test]
fn an_untagged_flow_has_no_task_class() {
    let items = index_from_config(&ship_flow(None)).expect("indexes");
    let flow = items
        .iter()
        .find(|i| i.id == "flow.ship")
        .expect("flow indexed");
    assert_eq!(flow.task_class(), None);
}
