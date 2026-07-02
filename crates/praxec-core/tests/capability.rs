//! Tests for the INIT-3 capability registry.

use praxec_core::capability::{Capability, CapabilityRegistry, CapabilitySource};
use praxec_core::proxy_workflow::compile_proxy_workflow_from_registry;
use serde_json::json;

#[test]
fn from_defined_classifies_executor_kinds() {
    let cfg = json!({
        "version": "1.0.0",
        "proxy": {
            "expose": [
                {
                    "name": "github.list_issues",
                    "executor": { "kind": "mcp", "connection": "github", "tool": "list_issues" }
                },
                {
                    "name": "dotnet.test",
                    "executor": { "kind": "cli", "connection": "dotnet" }
                },
                {
                    "name": "noop_thing",
                    "executor": { "kind": "noop" }
                }
            ]
        }
    });

    let reg = CapabilityRegistry::from_defined(&cfg);
    assert_eq!(reg.len(), 3);

    let by_id = |id: &str| reg.iter().find(|c| c.id == id).unwrap().source.clone();

    assert_eq!(by_id("github.list_issues"), CapabilitySource::Defined);
    assert_eq!(by_id("dotnet.test"), CapabilitySource::Cli);
    assert_eq!(by_id("noop_thing"), CapabilitySource::Defined);
}

#[test]
fn imported_capabilities_compile_into_proxy_workflow() {
    let mut reg = CapabilityRegistry::new();

    // Two imports from different "vendors" — what matters is they're MCP
    // tools accessible through a connection name. The fact that one might be
    // a `npx` shim and another a `docker run` is invisible at this layer.
    reg.add(Capability {
        id: "tools.echo".into(),
        source: CapabilitySource::Imported {
            connection: "external_a".into(),
            tool: "echo".into(),
        },
        title: "Echo".into(),
        description: Some("Echoes a string.".into()),
        input_schema: Some(json!({
            "type": "object",
            "properties": { "text": { "type": "string" } },
            "required": ["text"]
        })),
        executor: json!({
            "kind": "mcp",
            "connection": "external_a",
            "tool": "echo",
        }),
        tags: vec!["external".into()],
    });
    reg.add(Capability {
        id: "container.fetch".into(),
        source: CapabilitySource::Imported {
            connection: "containerized_b".into(),
            tool: "fetch".into(),
        },
        title: "Fetch".into(),
        description: None,
        input_schema: None,
        executor: json!({
            "kind": "mcp",
            "connection": "containerized_b",
            "tool": "fetch",
        }),
        tags: vec![],
    });

    let workflow = compile_proxy_workflow_from_registry(&reg).expect("workflow");
    let transitions = workflow
        .pointer("/states/ready/transitions")
        .and_then(|v| v.as_object())
        .unwrap();

    assert!(transitions.contains_key("tools.echo"));
    assert!(transitions.contains_key("container.fetch"));

    // Each transition routes back to the "ready" state — the null-op proxy pattern.
    for (_, t) in transitions {
        assert_eq!(t["target"], "ready");
        // Imported capabilities preserve their executor config so the runtime
        // can dispatch through the registered `mcp` executor.
        assert_eq!(t["executor"]["kind"], "mcp");
    }
}

#[test]
fn empty_registry_yields_no_workflow() {
    let reg = CapabilityRegistry::new();
    assert!(compile_proxy_workflow_from_registry(&reg).is_none());
}

#[test]
fn capability_source_token_is_stable() {
    assert_eq!(CapabilitySource::Defined.token(), "defined");
    assert_eq!(CapabilitySource::Cli.token(), "cli");
    assert_eq!(CapabilitySource::Rest.token(), "rest");
    assert_eq!(
        CapabilitySource::Imported {
            connection: "x".into(),
            tool: "y".into()
        }
        .token(),
        "imported"
    );
}
