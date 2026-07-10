//! Code-first published event contract — the JSON Schema for `AuditEvent` is
//! GENERATED from the Rust struct (`praxec_core::audit::audit_event_schema`)
//! and surfaced via the `praxec schema audit-event` CLI subcommand. These
//! tests pin the contract an external consumer relies on: the schema
//! generates, and it carries the execution-tree linkage fields.

use praxec_core::audit::audit_event_schema;

#[test]
fn schema_generates_and_carries_tree_linkage_fields() {
    let schema = audit_event_schema();
    let props = schema["properties"]
        .as_object()
        .expect("generated schema has a properties object");

    // Tree linkage — the fields an observer uses to rebuild the execution
    // tree from the flat event stream.
    assert!(
        props.contains_key("parent_workflow_id"),
        "schema must carry parent_workflow_id; got: {:?}",
        props.keys().collect::<Vec<_>>()
    );
    assert!(
        props.contains_key("depth"),
        "schema must carry depth; got: {:?}",
        props.keys().collect::<Vec<_>>()
    );
    assert!(
        props.contains_key("workflow_id"),
        "schema must carry workflow_id"
    );

    // The stable discriminator + core envelope fields.
    for field in ["event_type", "id", "timestamp", "correlation_id", "payload"] {
        assert!(props.contains_key(field), "schema must carry {field}");
    }
}

#[test]
fn a_recorded_event_validates_against_its_own_schema_shape() {
    // Round-trip sanity: a serialized event's keys are all declared in the
    // generated schema (the struct is canonical; the schema derives from it,
    // so drift here means the generator lost a field).
    let event = praxec_core::audit::AuditEvent::new("workflow.started")
        .with_workflow("wf_1")
        .with_topology(Some("wf_parent".into()), 2);
    let event_json = serde_json::to_value(&event).expect("event serializes");
    let schema = audit_event_schema();
    let props = schema["properties"].as_object().expect("properties");
    for key in event_json.as_object().expect("event is an object").keys() {
        assert!(
            props.contains_key(key),
            "serialized field '{key}' missing from the generated schema"
        );
    }
}
