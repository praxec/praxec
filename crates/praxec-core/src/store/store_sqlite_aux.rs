//! SQLite-backed `EvidenceStore`.
//!
//! Mirrors `SqliteWorkflowStore`: `rusqlite` with the `bundled` feature, WAL
//! journal mode, and all DB work on a `tokio::task::spawn_blocking` boundary.
//! Evidence is append-only, stored as one JSON-blob row per record:
//!
//! ```sql
//! CREATE TABLE evidence (
//!     workflow_id TEXT NOT NULL,
//!     evidence    TEXT NOT NULL  -- JSON-serialized Evidence
//! );
//! ```

use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use anyhow::Context;
use async_trait::async_trait;
use rusqlite::{Connection, params};

use crate::model::Evidence;
use crate::ports::{EvidenceStore, GuidanceAcknowledgmentStore, ScriptAcknowledgmentStore};

#[derive(Clone)]
pub struct SqliteEvidenceStore {
    conn: Arc<Mutex<Connection>>,
    path: PathBuf,
}

impl SqliteEvidenceStore {
    pub fn open(path: impl Into<PathBuf>) -> anyhow::Result<Self> {
        let path = path.into();
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)
                .with_context(|| format!("creating dir {}", parent.display()))?;
        }
        let conn = Connection::open(&path)
            .with_context(|| format!("opening sqlite at {}", path.display()))?;
        conn.pragma_update(None, "journal_mode", "WAL")?;
        conn.pragma_update(None, "synchronous", "NORMAL")?;
        conn.execute(
            "CREATE TABLE IF NOT EXISTS evidence (
                workflow_id TEXT NOT NULL,
                evidence    TEXT NOT NULL
            )",
            [],
        )?;
        conn.execute(
            "CREATE INDEX IF NOT EXISTS idx_evidence_wf ON evidence(workflow_id)",
            [],
        )?;
        Ok(Self {
            conn: Arc::new(Mutex::new(conn)),
            path,
        })
    }

    pub fn open_in_memory() -> anyhow::Result<Self> {
        let conn = Connection::open_in_memory()?;
        conn.execute(
            "CREATE TABLE IF NOT EXISTS evidence (
                workflow_id TEXT NOT NULL,
                evidence    TEXT NOT NULL
            )",
            [],
        )?;
        conn.execute(
            "CREATE INDEX IF NOT EXISTS idx_evidence_wf ON evidence(workflow_id)",
            [],
        )?;
        Ok(Self {
            conn: Arc::new(Mutex::new(conn)),
            path: PathBuf::from(":memory:"),
        })
    }

    pub fn path(&self) -> &std::path::Path {
        &self.path
    }
}

#[async_trait]
impl EvidenceStore for SqliteEvidenceStore {
    async fn record(&self, workflow_id: &str, evidence: Evidence) -> anyhow::Result<()> {
        let conn = self.conn.clone();
        let workflow_id = workflow_id.to_string();
        let json = serde_json::to_string(&evidence)?;
        tokio::task::spawn_blocking(move || -> anyhow::Result<()> {
            let conn = conn.lock().expect("LOCK_POISONED: sqlite connection");
            conn.execute(
                "INSERT INTO evidence (workflow_id, evidence) VALUES (?1, ?2)",
                params![workflow_id, json],
            )?;
            Ok(())
        })
        .await?
    }

    async fn list(&self, workflow_id: &str) -> anyhow::Result<Vec<Evidence>> {
        let conn = self.conn.clone();
        let workflow_id = workflow_id.to_string();
        tokio::task::spawn_blocking(move || -> anyhow::Result<Vec<Evidence>> {
            let conn = conn.lock().expect("LOCK_POISONED: sqlite connection");
            let mut stmt = conn
                .prepare("SELECT evidence FROM evidence WHERE workflow_id = ?1 ORDER BY rowid")?;
            let rows = stmt.query_map(params![workflow_id], |row| row.get::<_, String>(0))?;
            let mut out = Vec::new();
            for json in rows {
                let evidence: Evidence = serde_json::from_str(&json?)?;
                out.push(evidence);
            }
            Ok(out)
        })
        .await?
    }
}

