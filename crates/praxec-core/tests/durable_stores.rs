use praxec_core::model::Evidence;
use praxec_core::ports::{EvidenceStore, GuidanceAcknowledgmentStore, ScriptAcknowledgmentStore};
use praxec_core::store::{SqliteAckStore, SqliteEvidenceStore};

fn ev(id: &str) -> Evidence {
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
async fn evidence_survives_a_reopen() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("p3.db");
    {
        let s = SqliteEvidenceStore::open(&path).unwrap();
        s.record("wf1", ev("e1")).await.unwrap();
        s.record("wf1", ev("e2")).await.unwrap();
        s.record("wf2", ev("e3")).await.unwrap();
    }
    let s = SqliteEvidenceStore::open(&path).unwrap(); // reopen = simulated restart
    assert_eq!(
        s.list("wf1").await.unwrap().len(),
        2,
        "wf1 evidence persisted"
    );
    assert_eq!(s.list("wf2").await.unwrap().len(), 1);
    assert_eq!(s.list("absent").await.unwrap().len(), 0);
}

#[tokio::test]
async fn acks_survive_reopen_and_last_write_wins() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("p3ack.db");
    {
        let g = SqliteAckStore::open(&path, "guidance_acks").unwrap();
        GuidanceAcknowledgmentStore::record(&g, "wf1", "tdd.discipline", "hashA")
            .await
            .unwrap();
        GuidanceAcknowledgmentStore::record(&g, "wf1", "tdd.discipline", "hashB")
            .await
            .unwrap(); // re-ack overwrites
    }
    let g = SqliteAckStore::open(&path, "guidance_acks").unwrap(); // reopen = restart
    assert_eq!(
        GuidanceAcknowledgmentStore::last_acknowledged_hash(&g, "wf1", "tdd.discipline")
            .await
            .unwrap(),
        Some("hashB".into())
    );
    assert_eq!(
        GuidanceAcknowledgmentStore::last_acknowledged_hash(&g, "wf1", "absent")
            .await
            .unwrap(),
        None
    );

    // Same struct type also satisfies ScriptAcknowledgmentStore, on its own table.
    let s = SqliteAckStore::open(&path, "script_acks").unwrap();
    ScriptAcknowledgmentStore::record(&s, "wf1", "cargo.release", "h1")
        .await
        .unwrap();
    assert_eq!(
        ScriptAcknowledgmentStore::last_acknowledged_hash(&s, "wf1", "cargo.release")
            .await
            .unwrap(),
        Some("h1".into())
    );
    // guidance + script tables are independent
    assert_eq!(
        ScriptAcknowledgmentStore::last_acknowledged_hash(&s, "wf1", "tdd.discipline")
            .await
            .unwrap(),
        None
    );
}
