//! D4b registry-v3 — load / validate behavior.
//!
//! Covers: v2 back-compat (the ADR-0013 catalog loads unchanged), the v3
//! reference fixture (one mcp + one cli + one rest tool, each carrying a D1
//! descriptor, plus a crossmatrix), crossmatrix round-trip, and every
//! typed `validate()` failure path (unknown schema marker, v3 surface
//! smuggled under a v2 marker, kind/coordinate conflicts, dangling
//! crossmatrix edges, duplicate ids, unknown provider keys).

use serde_json::{Value, json};

use praxec_core::registry_v3::{
    CrossmatrixRole, CrossmatrixRow, Pack, Registry, RegistryError, RegistrySchema, RegistryTool,
};
use praxec_core::tool_descriptor::ToolKind;

// ── fixtures ──────────────────────────────────────────────────────────────

/// The current `praxec/packs` shape — ADR-0013 `praxec.packs/v2`, mcp-only.
const V2_REGISTRY: &str = r#"
schema: praxec.packs/v2
packs:
  - id: cognitive-architectures
    name: Cognitive Architectures
    namespace: cognitive
    description: Reasoning and inspection workflow suites.
    repo: https://github.com/praxec/cognitive-architectures
    tier: open
    tags: [reasoning, inspection]
    requires: [cpm-planner]
    external: [github-mcp]
  - id: cognitive-architectures-max
    name: Cognitive Architectures Max
    namespace: cognitive-max
    tier: premium
    extends: cognitive-architectures
tools:
  - id: cpm-planner
    name: CPM Planner
    description: Critical-path planning MCP server.
    repo: https://github.com/praxec/cpm-planner
    command: cpm-planner
    version: 0.0.1
    mcp_registry_id: io.github.praxec/cpm-planner
    providers:
      docker: ghcr.io/praxec/cpm-planner
      release: https://github.com/praxec/cpm-planner/releases
      cargo: cpm-planner
"#;

/// The `praxec.packs/v3` REFERENCE fixture — the shape `praxec/packs` will
/// mirror. v2 fields as-is; each tool MAY carry a D1 descriptor (this one
/// demonstrates all three kinds: mcp, cli, rest) + `suggested_workflows`;
/// the top-level crossmatrix indexes tool x workflow with a derived role.
const V3_REGISTRY: &str = r#"
schema: praxec.packs/v3
packs:
  - id: cognitive-architectures
    name: Cognitive Architectures
    namespace: cognitive
    description: Reasoning and inspection workflow suites.
    repo: https://github.com/praxec/cognitive-architectures
    tier: open
    tags: [reasoning, inspection]
    requires: [cpm-planner, ripgrep, httpbin]
tools:
  - id: cpm-planner
    name: CPM Planner
    description: Critical-path planning MCP server.
    repo: https://github.com/praxec/cpm-planner
    command: cpm-planner
    version: 0.0.1
    mcp_registry_id: io.github.praxec/cpm-planner
    providers:
      docker: ghcr.io/praxec/cpm-planner
      cargo: cpm-planner
    suggested_workflows: [cognitive/flow.plan-critical-path]
    descriptor:
      schema_version: praxec.tool/v1
      name: cpm-planner
      version: 0.0.1
      kind: mcp
      reach:
        connection_name: cpm-planner
        grant_as: cpm-planner
        connection:
          kind: mcp
          command: cpm-planner
          args: []
          env: {}
      operations:
        - id: plan
          verb: run
          input_schema: { type: object }
          output_schema: { type: object }
          mcp_tool: plan
      suggested_workflows: [cognitive/flow.derisk]
  - id: ripgrep
    name: ripgrep
    description: Fast line-oriented search.
    command: rg
    version: 14.1.0
    providers:
      cargo: ripgrep
    descriptor:
      schema_version: praxec.tool/v1
      name: ripgrep
      version: 14.1.0
      kind: cli
      reach:
        connection_name: rg
        grant_as: rg
        connection:
          kind: cli
          command: rg
          workingDirectory: "."
      operations:
        - id: grep
          verb: search
          input_schema: { type: object }
          output_schema: { type: object }
          cli: { args: ["--json"] }
      suggested_workflows: [cognitive/flow.inspect-repo]
  - id: httpbin
    name: httpbin
    description: HTTP request inspection API.
    version: 0.1.0
    descriptor:
      schema_version: praxec.tool/v1
      name: httpbin
      version: 0.1.0
      kind: rest
      reach:
        connection_name: httpbin
        grant_as: httpbin
        connection:
          kind: rest
          baseUrl: https://httpbin.org
          headers: {}
        auth:
          scheme: header
          headers: [X-Api-Key]
      operations:
        - id: get-anything
          verb: fetch
          input_schema: { type: object }
          output_schema: { type: object }
          rest: { method: GET, path: /anything }
    suggested_workflows: [cognitive/flow.probe-api]
