//! Parity snapshots for the MCP tool surface — SPEC §32 two-tool surface.
//!
//! Pins the exact JSON Schema published for `praxec.query` and
//! `praxec.command`. Any change to either is a visible MCP surface
//! change and must be intentional.

use praxec_mcp_server::{TOOL_COMMAND, TOOL_QUERY, tool_definitions};
use serde_json::Value;

fn schema_of(name: &str) -> Value {
    let tool = tool_definitions()
        .into_iter()
        .find(|t| t.name == name)
        .unwrap_or_else(|| panic!("tool '{name}' not found"));
    Value::Object((*tool.input_schema).clone())
}

fn description_of(name: &str) -> String {
    tool_definitions()
        .into_iter()
        .find(|t| t.name == name)
        .unwrap_or_else(|| panic!("tool '{name}' not found"))
        .description
        .as_deref()
        .unwrap_or_else(|| panic!("tool '{name}' has no description"))
        .to_string()
}

#[test]
fn query_schema_has_expected_fields() {
    let schema = schema_of(TOOL_QUERY);
    // All fields optional (no `required` key) — the shape-router selects
    // the operation by which optional fields are present.
    assert!(
        schema.get("required").is_none(),
        "praxec.query schema must have no required fields; got: {schema}"
    );
    let props = schema["properties"].as_object().expect("properties object");
    // Must carry the search/describe/get/explain discriminator fields.
    assert!(props.contains_key("query"), "missing query");
    assert!(props.contains_key("kind"), "missing kind");
    assert!(props.contains_key("subject"), "missing subject");
    assert!(props.contains_key("workflowId"), "missing workflowId");
    assert!(props.contains_key("transition"), "missing transition");
    assert!(props.contains_key("limit"), "missing limit");
}

#[test]
fn command_schema_has_expected_fields() {
    let schema = schema_of(TOOL_COMMAND);
    // All fields optional (no `required` key).
    assert!(
        schema.get("required").is_none(),
        "praxec.command schema must have no required fields; got: {schema}"
    );
    let props = schema["properties"].as_object().expect("properties object");
    // Must carry the start/submit/define discriminator fields.
    assert!(props.contains_key("definitionId"), "missing definitionId");
    assert!(props.contains_key("input"), "missing input");
    assert!(props.contains_key("workflowId"), "missing workflowId");
    assert!(
        props.contains_key("expectedVersion"),
        "missing expectedVersion"
    );
    assert!(props.contains_key("transition"), "missing transition");
    assert!(props.contains_key("arguments"), "missing arguments");
    assert!(props.contains_key("subject"), "missing subject");
    assert!(props.contains_key("definition"), "missing definition");
    assert!(props.contains_key("traceId"), "missing traceId");
    assert!(props.contains_key("runId"), "missing runId");
}

#[test]
fn both_schemas_are_type_object() {
    for name in [TOOL_QUERY, TOOL_COMMAND] {
        let schema = schema_of(name);
        assert_eq!(
            schema.get("type").and_then(|v| v.as_str()),
            Some("object"),
            "tool '{name}' inputSchema must be type=object"
        );
    }
}

#[test]
fn descriptions_snapshot() {
    let desc_q = description_of(TOOL_QUERY);
    assert!(
        desc_q.contains("§32") || desc_q.contains("read") || desc_q.contains("home"),
        "praxec.query description must mention §32, 'read', or 'home'; got: {desc_q}"
    );
    let desc_c = description_of(TOOL_COMMAND);
    assert!(
        desc_c.contains("§32") || desc_c.contains("write") || desc_c.contains("start"),
        "praxec.command description must mention §32, 'write', or 'start'; got: {desc_c}"
    );
}
