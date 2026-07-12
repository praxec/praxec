//! D6 — the `praxec.packs/v3` registry, wired into the LIVE gateway.
//!
//! Everything here runs through the real `praxec` binary, because the defect D6
//! closes was precisely that the registry loader and the selector's topology
//! parameter both existed and nothing in production ever connected them. A seam
//! test would have passed all along; only the gateway can prove the wiring.
//!
//! Covered:
//! - registry configured → its tool descriptors are searchable through
//!   `praxec.query {kind: "tool"}` (the catalog reaches the live index);
//! - registry configured → the crossmatrix topology CHANGES the ranking, in the
//!   direction the crossmatrix implies;
//! - no registry configured → byte-for-byte today's behavior (no tools, no
//!   topology term, workflow search unchanged);
//! - registry configured but missing / malformed → fail-fast with a diagnosable
//!   `REGISTRY_*` error, never a silent registry-less boot;
//! - the registry survives a hot-reload cycle (D6-T5), so topology keeps feeding
//!   ranking after `praxec.command { reload: true }`.

use std::path::Path;
use std::process::Command;

use rmcp::ServiceExt;
use rmcp::model::CallToolRequestParams;
use rmcp::transport::TokioChildProcess;
use serde_json::{Value, json};

// ── fixtures ──────────────────────────────────────────────────────────────

/// Two workflows that are DELIBERATELY indistinguishable to the lexical scorer:
/// same title, same description, same state text. The only thing that can
/// separate them is the registry topology — and `aaa_audit` wins the selector's
/// id tie-break, so without a registry it ranks first.
const CONFIG: &str = r#"
version: "1.0.0"
gateway:
  allow_ephemeral: true
workflows:
  aaa_audit:
    title: Audit repository
    description: Audit a repository for defects.
    initialState: start
    states:
      start:
        goal: Audit a repository for defects.
        transitions:
          begin:
            target: done
            executor: { kind: noop }
      done:
        terminal: true
        outcome: success
  zzz_audit:
    title: Audit repository
    description: Audit a repository for defects.
    initialState: start
    states:
      start:
        goal: Audit a repository for defects.
        transitions:
          begin:
            target: done
            executor: { kind: noop }
      done:
        terminal: true
        outcome: success
"#;

/// A `praxec.packs/v3` registry whose two tools both compose `zzz_audit` (one via
/// `suggested_workflows`, one via a `dependency` crossmatrix edge) — and neither
/// composes `aaa_audit`.
const REGISTRY: &str = r#"
schema: praxec.packs/v3
tools:
  - id: prometheus
    name: prometheus
    description: Time-series metrics database with alerting.
    version: 0.1.0
    descriptor:
      schema_version: praxec.tool/v1
      name: prometheus
      version: 0.1.0
      description: Time-series metrics database with alerting.
      tags: [observability]
      kind: rest
      reach:
        connection_name: prometheus
        grant_as: prometheus
        connection:
          kind: rest
          baseUrl: https://prometheus.example
          headers: {}
      operations:
        - id: query-range
          verb: search
          input_schema: { type: object }
          output_schema: { type: object }
          rest: { method: GET, path: /api/v1/query_range }
      suggested_workflows: [zzz_audit]
  - id: ripgrep
    name: ripgrep
    description: Fast line-oriented search.
    command: rg
    version: 14.1.0
    descriptor:
      schema_version: praxec.tool/v1
      name: ripgrep
      version: 14.1.0
      description: Fast line-oriented search.
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
crossmatrix:
  - { tool: prometheus, workflow: zzz_audit, role: suggested }
  - { tool: ripgrep, workflow: zzz_audit, role: dependency }
"#;

/// Write `CONFIG` (plus, when `registry` is Some, a `discovery.registry:` knob
/// pointing at the given file) into a temp dir; return the config path.
fn write_config(dir: &Path, registry: Option<&Path>) -> String {
    let mut config = CONFIG.to_string();
    if let Some(path) = registry {
        config.push_str(&format!("discovery:\n  registry: {}\n", path.display()));
    }
    let config_path = dir.join("praxec.yaml");
    std::fs::write(&config_path, config).expect("write config");
    config_path.to_string_lossy().to_string()
}

