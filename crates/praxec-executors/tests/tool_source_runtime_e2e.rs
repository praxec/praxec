//! v0.0.17 dogfood F11 + F14 — RUNTIME-level integration coverage.
//!
//! These guard two features that previously had only executor-unit tests or
//! manual CLI proof (docs/v0.0.17-functional-validation.md):
//!
//! - **F11** — a `kind: tool_source` transition driven end-to-end through the
//!   real `WorkflowRuntime` with the PRODUCTION executor registry
//!   (`default_registry`): descriptor → connection gate → cli dispatch →
//!   `cli_output` evidence → terminal `succeeded`. This proves the
//!   descriptor→dispatch→outcome path through the runtime, not just the
//!   executor in isolation.
//!
//! - **F14** — the connection spawn-gate holds through the runtime. The
//!   config goes through the REAL load gate (`load_resolved_with_repos`,
//!   which applies the SPEC §9.5 staged-connection grant gate) — never a
//!   hand-stamped `_ungrantedConnections`. A staged-but-ungranted connection
//!   referenced by a transition fails typed (`UNGRANTED_PACK_CONNECTION`
//!   with the grant remedy) and does NOT advance the workflow; the granted
//!   variant of the same config passes the gate and succeeds.
//!
//! Uses a trivial POSIX cli tool (`printf`) exactly like the documented
//! example, so no mock transport is needed.

#![cfg(unix)]

use std::path::PathBuf;
use std::sync::Arc;

use praxec_core::WorkflowRuntime;
use praxec_core::audit::{AuditSink, MemoryAuditSink};
use praxec_core::config::load_resolved_with_repos;
use praxec_core::guards::DefaultGuardEvaluator;
use praxec_core::model::{Principal, StartWorkflow, SubmitTransition};
use praxec_core::ports::WorkflowStore;
use praxec_core::store::{ConfigDefinitionStore, InMemoryEvidenceStore, InMemoryWorkflowStore};
use serde_json::{Value, json};
use tempfile::TempDir;

// ── harness ───────────────────────────────────────────────────────────────

/// Write `body` to `<tempdir>/praxec.yaml` and resolve it through the full
/// multi-repo load path — the same gate the binary uses, which applies the
/// staged-connection grant gate (SPEC §9.5) and stamps ungranted connections.
fn load_host(td: &TempDir, body: &str) -> Value {
    let path: PathBuf = td.path().join("praxec.yaml");
    std::fs::write(&path, body).expect("write host config");
    load_resolved_with_repos(&path)
        .expect("host config resolves")
        .0
}

