//! SPEC §22.6 (v0.3) — when a transition's executor is `kind: script`, the
//! audit transition record's executor descriptor includes `subject` and
//! `hash` alongside the legacy `{kind, ok, durationMs}`.
//!
//! This closes a gap in the v0.2 scripts surface plan: scriptSubject /
//! scriptHash landed in the executor's output JSON but NOT in the audit
//! descriptor. Without these fields on the descriptor, replay-by-hash has
//! to dig through executor-specific output shapes.
//!
//! The round-trip half of the test (FMECA F7) is also covered here: a
//! `kind: noop` transition's descriptor must NOT acquire `subject`/`hash`
//! fields (proves the change is additive only for script executors).

#![cfg(unix)]

use std::sync::Arc;

use praxec_core::audit::{AuditSink, MemoryAuditSink};
use praxec_core::config::resolve_str;
use praxec_core::guards::DefaultGuardEvaluator;
use praxec_core::model::{Principal, StartWorkflow, SubmitTransition};
use praxec_core::ports::{Executor, ExecutorRegistry};
use praxec_core::store::{ConfigDefinitionStore, InMemoryWorkflowStore};
use praxec_core::WorkflowRuntime;
use praxec_executors::{NoopExecutor, ScriptExecutor};
use serde_json::{json, Value};

struct ByKind {
    inner: std::collections::HashMap<String, Arc<dyn Executor>>,
}
impl ByKind {
    fn new() -> Self {
        Self {
            inner: std::collections::HashMap::new(),
        }
    }
    fn with(mut self, kind: &str, exec: Arc<dyn Executor>) -> Self {
        self.inner.insert(kind.to_string(), exec);
        self
    }
}
impl ExecutorRegistry for ByKind {
    fn get(&self, kind: &str) -> Option<Arc<dyn Executor>> {
        self.inner.get(kind).cloned()
    }
}

fn build_runtime(
    yaml: &str,
    executors: Arc<dyn ExecutorRegistry>,
    audit: Arc<MemoryAuditSink>,
) -> WorkflowRuntime {
    let resolved = resolve_str(yaml).expect("config resolves");
    let defs = Arc::new(ConfigDefinitionStore::from_config(&resolved));
    let store = Arc::new(InMemoryWorkflowStore::new());
    let guards = Arc::new(DefaultGuardEvaluator::new());
    WorkflowRuntime::new(defs, store, executors, guards, audit as Arc<dyn AuditSink>)
}

fn last_transition_record(audit: &MemoryAuditSink) -> Value {
    let events = audit.snapshot();
    let rec = events
        .iter()
        .rev()
        .find(|e| e.event_type == "workflow.transition")
        .expect("at least one workflow.transition record");
    serde_json::to_value(rec).expect("serializable")
}

// ── Positive: script executor → descriptor carries subject + hash ─────────

#[tokio::test]
async fn script_executor_transition_record_carries_subject_and_hash() {
    let yaml = r#"
version: "1.0.0"
# Lexicon entry so the pre-start walk (SPEC §30.10.4) passes.
lexicon:
  cargo.release:
    definition_short: "Cargo release build."
scripts:
  build.cargo.release:
    verb: build
    lifecycle: stable
    body: |
      #!/usr/bin/env bash
      echo built
workflows:
  demo:
    initialState: building
    states:
      building:
        transitions:
          done:
            target: terminal
            executor:
              kind: script
              subject: build.cargo.release
      terminal: { terminal: true }
"#;
    let audit = Arc::new(MemoryAuditSink::new());
    let executors: Arc<dyn ExecutorRegistry> =
        Arc::new(ByKind::new().with("script", Arc::new(ScriptExecutor::new())));
    let runtime = build_runtime(yaml, executors, audit.clone());

    let start = runtime
        .start(StartWorkflow {
            definition_id: "demo".into(),
            input: json!({}),
            principal: Principal::anonymous(),
            trace_id: None,
            run_id: None,
            depth: 0,
            parent: None,
        })
        .await
        .expect("start");
    let workflow_id = start["workflow"]["id"].as_str().unwrap().to_string();
    let version = start["workflow"]["version"].as_u64().unwrap();
    let resp = runtime
        .submit(SubmitTransition {
            workflow_id,
            expected_version: version,
            transition: "done".into(),
            arguments: json!({}),
            principal: Principal::anonymous(),
            summary: None,
            trace_id: None,
            run_id: None,
        })
        .await
        .expect("submit ok");
    assert_eq!(resp["error"], Value::Null);

    let record = last_transition_record(&audit);
    let descriptor = record
        .pointer("/payload/executor")
        .or_else(|| record.get("executor"))
        .unwrap_or_else(|| panic!("descriptor missing; record: {record}"));
    assert_eq!(descriptor["kind"], "script");
    assert!(descriptor.get("ok").is_some(), "ok must be present");
    assert!(
        descriptor.get("durationMs").is_some(),
        "durationMs must be present"
    );
    assert_eq!(
        descriptor["subject"].as_str(),
        Some("build.cargo.release"),
        "subject must be on the executor descriptor for script executors"
    );
    let hash = descriptor["hash"]
        .as_str()
        .expect("hash must be on the executor descriptor for script executors");
    assert!(
        hash.starts_with("sha256:"),
        "hash must carry the algorithm prefix"
    );
    assert_eq!(
        hash.len(),
        "sha256:".len() + 64,
        "hash must be 64 hex chars"
    );
}

// ── Negative: non-script executor → descriptor stays at {kind, ok, durationMs} ──

#[tokio::test]
async fn non_script_executor_descriptor_omits_subject_and_hash() {
    let yaml = r#"
version: "1.0.0"
workflows:
  demo:
    initialState: doing
    states:
      doing:
        transitions:
          done:
            target: terminal
            executor: { kind: noop }
      terminal: { terminal: true }
"#;
    let audit = Arc::new(MemoryAuditSink::new());
    let executors: Arc<dyn ExecutorRegistry> =
        Arc::new(ByKind::new().with("noop", Arc::new(NoopExecutor)));
    let runtime = build_runtime(yaml, executors, audit.clone());

    let start = runtime
        .start(StartWorkflow {
            definition_id: "demo".into(),
            input: json!({}),
            principal: Principal::anonymous(),
            trace_id: None,
            run_id: None,
            depth: 0,
            parent: None,
        })
        .await
        .expect("start");
    let workflow_id = start["workflow"]["id"].as_str().unwrap().to_string();
    let version = start["workflow"]["version"].as_u64().unwrap();
    let _ = runtime
        .submit(SubmitTransition {
            workflow_id,
            expected_version: version,
            transition: "done".into(),
            arguments: json!({}),
            principal: Principal::anonymous(),
            summary: None,
            trace_id: None,
            run_id: None,
        })
        .await
        .expect("submit ok");

    let record = last_transition_record(&audit);
    let descriptor = record
        .pointer("/payload/executor")
        .or_else(|| record.get("executor"))
        .unwrap_or_else(|| panic!("descriptor missing; record: {record}"));
    assert_eq!(descriptor["kind"], "noop");
    assert!(
        descriptor.get("subject").is_none(),
        "non-script descriptor must NOT carry subject; got: {descriptor}"
    );
    assert!(
        descriptor.get("hash").is_none(),
        "non-script descriptor must NOT carry hash; got: {descriptor}"
    );
}
