//! SPEC §8.4 — `DefinitionStoreWritable` audit-before-commit ordering and
//! state-visibility invariants.

use std::sync::Arc;

use async_trait::async_trait;
use praxec_core::audit::{AuditEvent, AuditSink, MemoryAuditSink};
use praxec_core::ports::{DefinitionStore, DefinitionStoreWritable};
use praxec_core::store::InMemoryWritableDefinitionStore;
use serde_json::json;

// ── Positive: register makes definition loadable ────────────────────────────

#[tokio::test]
async fn register_then_load_returns_definition() {
    let audit = Arc::new(MemoryAuditSink::new());
    let store = InMemoryWritableDefinitionStore::new(audit.clone() as Arc<dyn AuditSink>);
    store
        .register("new_wf", json!({ "initialState": "s" }), None)
        .await
        .expect("register succeeds");
    let loaded = store.load("new_wf").await.expect("load succeeds");
    assert_eq!(loaded["initialState"].as_str(), Some("s"));
}

// ── Positive: definition.published emitted BEFORE definition is loadable ────

#[tokio::test]
async fn published_audit_event_emitted() {
    let audit = Arc::new(MemoryAuditSink::new());
    let store = InMemoryWritableDefinitionStore::new(audit.clone() as Arc<dyn AuditSink>);
    store
        .register("new_wf", json!({ "initialState": "s" }), None)
        .await
        .unwrap();
    let kinds = audit.event_types();
    assert!(
        kinds.contains(&"definition.published".to_string()),
        "expected definition.published; got: {kinds:?}"
    );
}

#[tokio::test]
async fn published_event_appears_before_loadable_event() {
    let audit = Arc::new(MemoryAuditSink::new());
    let store = InMemoryWritableDefinitionStore::new(audit.clone() as Arc<dyn AuditSink>);
    store
        .register("ordering", json!({ "initialState": "s" }), None)
        .await
        .unwrap();
    let events = audit.snapshot();
    let pub_idx = events
        .iter()
        .position(|e| e.event_type == "definition.published")
        .expect("published event present");
    let load_idx = events
        .iter()
        .position(|e| e.event_type == "definition.loadable");
    if let Some(load_idx) = load_idx {
        assert!(
            pub_idx < load_idx,
            "published ({pub_idx}) must come before loadable ({load_idx})"
        );
    }
}

// ── Negative: audit failure aborts commit ──────────────────────────────────

#[derive(Clone)]
struct FailingAudit;

#[async_trait]
impl AuditSink for FailingAudit {
    async fn record(&self, _event: AuditEvent) -> anyhow::Result<()> {
        anyhow::bail!("simulated audit-write failure")
    }
}

#[tokio::test]
async fn audit_failure_aborts_register_commit() {
    let audit: Arc<dyn AuditSink> = Arc::new(FailingAudit);
    let store = InMemoryWritableDefinitionStore::new(audit);
    let err = store
        .register("never_published", json!({}), None)
        .await
        .expect_err("audit failure must abort");
    assert!(format!("{err}").contains("RECORD_WRITE_FAILED"));
    // Definition must NOT be loadable after a failed commit.
    let load = store.load("never_published").await;
    assert!(
        load.is_err(),
        "definition must not be loadable post-failed-audit"
    );
}

// ── Edge: load of unknown id errors with explicit message ──────────────────

#[tokio::test]
async fn load_unknown_definition_errors() {
    let audit = Arc::new(MemoryAuditSink::new());
    let store = InMemoryWritableDefinitionStore::new(audit as Arc<dyn AuditSink>);
    let err = store
        .load("nonexistent")
        .await
        .expect_err("unknown id must error");
    assert!(format!("{err}").contains("nonexistent"));
}

// ── Edge: re-register same id replaces (last-write-wins) ───────────────────

#[tokio::test]
async fn re_register_replaces_definition() {
    let audit = Arc::new(MemoryAuditSink::new());
    let store = InMemoryWritableDefinitionStore::new(audit as Arc<dyn AuditSink>);
    store
        .register("wf", json!({ "version": "v1" }), None)
        .await
        .unwrap();
    store
        .register("wf", json!({ "version": "v2" }), None)
        .await
        .unwrap();
    let loaded = store.load("wf").await.unwrap();
    assert_eq!(loaded["version"].as_str(), Some("v2"));
}
