//! Filesystem-backed `WorkflowStore`.
//!
//! Each workflow lives in its own JSON file under `<root>/<workflow_id>.json`.
//! Writes are made durable through fsync'd atomic rename (`*.tmp` → `*.json`,
//! with the tmp file and the parent directory both fsync'd) and serialized
//! through a single async mutex so the version check + write is one critical
//! section. Loads read directly off disk — they don't need the lock since file
//! writes are atomic.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use anyhow::{Context, anyhow, bail};
use async_trait::async_trait;
use tokio::sync::Mutex;

use crate::model::WorkflowInstance;
use crate::ports::WorkflowStore;

#[derive(Clone)]
pub struct FileWorkflowStore {
    root: PathBuf,
    write_lock: Arc<Mutex<()>>,
}

impl FileWorkflowStore {
    pub fn new(root: impl Into<PathBuf>) -> anyhow::Result<Self> {
        let root = root.into();
        std::fs::create_dir_all(&root)
            .with_context(|| format!("creating workflow store dir {}", root.display()))?;
        Ok(Self {
            root,
            write_lock: Arc::new(Mutex::new(())),
        })
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    fn path_for(&self, id: &str) -> PathBuf {
        // Workflow ids are uuid-shaped (`wf_<32 hex>`), no path-separator
        // concerns. Defensive sanitize anyway.
        let sanitized: String = id
            .chars()
            .map(|c| {
                if c.is_alphanumeric() || c == '_' || c == '-' {
                    c
                } else {
                    '_'
                }
            })
            .collect();
        self.root.join(format!("{sanitized}.json"))
    }

    async fn read_file(&self, id: &str) -> anyhow::Result<Option<WorkflowInstance>> {
        let path = self.path_for(id);
        match tokio::fs::read(&path).await {
            Ok(bytes) => {
                let inst = serde_json::from_slice(&bytes)
                    .with_context(|| format!("parsing {}", path.display()))?;
                Ok(Some(inst))
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
            Err(e) => Err(anyhow!("reading {}: {e}", path.display())),
        }
    }

    /// Durable atomic write (CMP-021): write to a `*.tmp` sidecar, `fsync` the
    /// tmp file so its bytes are on the storage device, rename it over the
    /// final path (atomic on POSIX), then `fsync` the parent directory so the
    /// rename itself survives a crash. Without the directory fsync, a crash
    /// after rename can leave the directory entry pointing at the old (or no)
    /// inode even though the data was synced.
    async fn write_atomic(&self, instance: &WorkflowInstance) -> anyhow::Result<()> {
        use tokio::io::AsyncWriteExt;
        let final_path = self.path_for(&instance.id);
        let tmp_path = final_path.with_extension("json.tmp");
        let bytes = serde_json::to_vec(instance)?;

        let mut file = tokio::fs::File::create(&tmp_path)
            .await
            .with_context(|| format!("creating {}", tmp_path.display()))?;
        file.write_all(&bytes)
            .await
            .with_context(|| format!("writing {}", tmp_path.display()))?;
        // Force the tmp file's contents to disk before we rename it into place.
        file.sync_all()
            .await
            .with_context(|| format!("fsync {}", tmp_path.display()))?;
        drop(file);

        tokio::fs::rename(&tmp_path, &final_path)
            .await
            .with_context(|| {
                format!("renaming {} → {}", tmp_path.display(), final_path.display())
            })?;

        // Force the directory entry (the rename) to disk. tokio::fs has no dir
        // sync, so open the dir with std on a blocking thread and sync_all it.
        let dir = self.root.clone();
        tokio::task::spawn_blocking(move || -> std::io::Result<()> {
            std::fs::File::open(&dir)?.sync_all()
        })
        .await?
        .with_context(|| format!("fsync dir {}", self.root.display()))?;

        Ok(())
    }
}

#[async_trait]
impl WorkflowStore for FileWorkflowStore {
    async fn create(&self, instance: WorkflowInstance) -> anyhow::Result<WorkflowInstance> {
        let _guard = self.write_lock.lock().await;
        if self.read_file(&instance.id).await?.is_some() {
            bail!("workflow id collision: {}", instance.id);
        }
        self.write_atomic(&instance).await?;
        Ok(instance)
    }

    async fn load(&self, workflow_id: &str) -> anyhow::Result<WorkflowInstance> {
        self.read_file(workflow_id)
            .await?
            .ok_or_else(|| anyhow!("workflow {} not found", workflow_id))
    }

    async fn save_if_version(
        &self,
        instance: WorkflowInstance,
        expected_version: u64,
    ) -> anyhow::Result<WorkflowInstance> {
        let _guard = self.write_lock.lock().await;
        match self.read_file(&instance.id).await? {
            Some(existing) if existing.version != expected_version => {
                bail!(
                    "stale workflow version: stored={}, expected={}",
                    existing.version,
                    expected_version
                );
            }
            None => bail!("workflow {} not found", instance.id),
            _ => {}
        }
        self.write_atomic(&instance).await?;
        Ok(instance)
    }

    /// SPEC §32 — scan stored instance files for one whose `run_id` matches.
    /// There is no secondary index on disk, so this is a linear scan of the
    /// store directory. Returns the matching instance's `id` so the runtime
    /// duplicate-run guard (RUN_ID_ALREADY_RUNNING) has teeth on file-backed
    /// deployments. Unreadable / unparseable files are skipped with a warning
    /// rather than aborting the scan — a single corrupt sidecar must not mask
    /// a real duplicate elsewhere, but the gap is made observable.
    async fn find_by_run_id(&self, run_id: &str) -> anyhow::Result<Option<String>> {
        let mut entries = match tokio::fs::read_dir(&self.root).await {
            Ok(rd) => rd,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(e) => return Err(anyhow!("scanning {}: {e}", self.root.display())),
        };
        while let Some(entry) = entries.next_entry().await? {
            let path = entry.path();
            if path.extension().and_then(|e| e.to_str()) != Some("json") {
                continue;
            }
            let bytes = match tokio::fs::read(&path).await {
                Ok(b) => b,
                Err(e) => {
                    tracing::warn!(path = %path.display(), error = %e,
                        "find_by_run_id: skipping unreadable instance file");
                    continue;
                }
            };
            let inst: WorkflowInstance = match serde_json::from_slice(&bytes) {
                Ok(i) => i,
                Err(e) => {
                    tracing::warn!(path = %path.display(), error = %e,
                        "find_by_run_id: skipping unparseable instance file");
                    continue;
                }
            };
            if inst.run_env.run_id.as_deref() == Some(run_id) {
                return Ok(Some(inst.id));
            }
        }
        Ok(None)
    }

    async fn list_waiting_on_lock(&self) -> anyhow::Result<Vec<WorkflowInstance>> {
        let mut entries = match tokio::fs::read_dir(&self.root).await {
            Ok(rd) => rd,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
            Err(e) => return Err(anyhow!("scanning {}: {e}", self.root.display())),
        };
        let mut waiting = Vec::new();
        while let Some(entry) = entries.next_entry().await? {
            let path = entry.path();
            if path.extension().and_then(|e| e.to_str()) != Some("json") {
                continue;
            }
            let bytes = match tokio::fs::read(&path).await {
                Ok(b) => b,
                Err(e) => {
                    tracing::warn!(path = %path.display(), error = %e,
                        "list_waiting_on_lock: skipping unreadable instance file");
                    continue;
                }
            };
            let inst: WorkflowInstance = match serde_json::from_slice(&bytes) {
                Ok(i) => i,
                Err(e) => {
                    tracing::warn!(path = %path.display(), error = %e,
                        "list_waiting_on_lock: skipping unparseable instance file");
                    continue;
                }
            };
            if inst.context.get("_lock_wait").is_some() {
                waiting.push(inst);
            }
        }
        Ok(waiting)
    }

    async fn list_waiting_on_subworkflow(&self) -> anyhow::Result<Vec<WorkflowInstance>> {
        let mut entries = match tokio::fs::read_dir(&self.root).await {
            Ok(rd) => rd,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
            Err(e) => return Err(anyhow!("scanning {}: {e}", self.root.display())),
        };
        let mut waiting = Vec::new();
        while let Some(entry) = entries.next_entry().await? {
            let path = entry.path();
            if path.extension().and_then(|e| e.to_str()) != Some("json") {
                continue;
            }
            let bytes = match tokio::fs::read(&path).await {
                Ok(b) => b,
                Err(e) => {
                    tracing::warn!(path = %path.display(), error = %e,
                        "list_waiting_on_subworkflow: skipping unreadable instance file");
                    continue;
                }
            };
            let inst: WorkflowInstance = match serde_json::from_slice(&bytes) {
                Ok(i) => i,
                Err(e) => {
                    tracing::warn!(path = %path.display(), error = %e,
                        "list_waiting_on_subworkflow: skipping unparseable instance file");
                    continue;
                }
            };
            if inst.context.get("_subworkflow_wait").is_some() {
                waiting.push(inst);
            }
        }
        Ok(waiting)
    }

    async fn list_all(&self) -> anyhow::Result<Vec<WorkflowInstance>> {
        let mut entries = match tokio::fs::read_dir(&self.root).await {
            Ok(rd) => rd,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
            Err(e) => return Err(anyhow!("scanning {}: {e}", self.root.display())),
        };
        let mut all = Vec::new();
        while let Some(entry) = entries.next_entry().await? {
            let path = entry.path();
            if path.extension().and_then(|e| e.to_str()) != Some("json") {
                continue;
            }
            let bytes = match tokio::fs::read(&path).await {
                Ok(b) => b,
                Err(e) => {
                    tracing::warn!(path = %path.display(), error = %e,
                        "list_all: skipping unreadable instance file");
                    continue;
                }
            };
            match serde_json::from_slice::<WorkflowInstance>(&bytes) {
                Ok(inst) => all.push(inst),
                Err(e) => {
                    tracing::warn!(path = %path.display(), error = %e,
                        "list_all: skipping unparseable instance file");
                }
            }
        }
        Ok(all)
    }
}
