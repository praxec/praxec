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

use anyhow::{Context, bail};
use async_trait::async_trait;
use rusqlite::{Connection, params};

use crate::model::{Evidence, ParkedAgentSession};
use crate::ports::{
    EvidenceStore, GuidanceAcknowledgmentStore, ParkedSessionStore, ScriptAcknowledgmentStore,
};

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

/// SQLite-backed [`ParkedSessionStore`] — P12 R1.4, the durable park for a
/// suspended agent tool-loop (`docs/await-resume-architecture.md`). One
/// JSON-blob row per parked session, keyed by `correlation_id`:
///
/// ```sql
/// CREATE TABLE parked_agent_sessions (
///     correlation_id TEXT PRIMARY KEY,
///     record         TEXT NOT NULL  -- JSON-serialized ParkedAgentSession
/// );
/// ```
///
/// Mirrors `SqliteEvidenceStore`: `rusqlite` (bundled), WAL, all DB work on a
/// `tokio::task::spawn_blocking` boundary.
#[derive(Clone)]
pub struct SqliteParkedSessionStore {
    conn: Arc<Mutex<Connection>>,
    path: PathBuf,
}

impl SqliteParkedSessionStore {
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
        Self::create_table(&conn)?;
        Ok(Self {
            conn: Arc::new(Mutex::new(conn)),
            path,
        })
    }

    pub fn open_in_memory() -> anyhow::Result<Self> {
        let conn = Connection::open_in_memory()?;
        Self::create_table(&conn)?;
        Ok(Self {
            conn: Arc::new(Mutex::new(conn)),
            path: PathBuf::from(":memory:"),
        })
    }

    fn create_table(conn: &Connection) -> anyhow::Result<()> {
        conn.execute(
            "CREATE TABLE IF NOT EXISTS parked_agent_sessions (
                correlation_id TEXT PRIMARY KEY,
                record         TEXT NOT NULL
            )",
            [],
        )?;
        Ok(())
    }

    pub fn path(&self) -> &std::path::Path {
        &self.path
    }

    /// Parse + integrity-check one stored row (shared by `load` and `list`).
    /// A blob that doesn't parse, or whose embedded `correlation_id` disagrees
    /// with its row key, is CORRUPTION — refuse it with a typed message
    /// (mirrors `SqliteWorkflowStore`'s CMP-039 guard), never a panic.
    fn parse_row(row_id: &str, json: &str) -> anyhow::Result<ParkedAgentSession> {
        let record: ParkedAgentSession = serde_json::from_str(json).map_err(|e| {
            anyhow::anyhow!("PARKED_SESSION_CORRUPT: row '{row_id}' is not a valid record: {e}")
        })?;
        if record.correlation_id != row_id {
            bail!(
                "PARKED_SESSION_CORRUPT: row key '{}' does not match embedded correlation_id '{}'",
                row_id,
                record.correlation_id
            );
        }
        Ok(record)
    }
}

#[async_trait]
impl ParkedSessionStore for SqliteParkedSessionStore {
    async fn park(&self, record: ParkedAgentSession) -> anyhow::Result<()> {
        let conn = self.conn.clone();
        let correlation_id = record.correlation_id.clone();
        let json = serde_json::to_string(&record)?;
        tokio::task::spawn_blocking(move || -> anyhow::Result<()> {
            let conn = conn.lock().expect("LOCK_POISONED: sqlite connection");
            let rows = conn.execute(
                "INSERT INTO parked_agent_sessions (correlation_id, record) VALUES (?1, ?2)",
                params![correlation_id, json],
            );
            match rows {
                Ok(_) => Ok(()),
                Err(rusqlite::Error::SqliteFailure(e, _))
                    if e.code == rusqlite::ErrorCode::ConstraintViolation =>
                {
                    // Never silently overwrite a live parked frame.
                    bail!(
                        "PARKED_SESSION_DUPLICATE: correlation_id '{}' is already parked",
                        correlation_id
                    )
                }
                Err(e) => Err(e.into()),
            }
        })
        .await?
    }

    async fn load(&self, correlation_id: &str) -> anyhow::Result<Option<ParkedAgentSession>> {
        let conn = self.conn.clone();
        let id = correlation_id.to_string();
        tokio::task::spawn_blocking(move || -> anyhow::Result<Option<ParkedAgentSession>> {
            let conn = conn.lock().expect("LOCK_POISONED: sqlite connection");
            let json: Option<String> = conn
                .query_row(
                    "SELECT record FROM parked_agent_sessions WHERE correlation_id = ?1",
                    params![id],
                    |row| row.get(0),
                )
                .map(Some)
                .or_else(|e| match e {
                    rusqlite::Error::QueryReturnedNoRows => Ok(None),
                    other => Err(other),
                })?;
            match json {
                Some(json) => Ok(Some(Self::parse_row(&id, &json)?)),
                None => Ok(None),
            }
        })
        .await?
    }

    async fn remove(&self, correlation_id: &str) -> anyhow::Result<()> {
        let conn = self.conn.clone();
        let id = correlation_id.to_string();
        tokio::task::spawn_blocking(move || -> anyhow::Result<()> {
            let conn = conn.lock().expect("LOCK_POISONED: sqlite connection");
            conn.execute(
                "DELETE FROM parked_agent_sessions WHERE correlation_id = ?1",
                params![id],
            )?;
            Ok(())
        })
        .await?
    }

