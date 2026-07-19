//! SQLite-backed `WorkflowStore`.
//!
//! Uses `rusqlite` with the `bundled` feature so no system libsqlite is
//! needed. The schema is one table:
//!
//! ```sql
//! CREATE TABLE workflows (
//!     id           TEXT PRIMARY KEY,
//!     version      INTEGER NOT NULL,
//!     instance     TEXT    NOT NULL  -- JSON-serialized WorkflowInstance
//! );
//! ```
//!
//! All ops happen on a `tokio::task::spawn_blocking` boundary to keep
//! synchronous SQLite calls off the async runtime. Optimistic locking is
//! enforced with `UPDATE ... WHERE id = ? AND version = ?` inside a
//! transaction; rows-affected = 0 means stale.

use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use anyhow::{Context, anyhow, bail};
use async_trait::async_trait;
use rusqlite::{Connection, params};

use crate::model::WorkflowInstance;
use crate::ports::WorkflowStore;

#[derive(Clone)]
pub struct SqliteWorkflowStore {
    conn: Arc<Mutex<Connection>>,
    path: PathBuf,
}

impl SqliteWorkflowStore {
    pub fn open(path: impl Into<PathBuf>) -> anyhow::Result<Self> {
        let path = path.into();
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)
                .with_context(|| format!("creating dir {}", parent.display()))?;
        }
        let conn = Connection::open(&path)
            .with_context(|| format!("opening sqlite at {}", path.display()))?;
        // WAL gives much better concurrent-read performance for our pattern
        // (many reads, occasional writes).
        conn.pragma_update(None, "journal_mode", "WAL")?;
        conn.pragma_update(None, "synchronous", "NORMAL")?;
        conn.execute(
            "CREATE TABLE IF NOT EXISTS workflows (
                id       TEXT PRIMARY KEY,
                version  INTEGER NOT NULL,
                instance TEXT NOT NULL
            )",
            [],
        )?;
        // run_id uniqueness identifies a RUN (a tree) and only its ROOT
        // establishes it. A sub-workflow inherits the parent's run_id so
        // correlation survives the spawn, so children (`$.parent` present) are
        // EXCLUDED from the constraint — otherwise every spawn would violate it
        // once every root run carries a minted run_id. `$.parent IS NULL` holds
        // for a root whether serde omits the field or writes an explicit null.
        // Drop-then-create (not IF NOT EXISTS): an older DB may hold the prior,
        // childless predicate, and the index is derived data — recreating is
        // lossless.
        conn.execute("DROP INDEX IF EXISTS idx_workflows_run_id", [])?;
        conn.execute(
            "CREATE UNIQUE INDEX idx_workflows_run_id \
             ON workflows (json_extract(instance, '$.run_env.run_id')) \
             WHERE json_extract(instance, '$.run_env.run_id') IS NOT NULL \
               AND json_extract(instance, '$.parent') IS NULL",
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
            "CREATE TABLE IF NOT EXISTS workflows (
                id       TEXT PRIMARY KEY,
                version  INTEGER NOT NULL,
                instance TEXT NOT NULL
            )",
            [],
        )?;
        // run_id uniqueness identifies a RUN (a tree) and only its ROOT
        // establishes it. A sub-workflow inherits the parent's run_id so
        // correlation survives the spawn, so children (`$.parent` present) are
        // EXCLUDED from the constraint — otherwise every spawn would violate it
        // once every root run carries a minted run_id. `$.parent IS NULL` holds
        // for a root whether serde omits the field or writes an explicit null.
        // Drop-then-create (not IF NOT EXISTS): an older DB may hold the prior,
        // childless predicate, and the index is derived data — recreating is
        // lossless.
        conn.execute("DROP INDEX IF EXISTS idx_workflows_run_id", [])?;
        conn.execute(
            "CREATE UNIQUE INDEX idx_workflows_run_id \
             ON workflows (json_extract(instance, '$.run_env.run_id')) \
             WHERE json_extract(instance, '$.run_env.run_id') IS NOT NULL \
               AND json_extract(instance, '$.parent') IS NULL",
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
impl WorkflowStore for SqliteWorkflowStore {
    async fn create(&self, instance: WorkflowInstance) -> anyhow::Result<WorkflowInstance> {
        let conn = self.conn.clone();
        let json = serde_json::to_string(&instance)?;
        let inst = instance.clone();
        tokio::task::spawn_blocking(move || -> anyhow::Result<WorkflowInstance> {
            let conn = conn.lock().expect("LOCK_POISONED: sqlite connection");
            let rows = conn.execute(
                "INSERT INTO workflows (id, version, instance) VALUES (?1, ?2, ?3)",
                params![inst.id, inst.version as i64, json],
            );
            match rows {
                Ok(_) => Ok(inst),
                Err(rusqlite::Error::SqliteFailure(e, ref msg))
                    if e.code == rusqlite::ErrorCode::ConstraintViolation =>
                {
                    // NOTE: this distinguishes the two UNIQUE constraints by the
                    // index name in rusqlite's error *message* text — not a
                    // stability-guaranteed structured API. It's guarded by
                    // `store_run_id_uniqueness.rs` (which asserts the outcome), and
                    // the index name is distinctive; a rusqlite upgrade that
                    // reworded constraint messages would route a run_id conflict to
                    // the id-collision arm, which the test would catch.
                    let m = msg.as_deref().unwrap_or("");
                    if m.contains("idx_workflows_run_id") {
                        bail!("RUN_ID_ALREADY_RUNNING: run_id already has an in-flight instance")
                    }
                    bail!("workflow id collision: {}", inst.id)
                }
                Err(e) => Err(e.into()),
            }
        })
        .await?
    }

    async fn load(&self, workflow_id: &str) -> anyhow::Result<WorkflowInstance> {
        let conn = self.conn.clone();
        let id = workflow_id.to_string();
        tokio::task::spawn_blocking(move || -> anyhow::Result<WorkflowInstance> {
            let conn = conn.lock().expect("LOCK_POISONED: sqlite connection");
            let mut stmt = conn.prepare("SELECT instance FROM workflows WHERE id = ?1")?;
            let json: String =
                stmt.query_row(params![id], |row| row.get(0))
                    .map_err(|e| match e {
                        rusqlite::Error::QueryReturnedNoRows => {
                            anyhow!("workflow {} not found", id)
                        }
                        other => other.into(),
                    })?;
            let instance: WorkflowInstance = serde_json::from_str(&json)?;
            // CMP-039 — the row was selected by its `id` column; the embedded
            // `instance.id` MUST agree. A mismatch means the JSON blob was
            // written under the wrong key (corruption), so we refuse to hand
            // back an instance masquerading as another id.
            if instance.id != id {
                bail!(
                    "CORRUPT_INSTANCE: row id '{}' does not match embedded instance.id '{}'",
                    id,
                    instance.id
                );
            }
            Ok(instance)
        })
        .await?
    }

    async fn save_if_version(
        &self,
        instance: WorkflowInstance,
        expected_version: u64,
    ) -> anyhow::Result<WorkflowInstance> {
        let conn = self.conn.clone();
        let json = serde_json::to_string(&instance)?;
        let inst = instance.clone();
        tokio::task::spawn_blocking(move || -> anyhow::Result<WorkflowInstance> {
            let mut conn = conn.lock().expect("LOCK_POISONED: sqlite connection");
            let tx = conn.transaction()?;
            // Confirm the row exists.
            let exists: bool = tx
                .query_row(
                    "SELECT 1 FROM workflows WHERE id = ?1",
                    params![inst.id],
                    |_| Ok(()),
                )
                .map(|_: ()| true)
                .or_else(|e| match e {
                    rusqlite::Error::QueryReturnedNoRows => Ok(false),
                    other => Err(other),
                })?;
            if !exists {
                bail!("workflow {} not found", inst.id);
            }
            let updated = tx.execute(
                "UPDATE workflows SET version = ?1, instance = ?2
                 WHERE id = ?3 AND version = ?4",
                params![inst.version as i64, json, inst.id, expected_version as i64],
            )?;
            if updated == 0 {
                bail!(
                    "stale workflow version (expected {} for {})",
                    expected_version,
                    inst.id
                );
            }
            tx.commit()?;
            Ok(inst)
        })
        .await?
    }

    /// SPEC §32 — look up an instance by `run_id` using a JSON query against
    /// the stored blob. Returns the column `id` of the matching row so the
    /// runtime duplicate-run guard (RUN_ID_ALREADY_RUNNING) has teeth on
    /// SQLite-backed deployments.
    async fn find_by_run_id(&self, run_id: &str) -> anyhow::Result<Option<String>> {
        let conn = self.conn.clone();
        let run_id = run_id.to_string();
        tokio::task::spawn_blocking(move || -> anyhow::Result<Option<String>> {
            let conn = conn.lock().expect("LOCK_POISONED: sqlite connection");
            let mut stmt = conn.prepare(
                "SELECT id FROM workflows \
                 WHERE json_extract(instance, '$.run_env.run_id') = ?1 \
                 LIMIT 1",
            )?;
            let id: Option<String> = stmt
                .query_row(params![run_id], |row| row.get(0))
                .map(Some)
                .or_else(|e| match e {
                    rusqlite::Error::QueryReturnedNoRows => Ok(None),
                    other => Err(other),
                })?;
            Ok(id)
        })
        .await?
    }

    async fn list_waiting_on_lock(&self) -> anyhow::Result<Vec<WorkflowInstance>> {
        let conn = self.conn.clone();
        tokio::task::spawn_blocking(move || -> anyhow::Result<Vec<WorkflowInstance>> {
            let conn = conn.lock().expect("LOCK_POISONED: sqlite connection");
            let mut stmt = conn.prepare(
                "SELECT instance FROM workflows \
                 WHERE json_extract(instance, '$.context._lock_wait') IS NOT NULL",
            )?;
            let rows = stmt.query_map([], |row| row.get::<_, String>(0))?;
            let mut waiting = Vec::new();
            for json in rows {
                let inst: WorkflowInstance = serde_json::from_str(&json?)?;
                waiting.push(inst);
            }
            Ok(waiting)
        })
        .await?
    }

    async fn list_waiting_on_subworkflow(&self) -> anyhow::Result<Vec<WorkflowInstance>> {
        let conn = self.conn.clone();
        tokio::task::spawn_blocking(move || -> anyhow::Result<Vec<WorkflowInstance>> {
            let conn = conn.lock().expect("LOCK_POISONED: sqlite connection");
            let mut stmt = conn.prepare(
                "SELECT instance FROM workflows \
                 WHERE json_extract(instance, '$.context._subworkflow_wait') IS NOT NULL",
            )?;
            let rows = stmt.query_map([], |row| row.get::<_, String>(0))?;
            let mut waiting = Vec::new();
            for json in rows {
                let inst: WorkflowInstance = serde_json::from_str(&json?)?;
                waiting.push(inst);
            }
            Ok(waiting)
        })
        .await?
    }

    async fn list_all(&self) -> anyhow::Result<Vec<WorkflowInstance>> {
        let conn = self.conn.clone();
        tokio::task::spawn_blocking(move || -> anyhow::Result<Vec<WorkflowInstance>> {
            let conn = conn.lock().expect("LOCK_POISONED: sqlite connection");
            let mut stmt = conn.prepare("SELECT instance FROM workflows")?;
            let rows = stmt.query_map([], |row| row.get::<_, String>(0))?;
            let mut all = Vec::new();
            for json in rows {
                all.push(serde_json::from_str::<WorkflowInstance>(&json?)?);
            }
            Ok(all)
        })
        .await?
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::WorkflowInstance;

    fn instance(id: &str) -> WorkflowInstance {
        WorkflowInstance {
            id: id.to_string(),
            definition_id: "demo".into(),
            definition_version: "1.0.0".into(),
            definition: serde_json::json!({"initialState": "s", "states": {}}),
            state: "s".into(),
            version: 0,
            input: serde_json::json!({}),
            context: serde_json::json!({}),
            started_at: chrono::Utc::now(),
            run_env: crate::RunEnv::for_test(),
            cancelled_at: None,
            cancelled_reason: None,
            depth: 0,
            parent: None,
        }
    }

    /// CMP-039 — `load` selects by the `id` column; if the stored JSON blob's
    /// embedded `instance.id` disagrees, that is corruption and must error
    /// rather than silently return an instance under the wrong id.
    #[tokio::test]
    async fn load_rejects_id_mismatch() {
        let store = SqliteWorkflowStore::open_in_memory().unwrap();
        // Write a blob for instance "wf_real" under the row key "wf_wrong".
        let blob = serde_json::to_string(&instance("wf_real")).unwrap();
        {
            let conn = store.conn.lock().unwrap();
            conn.execute(
                "INSERT INTO workflows (id, version, instance) VALUES (?1, ?2, ?3)",
                params!["wf_wrong", 0_i64, blob],
            )
            .unwrap();
        }
        let err = store.load("wf_wrong").await.unwrap_err();
        assert!(
            err.to_string().contains("CORRUPT_INSTANCE"),
            "expected CORRUPT_INSTANCE, got: {err}"
        );
    }
}