/// SQLite-backed acknowledgment store serving BOTH
/// [`GuidanceAcknowledgmentStore`] and [`ScriptAcknowledgmentStore`] — the two
/// traits are identical in shape, so one struct implements both, instantiated
/// against a different table (`guidance_acks` / `script_acks`). Acks survive a
/// restart; per `(workflow_id, subject)` the LAST write wins via an
/// `ON CONFLICT` upsert. Mirrors `SqliteEvidenceStore`: `rusqlite` (bundled),
/// WAL, all DB work on `spawn_blocking`.
///
/// ```sql
/// CREATE TABLE {table} (
///     workflow_id TEXT NOT NULL,
///     subject     TEXT NOT NULL,
///     body_hash   TEXT NOT NULL,
///     PRIMARY KEY (workflow_id, subject)
/// );
/// ```
#[derive(Clone)]
pub struct SqliteAckStore {
    conn: Arc<Mutex<Connection>>,
    table: &'static str,
}

impl SqliteAckStore {
    pub fn open(path: impl Into<PathBuf>, table: &'static str) -> anyhow::Result<Self> {
        // The table name is an internal constant, never user input. Guarding it
        // to the allowed set makes the `format!`'d DDL provably non-injectable.
        assert!(
            matches!(table, "guidance_acks" | "script_acks"),
            "SqliteAckStore table must be an allowed internal constant, got {table:?}"
        );
        let path = path.into();
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)
                .with_context(|| format!("creating dir {}", parent.display()))?;
        }
        let conn = Connection::open(&path)
            .with_context(|| format!("opening sqlite at {}", path.display()))?;
        conn.pragma_update(None, "journal_mode", "WAL")?;
        conn.pragma_update(None, "synchronous", "NORMAL")?;
        Self::create_table(&conn, table)?;
        Ok(Self {
            conn: Arc::new(Mutex::new(conn)),
            table,
        })
    }

    #[cfg(test)]
    pub fn open_in_memory(table: &'static str) -> anyhow::Result<Self> {
        assert!(
            matches!(table, "guidance_acks" | "script_acks"),
            "SqliteAckStore table must be an allowed internal constant, got {table:?}"
        );
        let conn = Connection::open_in_memory()?;
        Self::create_table(&conn, table)?;
        Ok(Self {
            conn: Arc::new(Mutex::new(conn)),
            table,
        })
    }

    fn create_table(conn: &Connection, table: &'static str) -> anyhow::Result<()> {
        // Safe `format!`: `table` is guarded to an allowed-set constant above.
        conn.execute(
            &format!(
                "CREATE TABLE IF NOT EXISTS {table} (
                    workflow_id TEXT NOT NULL,
                    subject     TEXT NOT NULL,
                    body_hash   TEXT NOT NULL,
                    PRIMARY KEY (workflow_id, subject)
                )"
            ),
            [],
        )?;
        Ok(())
    }

    async fn record_inner(
        &self,
        workflow_id: &str,
        subject: &str,
        body_hash: &str,
    ) -> anyhow::Result<()> {
        let conn = self.conn.clone();
        let table = self.table;
        let workflow_id = workflow_id.to_string();
        let subject = subject.to_string();
        let body_hash = body_hash.to_string();
        tokio::task::spawn_blocking(move || -> anyhow::Result<()> {
            let conn = conn.lock().expect("LOCK_POISONED: sqlite connection");
            // Safe `format!`: `table` is guarded to an allowed-set constant.
            conn.execute(
                &format!(
                    "INSERT INTO {table} (workflow_id, subject, body_hash)
                     VALUES (?1, ?2, ?3)
                     ON CONFLICT(workflow_id, subject)
                     DO UPDATE SET body_hash = excluded.body_hash"
                ),
                params![workflow_id, subject, body_hash],
            )?;
            Ok(())
        })
        .await?
    }

    async fn get_inner(&self, workflow_id: &str, subject: &str) -> anyhow::Result<Option<String>> {
        let conn = self.conn.clone();
        let table = self.table;
        let workflow_id = workflow_id.to_string();
        let subject = subject.to_string();
        tokio::task::spawn_blocking(move || -> anyhow::Result<Option<String>> {
            let conn = conn.lock().expect("LOCK_POISONED: sqlite connection");
            // Safe `format!`: `table` is guarded to an allowed-set constant.
            let res = conn.query_row(
                &format!("SELECT body_hash FROM {table} WHERE workflow_id = ?1 AND subject = ?2"),
                params![workflow_id, subject],
                |row| row.get::<_, String>(0),
            );
            match res {
                Ok(hash) => Ok(Some(hash)),
                Err(rusqlite::Error::QueryReturnedNoRows) => Ok(None),
                Err(e) => Err(e.into()),
            }
        })
        .await?
    }
}