fn write_registry(dir: &Path, body: &str) -> std::path::PathBuf {
    let path = dir.join("packs.yaml");
    std::fs::write(&path, body).expect("write registry");
    path
}

/// `praxec query --config <cfg> <json>` → the command output. Not asserted
/// successful: the fail-fast cases assert on the FAILURE.
fn query_raw(config_path: &str, args: &str) -> std::process::Output {
    let bin = env!("CARGO_BIN_EXE_praxec");
    let mut cmd = Command::new(bin);
    cmd.arg("query").arg("--config").arg(config_path).arg(args);
    // No network in tests: keep discovery on the free lexical index. (The tool
    // catalog must be searchable WITHOUT embeddings — that is the point of the
    // lexical path carrying the registry's tools too.)
    cmd.env("PRAXEC_EMBEDDING_FILE", "/tmp/praxec-no-embed.json");
    cmd.output().expect("run praxec query")
}

fn query(config_path: &str, args: &str) -> Value {
    let out = query_raw(config_path, args);
    assert!(
        out.status.success(),
        "query exited non-zero.\nstdout: {}\nstderr: {}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr),
    );
    serde_json::from_slice(&out.stdout).expect("query stdout is JSON")
}

/// The ids of a search response's items, in ranked order.
fn ids(response: &Value) -> Vec<String> {
    response["items"]
        .as_array()
        .expect("items array")
        .iter()
        .map(|i| i["item"]["id"].as_str().expect("item id").to_string())
        .collect()
}

/// The `why` string of a ranked item — the selector's audit trail.
fn why(response: &Value, id: &str) -> String {
    response["items"]
        .as_array()
        .expect("items array")
        .iter()
        .find(|i| i["item"]["id"] == id)
        .unwrap_or_else(|| panic!("no ranked item `{id}` in {response}"))["ranking"]["why"]
        .as_str()
        .expect("ranking.why")
        .to_string()
}

// ── D6-T1 / the tool surface, end to end ──────────────────────────────────

