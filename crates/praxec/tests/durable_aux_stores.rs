//! P3 Task 3 — the gateway's evidence + ack store builders are durable when
//! `store.kind=sqlite` and ephemeral (`InMemory*`) otherwise. Tests the
//! builders directly: durability means a SECOND builder over the same config
//! sees what the FIRST recorded.

use praxec::gateway::{build_evidence_store, build_guidance_ack_store};
use praxec_core::model::Evidence;

fn evidence(id: &str) -> Evidence {
    Evidence {
        kind: "k".into(),
        id: id.into(),
        uri: None,
        summary: None,
        digest: None,
        confidence: None,
    }
}

#[tokio::test]
async fn evidence_builder_is_durable_for_sqlite_memory_for_memory() {
    let dir = tempfile::tempdir().unwrap();
    let sqlite_cfg = serde_json::json!({
        "store": { "kind": "sqlite", "path": dir.path().join("g.db").to_str().unwrap() }
    });

    // sqlite → durable: record, then a SECOND builder over the same config sees it.
    let s1 = build_evidence_store(&sqlite_cfg).unwrap();
    s1.record("wf", evidence("e1")).await.unwrap();
    let s2 = build_evidence_store(&sqlite_cfg).unwrap();
    assert_eq!(
        s2.list("wf").await.unwrap().len(),
        1,
        "sqlite evidence store is durable across builder calls"
    );

    // memory → not durable: a second builder is a fresh empty store.
    let mem_cfg = serde_json::json!({ "store": { "kind": "memory" }});
    let m1 = build_evidence_store(&mem_cfg).unwrap();
    m1.record("wf", evidence("e1")).await.unwrap();
    let m2 = build_evidence_store(&mem_cfg).unwrap();
    assert_eq!(
        m2.list("wf").await.unwrap().len(),
        0,
        "in-memory store does not persist across builder calls"
    );
}

#[tokio::test]
async fn guidance_ack_builder_is_durable_for_sqlite() {
    let dir = tempfile::tempdir().unwrap();
    let sqlite_cfg = serde_json::json!({
        "store": { "kind": "sqlite", "path": dir.path().join("g.db").to_str().unwrap() }
    });

    let a1 = build_guidance_ack_store(&sqlite_cfg).unwrap();
    a1.record("wf", "topic", "hash1").await.unwrap();
    let a2 = build_guidance_ack_store(&sqlite_cfg).unwrap();
    assert_eq!(
        a2.last_acknowledged_hash("wf", "topic").await.unwrap(),
        Some("hash1".to_string()),
        "sqlite guidance ack store is durable across builder calls"
    );

    let mem_cfg = serde_json::json!({ "store": { "kind": "memory" }});
    let m1 = build_guidance_ack_store(&mem_cfg).unwrap();
    m1.record("wf", "topic", "hash1").await.unwrap();
    let m2 = build_guidance_ack_store(&mem_cfg).unwrap();
    assert_eq!(
        m2.last_acknowledged_hash("wf", "topic").await.unwrap(),
        None,
        "in-memory guidance ack store does not persist across builder calls"
    );
}