/// Build a `WorkflowRuntime` around the PRODUCTION executor registry
/// (`default_registry`) — the same wiring the binary uses, so `tool_source`
/// and `cli` dispatch with their real connection registries from `config`.
fn build_runtime(config: &Value, audit: Arc<MemoryAuditSink>) -> WorkflowRuntime {
    let definitions = Arc::new(ConfigDefinitionStore::from_config(config));
    let store: Arc<dyn WorkflowStore> = Arc::new(InMemoryWorkflowStore::new());
    let evidence = Arc::new(InMemoryEvidenceStore::new());
    let guards = Arc::new(DefaultGuardEvaluator::with_evidence(evidence.clone()));
    let registry = praxec_executors::default_registry(config);
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

/// Start `definition_id`, then submit `transition` with `arguments`; return
/// the submit response. Panics only on transport-level failures — a rejected
/// or failed transition still returns `Ok(response)` for the test to assert.
async fn start_and_submit(
    runtime: &WorkflowRuntime,
    definition_id: &str,
    transition: &str,
    arguments: Value,
) -> Value {
    let start = runtime
        .start(StartWorkflow {
            definition_id: definition_id.to_string(),
            input: json!({}),
            principal: Principal::anonymous(),
            run_env: praxec_core::RunEnv::for_test(),
            depth: 0,
            parent: None,
        })
        .await
        .unwrap_or_else(|e| panic!("start({definition_id}): {e}"));
    let workflow_id = start
        .pointer("/workflow/id")
        .and_then(Value::as_str)
        .expect("start response carries workflow.id")
        .to_string();
    let version = start
        .pointer("/workflow/version")
        .and_then(Value::as_u64)
        .expect("start response carries workflow.version");
    runtime
        .submit(SubmitTransition {
            workflow_id,
            expected_version: version,
            transition: transition.to_string(),
            arguments,
            principal: Principal::anonymous(),
            summary: None,
            trace_id: None,
            run_id: None,
        })
        .await
        .expect("submit returns Ok(response) even on rejection")
}

fn evidence_kinds(resp: &Value) -> Vec<String> {
    resp.get("evidence")
        .and_then(Value::as_array)
        .map(|a| {
            a.iter()
                .filter_map(|e| e.get("kind").and_then(Value::as_str))
                .map(str::to_string)
                .collect()
        })
        .unwrap_or_default()
}

// ── F11: kind: tool_source driven end-to-end through the runtime ──────────

/// Host config: a granted cli connection (`printer`, backed by `printf`)
/// staged + granted through the real gate, and a workflow whose transition
/// is `executor: { kind: tool_source, operation: print, descriptor: … }` —
/// the documented v0.0.17 example (functional-validation doc), inline.
const F11_HOST: &str = r#"
version: "1.0.0"
stagedConnections:
  printer: { kind: cli, command: printf }
grant_connections: [printer]
workflows:
  demo.print:
    initialState: ready
    states:
      ready:
        transitions:
          print:
            target: done
            executor:
              kind: tool_source
              operation: print
              descriptor:
                schema_version: praxec.tool/v1
                name: printer
                version: "0.1.0"
                kind: cli
                reach:
                  connection_name: printer
                  grant_as: printer
                  connection: { kind: cli, command: printf }
                operations:
                  - id: print
                    verb: run
                    input_schema:
                      type: object
                      required: [name]
                      properties:
                        name: { type: string }
                    output_schema: { type: object }
                    cli: { args: ["ok:%s", "$.arguments.name"] }
      done: { terminal: true }
"#;

#[tokio::test]
async fn f11_tool_source_transition_drives_to_succeeded_with_cli_output_evidence() {
    let td = TempDir::new().unwrap();
    let config = load_host(&td, F11_HOST);
    // The grant gate promoted the staged connection into the live registry —
    // the descriptor's reach is satisfied by a genuinely granted connection.
    assert_eq!(
        config
            .pointer("/connections/printer/kind")
            .and_then(Value::as_str),
        Some("cli"),
        "granted staged connection must be live; config: {config:#}"
    );

    let audit = Arc::new(MemoryAuditSink::new());
    let runtime = build_runtime(&config, audit.clone());
    let resp = start_and_submit(&runtime, "demo.print", "print", json!({ "name": "world" })).await;

    // Terminal success through the real runtime (a success response carries
    // no `error` key at all).
    assert_eq!(
        resp["error"],
        Value::Null,
        "no error expected; resp: {resp:#}"
    );
    assert_eq!(
        resp.pointer("/workflow/state").and_then(Value::as_str),
        Some("done"),
        "workflow must reach the terminal state; resp: {resp:#}"
    );
    assert_eq!(
        resp.pointer("/result/status").and_then(Value::as_str),
        Some("succeeded"),
        "mission must resolve succeeded; resp: {resp:#}"
    );

    // The cli executor's evidence surfaced through the runtime response —
    // proof the tool ACTUALLY ran (descriptor → tool_source → cli dispatch).
    let kinds = evidence_kinds(&resp);
    assert!(
        kinds.iter().any(|k| k == "cli_output"),
        "expected cli_output evidence; got {kinds:?}; resp: {resp:#}"
    );
    let summary = resp
        .get("evidence")
        .and_then(Value::as_array)
        .and_then(|a| a.iter().find(|e| e["kind"] == "cli_output"))
        .and_then(|e| e.get("summary"))
        .and_then(Value::as_str)
        .unwrap_or_default();
    assert!(
        summary.contains("printf"),
        "cli_output evidence names the command; got: {summary}"
    );

    // The audit transition record's executor descriptor names tool_source —
    // the REAL runtime dispatched the real executor kind.
    let events = audit.snapshot();
    let record = events
        .iter()
        .rev()
        .find(|e| e.event_type == "workflow.transition")
        .expect("a workflow.transition record exists");
    let record = serde_json::to_value(record).expect("serializable");
    assert_eq!(
        record
            .pointer("/payload/executor/kind")
            .and_then(Value::as_str),
        Some("tool_source"),
        "transition record must carry the tool_source descriptor; record: {record:#}"
    );
}

// ── F14: the connection spawn-gate holds through the runtime ──────────────

/// Host config template: a staged cli connection and a workflow transition
/// that spawns through it (`kind: cli`, no inline command — the connection
/// is the only route to a spawnable command). `{GRANT}` toggles the grant.
fn f14_host(granted: bool) -> String {
    let grant = if granted {
        "grant_connections: [printer]\n"
    } else {
        ""
    };
    format!(
        r#"
version: "1.0.0"
stagedConnections:
  printer: {{ kind: cli, command: printf }}
{grant}workflows:
  demo.spawn:
    initialState: ready
    states:
      ready:
        transitions:
          run:
            target: done
            executor:
              kind: cli
              connection: printer
              args: ["hello"]
      done: {{ terminal: true }}
"#
    )
}

#[tokio::test]
async fn f14_ungranted_staged_connection_fails_typed_through_the_runtime() {
    let td = TempDir::new().unwrap();
    let config = load_host(&td, &f14_host(false));
    // The REAL load gate diverted the staged connection: not live, stamped
    // ungranted with the grant remedy.
    assert!(
        config.pointer("/connections/printer").is_none(),
        "a staged-but-ungranted connection must NOT be live; config: {config:#}"
    );
    assert!(
        config
            .pointer("/praxec/_ungrantedConnections/printer")
            .is_some(),
        "the load gate must stamp the ungranted connection; config: {config:#}"
    );

    let audit = Arc::new(MemoryAuditSink::new());
    let runtime = build_runtime(&config, audit);
    let resp = start_and_submit(&runtime, "demo.spawn", "run", json!({})).await;

    // The gate holds: typed failure with the grant remedy, no spawn.
    assert_eq!(
        resp.pointer("/error/code").and_then(Value::as_str),
        Some("EXECUTOR_FAILED"),
        "ungranted spawn must fail; resp: {resp:#}"
    );
    let message = resp
        .pointer("/error/message")
        .and_then(Value::as_str)
        .unwrap_or_default();
    assert!(
        message.contains("UNGRANTED_PACK_CONNECTION"),
        "failure must be the typed gate error; got: {message}"
    );
    assert!(
        message.contains("px connections grant printer"),
        "failure must carry the exact grant remedy; got: {message}"
    );
    // …and the workflow did not advance past the gate.
    assert_eq!(
        resp.pointer("/workflow/state").and_then(Value::as_str),
        Some("ready"),
        "a gated spawn must not advance the workflow; resp: {resp:#}"
    );
    assert_ne!(
        resp.pointer("/result/status").and_then(Value::as_str),
        Some("succeeded"),
        "a gated spawn must not resolve the mission; resp: {resp:#}"
    );
}

#[tokio::test]
async fn f14_granted_variant_of_the_same_config_passes_the_gate_and_succeeds() {
    let td = TempDir::new().unwrap();
    let config = load_host(&td, &f14_host(true));
    assert_eq!(
        config
            .pointer("/connections/printer/command")
            .and_then(Value::as_str),
        Some("printf"),
        "the granted connection must be live with its command; config: {config:#}"
    );
    assert!(
        config
            .pointer("/praxec/_ungrantedConnections/printer")
            .is_none(),
        "a granted connection must not be stamped ungranted; config: {config:#}"
    );

    let audit = Arc::new(MemoryAuditSink::new());
    let runtime = build_runtime(&config, audit);
    let resp = start_and_submit(&runtime, "demo.spawn", "run", json!({})).await;

    assert_eq!(
        resp["error"],
        Value::Null,
        "granted spawn must pass the gate; resp: {resp:#}"
    );
    assert_eq!(
        resp.pointer("/workflow/state").and_then(Value::as_str),
        Some("done"),
        "granted spawn must advance to terminal; resp: {resp:#}"
    );
    assert_eq!(
        resp.pointer("/result/status").and_then(Value::as_str),
        Some("succeeded"),
        "granted spawn resolves the mission; resp: {resp:#}"
    );
    let kinds = evidence_kinds(&resp);
    assert!(
        kinds.iter().any(|k| k == "cli_output"),
        "the granted spawn actually ran the tool; got {kinds:?}; resp: {resp:#}"
    );
}