crossmatrix:
  - { tool: cpm-planner, workflow: cognitive/flow.plan-critical-path, role: suggested }
  - { tool: cpm-planner, workflow: cognitive/flow.derisk, role: suggested }
  - { tool: ripgrep, workflow: cognitive/flow.inspect-repo, role: dependency }
  - { tool: httpbin, workflow: cognitive/flow.probe-api, role: suggested }
"#;

fn v3_value() -> Value {
    serde_yaml::from_str(V3_REGISTRY).expect("v3 fixture parses as YAML")
}

// ── v2 back-compat ────────────────────────────────────────────────────────

#[test]
fn v2_registry_loads_with_v3_fields_absent() {
    let registry = Registry::load_str(V2_REGISTRY).expect("v2 registry loads");
    assert_eq!(registry.schema, RegistrySchema::V2);
    assert_eq!(registry.packs.len(), 2);
    assert_eq!(registry.tools.len(), 1);

    let pack = &registry.packs[0];
    assert_eq!(pack.namespace, "cognitive");
    assert_eq!(pack.requires, vec!["cpm-planner"]);
    assert_eq!(pack.external, vec!["github-mcp"]);
    assert_eq!(
        registry.packs[1].extends.as_deref(),
        Some("cognitive-architectures")
    );

    let tool = registry.tool("cpm-planner").expect("tool by id");
    assert_eq!(
        tool.mcp_registry_id.as_deref(),
        Some("io.github.praxec/cpm-planner")
    );
    assert_eq!(tool.providers.len(), 3);
    assert!(tool.descriptor.is_none(), "v2 carries no descriptors");
    assert!(tool.suggested_workflows.is_empty());
    assert!(registry.crossmatrix().is_empty());
}

// ── v3 reference fixture: all three kinds ─────────────────────────────────

#[test]
fn v3_registry_loads_with_mcp_cli_and_rest_descriptors() {
    let registry = Registry::load_str(V3_REGISTRY).expect("v3 registry loads");
    assert_eq!(registry.schema, RegistrySchema::V3);
    assert_eq!(registry.tools.len(), 3);

    let kind_of = |id: &str| {
        registry
            .tool(id)
            .and_then(|t| t.descriptor.as_ref())
            .map(|d| d.kind)
            .unwrap_or_else(|| panic!("tool `{id}` must carry a descriptor"))
    };
    assert_eq!(kind_of("cpm-planner"), ToolKind::Mcp);
    assert_eq!(kind_of("ripgrep"), ToolKind::Cli);
    assert_eq!(kind_of("httpbin"), ToolKind::Rest);

    // The descriptor is the D1 type: grant declaration surfaces verbatim.
    let rg = registry.tool("ripgrep").expect("ripgrep");
    let descriptor = rg.descriptor.as_ref().expect("descriptor");
    assert_eq!(descriptor.grant_token(), "rg");

    // Registry-level + descriptor-level suggested workflows union.
    assert_eq!(
        registry
            .tool("cpm-planner")
            .expect("cpm-planner")
            .effective_suggested_workflows(),
        vec!["cognitive/flow.plan-critical-path", "cognitive/flow.derisk"],
    );
}

#[test]
fn v3_crossmatrix_reads_both_directions() {
    let registry = Registry::load_str(V3_REGISTRY).expect("v3 registry loads");
    assert_eq!(registry.crossmatrix().len(), 4);
    assert_eq!(registry.crossmatrix()[2].role, CrossmatrixRole::Dependency);

    assert_eq!(
        registry.workflows_for_tool("cpm-planner"),
        vec!["cognitive/flow.plan-critical-path", "cognitive/flow.derisk"],
    );
    let tools = registry.tools_for_workflow("cognitive/flow.inspect-repo");
    assert_eq!(tools.len(), 1);
    assert_eq!(tools[0].id, "ripgrep");
    assert!(registry.workflows_for_tool("no-such-tool").is_empty());
    assert!(registry.tools_for_workflow("no/flow.such").is_empty());
}