/// The registry's tool descriptors reach the LIVE discovery index: a
/// `praxec.query {kind: "tool"}` through the real binary finds a tool that exists
/// nowhere in the gateway config — only in the registry.
#[test]
fn registry_tools_are_searchable_through_the_live_query_surface() {
    let dir = tempfile::tempdir().expect("tempdir");
    let registry = write_registry(dir.path(), REGISTRY);
    let cfg = write_config(dir.path(), Some(&registry));

    let response = query(&cfg, r#"{"query":"metrics alerting","kind":"tool"}"#);
    assert_eq!(
        ids(&response),
        ["prometheus"],
        "the registry's tool catalog is searchable: {response}"
    );
    let item = &response["items"][0]["item"];
    assert_eq!(item["kind"], "tool");
    assert_eq!(
        item["description"],
        "Time-series metrics database with alerting."
    );

    // The tool's HATEOAS next-step is the workflow its descriptor nominates —
    // the tool is reachable, not a dead-end hit.
    assert_eq!(item["links"][0]["rel"], "start_suggested_workflow");
    assert_eq!(item["links"][0]["args"]["definitionId"], "zzz_audit");

    // Both descriptors are indexed, not just the first. (`prometheus` also hits
    // here — its `query-range` operation carries the `search` verb — which is the
    // projection working as designed: an operation's vocabulary is searchable.
    // What matters is that ripgrep is indexed and ranks first for its own text.)
    let rg = query(&cfg, r#"{"query":"line-oriented search","kind":"tool"}"#);
    assert_eq!(
        ids(&rg).first().map(String::as_str),
        Some("ripgrep"),
        "{rg}"
    );
}

/// D6-T1/T4 through the gateway — the crossmatrix changes what the gateway
/// RECOMMENDS. Two workflows the lexical scorer cannot tell apart; the one the
/// registry's tools compose comes back first, and says why.
#[test]
fn registry_topology_changes_the_ranking_through_the_gateway() {
    let dir = tempfile::tempdir().expect("tempdir");
    let registry = write_registry(dir.path(), REGISTRY);
    let with_registry = write_config(dir.path(), Some(&registry));

    let no_registry_dir = tempfile::tempdir().expect("tempdir");
    let without_registry = write_config(no_registry_dir.path(), None);

    let args = r#"{"query":"audit repository defects","kind":"workflow"}"#;

    // Baseline (no registry): identical scores, id tie-break → aaa_audit first.
    let before = query(&without_registry, args);
    assert_eq!(ids(&before), ["aaa_audit", "zzz_audit"], "{before}");
    assert!(
        why(&before, "zzz_audit").contains("topology 0.000 (no registry tool links)"),
        "no registry ⇒ a uniform zero topology term: {}",
        why(&before, "zzz_audit")
    );

    // With the registry: the composed workflow overtakes it.
    let after = query(&with_registry, args);
    assert_eq!(
        ids(&after),
        ["zzz_audit", "aaa_audit"],
        "the crossmatrix-linked workflow must outrank the unlinked one: {after}"
    );
    let explanation = why(&after, "zzz_audit");
    assert!(
        explanation.contains("prometheus, ripgrep"),
        "the ranking names the tools that composed it: {explanation}"
    );
    assert!(
        why(&after, "aaa_audit").contains("no registry tool links"),
        "the unlinked workflow gets no boost: {}",
        why(&after, "aaa_audit")
    );
}

// ── D6-T2 — no registry configured: no regression ─────────────────────────

#[test]
fn no_registry_configured_behaves_exactly_as_before() {
    let dir = tempfile::tempdir().expect("tempdir");
    let cfg = write_config(dir.path(), None);

    // Workflow search still answers.
    let workflows = query(
        &cfg,
        r#"{"query":"audit repository defects","kind":"workflow"}"#,
    );
    assert_eq!(ids(&workflows), ["aaa_audit", "zzz_audit"], "{workflows}");

    // …with no topology term anywhere.
    for id in ["aaa_audit", "zzz_audit"] {
        assert!(
            why(&workflows, id).contains("topology 0.000"),
            "{}",
            why(&workflows, id)
        );
    }

    // And the tool surface is simply empty — not an error.
    let tools = query(&cfg, r#"{"query":"metrics alerting","kind":"tool"}"#);
    assert!(
        ids(&tools).is_empty(),
        "no registry ⇒ no tools in the catalog: {tools}"
    );
}

// ── D6-T3 — configured but unloadable: fail fast ──────────────────────────

#[test]
fn a_missing_registry_file_fails_the_boot_with_a_diagnosable_error() {
    let dir = tempfile::tempdir().expect("tempdir");
    let absent = dir.path().join("packs.yaml"); // never written
    let cfg = write_config(dir.path(), Some(&absent));

    let out = query_raw(&cfg, r#"{"query":"anything"}"#);
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        !out.status.success(),
        "a configured-but-missing registry must NOT boot registry-less.\nstdout: {}",
        String::from_utf8_lossy(&out.stdout)
    );
    assert!(stderr.contains("REGISTRY_READ"), "{stderr}");
    assert!(stderr.contains("discovery.registry"), "{stderr}");
}

/// The other door: `serve` does not exit on a config fault — it comes up
/// DEGRADED and answers every call with the health report (so an MCP client sees
/// a diagnosis instead of an opaque transport error). A broken registry must land
/// in THAT channel, loudly, rather than being shrugged off into a healthy gateway
/// that quietly serves no tools and no topology.
#[tokio::test]
async fn serve_refuses_to_come_up_healthy_on_a_broken_registry() {
    let dir = tempfile::tempdir().expect("tempdir");
    let absent = dir.path().join("packs.yaml"); // never written
    let cfg = write_config(dir.path(), Some(&absent));

    let bin = env!("CARGO_BIN_EXE_praxec");
    let mut cmd = tokio::process::Command::new(bin);
    cmd.arg("serve").arg("--config").arg(&cfg);
    cmd.env("PRAXEC_EMBEDDING_FILE", "/tmp/praxec-no-embed.json");
    let transport = TokioChildProcess::new(cmd).expect("spawn praxec serve");
    let service = ().serve(transport).await.expect("mcp client handshake");

    let params = CallToolRequestParams::new("praxec.query".to_string());
    let err = service
        .peer()
        .call_tool(params)
        .await
        .expect_err("a degraded gateway must refuse the call, not serve it");
    let rendered = format!("{err:?}");
    assert!(
        rendered.contains("REGISTRY_READ") && rendered.contains("discovery.registry"),
        "the refusal names the registry fault: {rendered}"
    );
}

#[test]
fn a_malformed_registry_fails_the_boot_with_a_diagnosable_error() {
    let dir = tempfile::tempdir().expect("tempdir");
    // A crossmatrix edge to a tool that does not exist — the FM7 drift the
    // loader refuses (a registry that is not layerable must not be layered).
    let broken = write_registry(
        dir.path(),
        "schema: praxec.packs/v3\ntools: []\ncrossmatrix:\n  - { tool: ghost, workflow: zzz_audit, role: suggested }\n",
    );
    let cfg = write_config(dir.path(), Some(&broken));

    let out = query_raw(&cfg, r#"{"query":"anything"}"#);
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(!out.status.success(), "a broken registry must not boot");
    assert!(
        stderr.contains("REGISTRY_CROSSMATRIX_UNKNOWN_TOOL"),
        "the error names the exact defect, not just 'failed': {stderr}"
    );
}

// ── D6-T5 — the registry survives a hot-reload ────────────────────────────

/// The reload path rebuilds the index AND the registry from the same seam. If it
/// dropped the registry (the D3 defect, one layer up), the topology term would go
/// silently dead after the first reload — the ranking would revert to the
/// pre-registry answer while the gateway still claimed the registry was loaded.
#[tokio::test]
async fn topology_still_feeds_ranking_after_a_hot_reload() {
    let dir = tempfile::tempdir().expect("tempdir");
    let registry = write_registry(dir.path(), REGISTRY);
    let cfg = write_config(dir.path(), Some(&registry));

    let bin = env!("CARGO_BIN_EXE_praxec");
    let mut cmd = tokio::process::Command::new(bin);
    cmd.arg("serve").arg("--config").arg(&cfg);
    cmd.env("PRAXEC_EMBEDDING_FILE", "/tmp/praxec-no-embed.json");
    let transport = TokioChildProcess::new(cmd).expect("spawn praxec serve");
    let service = ().serve(transport).await.expect("mcp client handshake");
    let peer = service.peer().clone();

    let call = async |tool: &str, args: Value| -> Value {
        let params = CallToolRequestParams::new(tool.to_string())
            .with_arguments(args.as_object().cloned().unwrap_or_default());
        peer.call_tool(params)
            .await
            .unwrap_or_else(|e| panic!("{tool} call: {e}"))
            .structured_content
            .expect("structured response body")
    };

    let search = json!({ "query": "audit repository defects", "kind": "workflow" });
    let before = call("praxec.query", search.clone()).await;
    assert_eq!(ids(&before), ["zzz_audit", "aaa_audit"], "{before}");

    // The SAME gated reload SIGHUP fires.
    let reloaded = call("praxec.command", json!({ "reload": true })).await;
    assert_eq!(reloaded["status"], "reloaded", "{reloaded}");

    let after = call("praxec.query", search).await;
    assert_eq!(
        ids(&after),
        ["zzz_audit", "aaa_audit"],
        "the registry must survive the reload — topology still ranks: {after}"
    );
    assert!(
        why(&after, "zzz_audit").contains("prometheus, ripgrep"),
        "{}",
        why(&after, "zzz_audit")
    );

    // The tool catalog survives the reload too (same seam, same rebuild).
    let tools = call(
        "praxec.query",
        json!({ "query": "metrics alerting", "kind": "tool" }),
    )
    .await;
    assert_eq!(ids(&tools), ["prometheus"], "{tools}");
}