#[async_trait]
impl GuidanceAcknowledgmentStore for SqliteAckStore {
    async fn record(
        &self,
        workflow_id: &str,
        subject: &str,
        body_hash: &str,
    ) -> anyhow::Result<()> {
        self.record_inner(workflow_id, subject, body_hash).await
    }

    async fn last_acknowledged_hash(
        &self,
        workflow_id: &str,
        subject: &str,
    ) -> anyhow::Result<Option<String>> {
        self.get_inner(workflow_id, subject).await
    }
}

#[async_trait]
impl ScriptAcknowledgmentStore for SqliteAckStore {
    async fn record(
        &self,
        workflow_id: &str,
        subject: &str,
        body_hash: &str,
    ) -> anyhow::Result<()> {
        self.record_inner(workflow_id, subject, body_hash).await
    }

    async fn last_acknowledged_hash(
        &self,
        workflow_id: &str,
        subject: &str,
    ) -> anyhow::Result<Option<String>> {
        self.get_inner(workflow_id, subject).await
    }
}

#[cfg(test)]
mod tests {
    use super::*;

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
    async fn record_and_list_scoped_by_workflow() {
        let store = SqliteEvidenceStore::open_in_memory().unwrap();
        store.record("wf1", ev("e1")).await.unwrap();
        store.record("wf1", ev("e2")).await.unwrap();
        store.record("wf2", ev("e3")).await.unwrap();

        let wf1 = store.list("wf1").await.unwrap();
        assert_eq!(wf1.len(), 2);
        assert_eq!(wf1[0].id, "e1");
        assert_eq!(wf1[1].id, "e2");
        assert_eq!(store.list("wf2").await.unwrap().len(), 1);
        assert_eq!(store.list("absent").await.unwrap().len(), 0);
    }

    #[tokio::test]
    async fn ack_upsert_last_write_wins_and_tables_independent() {
        let g = SqliteAckStore::open_in_memory("guidance_acks").unwrap();
        GuidanceAcknowledgmentStore::record(&g, "wf1", "subj", "h1")
            .await
            .unwrap();
        GuidanceAcknowledgmentStore::record(&g, "wf1", "subj", "h2")
            .await
            .unwrap();
        assert_eq!(
            GuidanceAcknowledgmentStore::last_acknowledged_hash(&g, "wf1", "subj")
                .await
                .unwrap(),
            Some("h2".into()),
            "last write wins"
        );
        assert_eq!(
            GuidanceAcknowledgmentStore::last_acknowledged_hash(&g, "wf1", "missing")
                .await
                .unwrap(),
            None
        );

        // Distinct in-memory DBs → independent tables.
        let s = SqliteAckStore::open_in_memory("script_acks").unwrap();
        assert_eq!(
            ScriptAcknowledgmentStore::last_acknowledged_hash(&s, "wf1", "subj")
                .await
                .unwrap(),
            None
        );
    }

    #[test]
    #[should_panic(expected = "allowed internal constant")]
    fn ack_rejects_unknown_table() {
        let _ = SqliteAckStore::open_in_memory("evil; DROP TABLE x");
    }
}