#[test]
fn v3_registry_round_trips_through_serialization() {
    let registry = Registry::load_str(V3_REGISTRY).expect("v3 registry loads");
    let serialized = serde_json::to_value(&registry).expect("registry serializes");
    let reloaded = Registry::load_value(serialized).expect("serialized registry reloads");
    assert_eq!(
        registry, reloaded,
        "load -> serialize -> load must be identity"
    );
}

// ── failure paths ─────────────────────────────────────────────────────────

#[test]
fn unknown_schema_marker_fails_typed() {
    for (label, doc) in [
        ("bad marker", "schema: praxec.packs/v9\ntools: []\n"),
        ("missing marker", "tools: []\n"),
    ] {
        let err = Registry::load_str(doc).expect_err(label);
        assert!(
            matches!(err, RegistryError::UnknownSchema { .. }),
            "{label}: expected UnknownSchema, got {err}"
        );
    }
}

#[test]
fn v3_surface_under_v2_marker_fails_typed() {
    let mut with_crossmatrix = v3_value();
    with_crossmatrix["schema"] = json!("praxec.packs/v2");
    let err = Registry::load_value(with_crossmatrix).expect_err("crossmatrix under v2");
    assert!(
        matches!(&err, RegistryError::V3FieldUnderV2 { site } if site == "crossmatrix"),
        "expected V3FieldUnderV2(crossmatrix), got {err}"
    );

    let mut with_descriptor = v3_value();
    with_descriptor["schema"] = json!("praxec.packs/v2");
    with_descriptor
        .as_object_mut()
        .expect("registry is an object")
        .remove("crossmatrix");
    let err = Registry::load_value(with_descriptor).expect_err("descriptor under v2");
    assert!(
        matches!(&err, RegistryError::V3FieldUnderV2 { site } if site == "tools[cpm-planner].descriptor"),
        "expected V3FieldUnderV2(tools[cpm-planner].descriptor), got {err}"
    );
}

#[test]
fn descriptor_kind_conflicting_with_declared_coords_fails_typed() {
    // A rest descriptor on a tool that declares a spawnable `command`.
    let mut doc = v3_value();
    doc["tools"][2]["command"] = json!("httpbin");
    let err = Registry::load_value(doc).expect_err("rest descriptor + command");
    assert!(
        matches!(
            &err,
            RegistryError::ToolKindConflict { tool, kind: "rest", coordinate: "command" }
                if tool == "httpbin"
        ),
        "expected ToolKindConflict(httpbin/rest/command), got {err}"
    );

    // A cli descriptor on a tool that declares an MCP registry identity.
    let mut doc = v3_value();
    doc["tools"][1]["mcp_registry_id"] = json!("io.github.praxec/ripgrep");
    let err = Registry::load_value(doc).expect_err("cli descriptor + mcp_registry_id");
    assert!(
        matches!(
            &err,
            RegistryError::ToolKindConflict { tool, kind: "cli", coordinate: "mcp_registry_id" }
                if tool == "ripgrep"
        ),
        "expected ToolKindConflict(ripgrep/cli/mcp_registry_id), got {err}"
    );
}

#[test]
fn descriptor_failing_its_own_cross_field_validation_fails_typed() {
    // FM2 inside the descriptor: kind says cli, reach.connection says mcp.
    // The registry schema cannot see this (it is cross-field), so it must
    // surface through Registry::validate as a wrapped descriptor error.
    let mut doc = v3_value();
    doc["tools"][1]["descriptor"]["reach"]["connection"] =
        json!({ "kind": "mcp", "command": "rg", "args": [], "env": {} });
    let err = Registry::load_value(doc).expect_err("descriptor kind mismatch");
    assert!(
        matches!(&err, RegistryError::Descriptor { tool, .. } if tool == "ripgrep"),
        "expected Descriptor(ripgrep), got {err}"
    );
    assert!(
        err.to_string().contains("TOOL_KIND_MISMATCH"),
        "must carry the descriptor's own typed code: {err}"
    );
}

#[test]
fn crossmatrix_row_referencing_unknown_tool_fails_typed() {
    let mut doc = v3_value();
    doc["crossmatrix"]
        .as_array_mut()
        .expect("crossmatrix is an array")
        .push(json!({ "tool": "ghost", "workflow": "cognitive/flow.derisk", "role": "suggested" }));
    let err = Registry::load_value(doc).expect_err("unknown tool in crossmatrix");
    assert!(
        matches!(&err, RegistryError::CrossmatrixUnknownTool { tool, .. } if tool == "ghost"),
        "expected CrossmatrixUnknownTool(ghost), got {err}"
    );
}