    async fn list(&self) -> anyhow::Result<Vec<ParkedAgentSession>> {
        let conn = self.conn.clone();
        tokio::task::spawn_blocking(move || -> anyhow::Result<Vec<ParkedAgentSession>> {
            let conn = conn.lock().expect("LOCK_POISONED: sqlite connection");
            let mut stmt = conn.prepare(
                "SELECT correlation_id, record FROM parked_agent_sessions ORDER BY rowid",
            )?;
            let rows = stmt.query_map([], |row| {
                Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
            })?;
            let mut out = Vec::new();
            for row in rows {
                let (id, json) = row?;
                out.push(Self::parse_row(&id, &json)?);
            }
            Ok(out)
        })
        .await?
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

    // ── ParkedSessionStore (P12 R1.4) ────────────────────────────────────────

    fn parked(correlation_id: &str) -> ParkedAgentSession {
        ParkedAgentSession {
            correlation_id: correlation_id.into(),
            prompt: "approve the plan?".into(),
            session: serde_json::json!({ "model": "anthropic:claude-sonnet-4-6" }),
            conversation: serde_json::json!({ "history": [], "pending": [] }),
            parked_at: chrono::Utc::now(),
        }
    }

    #[tokio::test]
    async fn parked_session_round_trips_byte_faithfully() {
        let store = SqliteParkedSessionStore::open_in_memory().unwrap();
        let rec = parked("corr-1");
        store.park(rec.clone()).await.unwrap();
        let loaded = store.load("corr-1").await.unwrap().expect("parked row");
        assert_eq!(loaded, rec, "persist → reload must round-trip exactly");
    }

    #[tokio::test]
    async fn loading_an_unknown_correlation_id_is_none_not_an_error() {
        let store = SqliteParkedSessionStore::open_in_memory().unwrap();
        assert_eq!(store.load("absent").await.unwrap(), None);
    }

    #[tokio::test]
    async fn parking_a_duplicate_correlation_id_is_refused() {
        let store = SqliteParkedSessionStore::open_in_memory().unwrap();
        store.park(parked("corr-1")).await.unwrap();
        let err = store.park(parked("corr-1")).await.unwrap_err();
        assert!(
            err.to_string().contains("PARKED_SESSION_DUPLICATE"),
            "a duplicate park must never overwrite a live frame; got: {err}"
        );
        // The original frame is untouched.
        assert!(store.load("corr-1").await.unwrap().is_some());
    }

    #[tokio::test]
    async fn remove_clears_the_row_and_is_idempotent() {
        let store = SqliteParkedSessionStore::open_in_memory().unwrap();
        store.park(parked("corr-1")).await.unwrap();
        store.remove("corr-1").await.unwrap();
        assert_eq!(store.load("corr-1").await.unwrap(), None);
        // Removing again (already resumed) is not an error.
        store.remove("corr-1").await.unwrap();
    }

    #[tokio::test]
    async fn list_returns_parked_sessions_oldest_first() {
        let store = SqliteParkedSessionStore::open_in_memory().unwrap();
        store.park(parked("corr-a")).await.unwrap();
        store.park(parked("corr-b")).await.unwrap();
        let ids: Vec<String> = store
            .list()
            .await
            .unwrap()
            .into_iter()
            .map(|r| r.correlation_id)
            .collect();
        assert_eq!(ids, vec!["corr-a".to_string(), "corr-b".to_string()]);
    }

    #[tokio::test]
    async fn a_corrupt_parked_row_is_a_typed_error_not_a_panic() {
        let store = SqliteParkedSessionStore::open_in_memory().unwrap();
        {
            let conn = store.conn.lock().unwrap();
            conn.execute(
                "INSERT INTO parked_agent_sessions (correlation_id, record) VALUES (?1, ?2)",
                params!["corr-bad", "{not valid json"],
            )
            .unwrap();
        }
        let err = store.load("corr-bad").await.unwrap_err();
        assert!(
            err.to_string().contains("PARKED_SESSION_CORRUPT"),
            "expected PARKED_SESSION_CORRUPT, got: {err}"
        );
    }

    #[tokio::test]
    async fn a_row_whose_embedded_id_disagrees_is_corrupt() {
        // Mirrors the workflow store's CMP-039 guard: a blob written under the
        // wrong key must be refused, never handed back masquerading.
        let store = SqliteParkedSessionStore::open_in_memory().unwrap();
        let blob = serde_json::to_string(&parked("corr-real")).unwrap();
        {
            let conn = store.conn.lock().unwrap();
            conn.execute(
                "INSERT INTO parked_agent_sessions (correlation_id, record) VALUES (?1, ?2)",
                params!["corr-wrong", blob],
            )
            .unwrap();
        }
        let err = store.load("corr-wrong").await.unwrap_err();
        assert!(
            err.to_string().contains("PARKED_SESSION_CORRUPT"),
            "expected PARKED_SESSION_CORRUPT on id mismatch, got: {err}"
        );
    }
}