#[test]
fn crossmatrix_row_referencing_unknown_workflow_fails_typed() {
    let mut doc = v3_value();
    doc["crossmatrix"]
        .as_array_mut()
        .expect("crossmatrix is an array")
        .push(
            json!({ "tool": "ripgrep", "workflow": "cognitive/flow.ghost", "role": "dependency" }),
        );
    let err = Registry::load_value(doc).expect_err("unknown workflow in crossmatrix");
    assert!(
        matches!(
            &err,
            RegistryError::CrossmatrixUnknownWorkflow { workflow, .. }
                if workflow == "cognitive/flow.ghost"
        ),
        "expected CrossmatrixUnknownWorkflow(cognitive/flow.ghost), got {err}"
    );
}

#[test]
fn duplicate_ids_fail_typed() {
    let mut doc = v3_value();
    let dup_tool = doc["tools"][1].clone();
    doc["tools"].as_array_mut().expect("tools").push(dup_tool);
    let err = Registry::load_value(doc).expect_err("duplicate tool id");
    assert!(
        matches!(&err, RegistryError::DuplicateToolId(id) if id == "ripgrep"),
        "expected DuplicateToolId(ripgrep), got {err}"
    );

    let mut doc = v3_value();
    let dup_pack = doc["packs"][0].clone();
    doc["packs"].as_array_mut().expect("packs").push(dup_pack);
    let err = Registry::load_value(doc).expect_err("duplicate pack id");
    assert!(
        matches!(&err, RegistryError::DuplicatePackId(id) if id == "cognitive-architectures"),
        "expected DuplicatePackId(cognitive-architectures), got {err}"
    );
}

#[test]
fn unknown_provider_key_fails_at_schema_and_at_validate() {
    // Through the loader, the schema's propertyNames enum catches it.
    let mut doc = v3_value();
    doc["tools"][0]["providers"]["homebrew"] = json!("praxec/tap/cpm-planner");
    let err = Registry::load_value(doc).expect_err("unknown provider key");
    assert!(
        matches!(err, RegistryError::Schema(_)),
        "expected Schema violation, got {err}"
    );

    // Defense in depth: a directly-constructed registry hits the typed
    // validate() check (the provider map is string-keyed for v2 back-compat).
    let mut registry = Registry::load_str(V3_REGISTRY).expect("v3 registry loads");
    registry.tools[0]
        .providers
        .insert("homebrew".to_string(), "praxec/tap/cpm-planner".to_string());
    let err = registry
        .validate()
        .expect_err("unknown provider at validate");
    assert!(
        matches!(
            &err,
            RegistryError::UnknownProvider { tool, provider }
                if tool == "cpm-planner" && provider == "homebrew"
        ),
        "expected UnknownProvider(cpm-planner/homebrew), got {err}"
    );
}

#[test]
fn schema_violations_fail_before_deserialization() {
    // Unknown top-level field.
    let err = Registry::load_str("schema: praxec.packs/v3\nsurprise: true\n")
        .expect_err("unknown top-level field");
    assert!(matches!(err, RegistryError::Schema(_)), "got {err}");

    // Pack missing its required namespace.
    let err = Registry::load_str("schema: praxec.packs/v3\npacks:\n  - id: p\n    name: P\n")
        .expect_err("pack missing namespace");
    assert!(matches!(err, RegistryError::Schema(_)), "got {err}");

    // Crossmatrix role outside the closed enum.
    let mut doc = v3_value();
    doc["crossmatrix"][0]["role"] = json!("vibes");
    let err = Registry::load_value(doc).expect_err("unknown crossmatrix role");
    assert!(matches!(err, RegistryError::Schema(_)), "got {err}");
}

#[test]
fn directly_constructed_registry_validates_crossmatrix() {
    // validate() is a public seam — it must hold without the loader.
    let registry = Registry {
        schema: RegistrySchema::V3,
        packs: Vec::<Pack>::new(),
        tools: vec![RegistryTool {
            id: "solo".to_string(),
            name: "Solo".to_string(),
            description: String::new(),
            repo: None,
            command: None,
            version: None,
            mcp_registry_id: None,
            providers: Default::default(),
            descriptor: None,
            suggested_workflows: vec!["ns/flow.a".to_string()],
        }],
        crossmatrix: vec![CrossmatrixRow {
            tool: "solo".to_string(),
            workflow: "ns/flow.a".to_string(),
            role: CrossmatrixRole::Suggested,
        }],
    };
    registry.validate().expect("consistent registry validates");
}
