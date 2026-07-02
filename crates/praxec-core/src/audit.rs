use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use anyhow::Context as _;
use async_trait::async_trait;
use chrono::{DateTime, Datelike, IsoWeek, Utc};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use uuid::Uuid;

use crate::fs::{Filesystem, RealFilesystem};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AuditEvent {
    pub id: String,
    pub timestamp: DateTime<Utc>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub workflow_id: Option<String>,
    pub correlation_id: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub actor: Option<String>,
    pub event_type: String,
    pub payload: Value,
    /// SPEC §20.2 — caller-supplied trace id spanning multiple workflows in
    /// one logical operation (e.g. a CI build that launches N sub-workflows).
    /// Opaque to the gateway: written through unchanged. Default `None`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub trace_id: Option<String>,
    /// SPEC §20.2 — caller-supplied id for grouping related workflow
    /// instances (e.g. one model-evaluation run that exercises N workflows).
    /// Opaque to the gateway. Default `None`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub run_id: Option<String>,
}

impl AuditEvent {
    pub fn new(event_type: impl Into<String>) -> Self {
        Self {
            id: format!("evt_{}", Uuid::new_v4().simple()),
            timestamp: Utc::now(),
            workflow_id: None,
            correlation_id: format!("cor_{}", Uuid::new_v4().simple()),
            actor: None,
            event_type: event_type.into(),
            payload: json!({}),
            trace_id: None,
            run_id: None,
        }
    }

    pub fn with_workflow(mut self, workflow_id: impl Into<String>) -> Self {
        self.workflow_id = Some(workflow_id.into());
        self
    }

    pub fn with_correlation(mut self, correlation_id: impl Into<String>) -> Self {
        self.correlation_id = correlation_id.into();
        self
    }

    pub fn with_actor(mut self, actor: impl Into<String>) -> Self {
        self.actor = Some(actor.into());
        self
    }

    pub fn with_payload(mut self, payload: Value) -> Self {
        self.payload = payload;
        self
    }

    /// SPEC §20.2 — set the optional `trace_id` for hierarchical
    /// observability. Sinks include it when present, omit when None.
    pub fn with_trace_id(mut self, trace_id: impl Into<String>) -> Self {
        self.trace_id = Some(trace_id.into());
        self
    }

    /// SPEC §20.2 — set the optional `run_id` for grouping related
    /// workflow instances.
    pub fn with_run_id(mut self, run_id: impl Into<String>) -> Self {
        self.run_id = Some(run_id.into());
        self
    }
}

#[async_trait]
pub trait AuditSink: Send + Sync {
    async fn record(&self, event: AuditEvent) -> anyhow::Result<()>;

    /// Return all recorded events. Returns `None` if the sink doesn't
    /// support retrieval (stderr, null).
    async fn list_events(&self) -> Option<Vec<AuditEvent>> {
        None
    }

    /// VER-001 — like [`list_events`](Self::list_events), but distinguishes a
    /// genuine retrieval ERROR (I/O, permissions) from "this sink doesn't store
    /// events" (`Ok(None)`) and "stored but empty" (`Ok(Some(vec![]))`). An
    /// operator inspecting a human-in-the-loop approval queue must never see a
    /// read failure masquerade as an empty queue. The default delegates to
    /// `list_events` (correct for the infallible in-memory/stderr/null sinks);
    /// `FileAuditSink` overrides it to surface directory-read failures.
    async fn try_list_events(&self) -> anyhow::Result<Option<Vec<AuditEvent>>> {
        Ok(self.list_events().await)
    }
}

/// Drops every event. Useful as a default when audit isn't configured.
pub struct NullAuditSink;

#[async_trait]
impl AuditSink for NullAuditSink {
    async fn record(&self, _event: AuditEvent) -> anyhow::Result<()> {
        Ok(())
    }

    async fn list_events(&self) -> Option<Vec<AuditEvent>> {
        None
    }
}

/// Writes one JSON line per event to stderr. stdout is a structured channel in
/// the contexts this sink runs (the `serve` stdio MCP transport and the
/// one-shot `command` / `query` driver, whose stdout is the JSON response), so
/// audit narration goes to the diagnostic stream to avoid corrupting it. The
/// config value that selects this sink is `audit.sink: stderr`.
pub struct StderrAuditSink;

#[async_trait]
impl AuditSink for StderrAuditSink {
    async fn record(&self, event: AuditEvent) -> anyhow::Result<()> {
        let line = serde_json::to_string(&event)?;
        eprintln!("{line}");
        Ok(())
    }

    async fn list_events(&self) -> Option<Vec<AuditEvent>> {
        None
    }
}

/// Stores events in memory. Cheap, useful for tests and short-lived processes.
#[derive(Default, Clone)]
pub struct MemoryAuditSink {
    events: Arc<Mutex<Vec<AuditEvent>>>,
}

impl MemoryAuditSink {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn snapshot(&self) -> Vec<AuditEvent> {
        self.events
            .lock()
            .expect("LOCK_POISONED: audit event buffer")
            .clone()
    }

    pub fn event_types(&self) -> Vec<String> {
        self.events
            .lock()
            .expect("LOCK_POISONED: audit event buffer")
            .iter()
            .map(|e| e.event_type.clone())
            .collect()
    }

    pub fn clear(&self) {
        self.events
            .lock()
            .expect("LOCK_POISONED: audit event buffer")
            .clear();
    }
}

#[async_trait]
impl AuditSink for MemoryAuditSink {
    async fn record(&self, event: AuditEvent) -> anyhow::Result<()> {
        self.events
            .lock()
            .expect("LOCK_POISONED: audit event buffer")
            .push(event);
        Ok(())
    }

    async fn list_events(&self) -> Option<Vec<AuditEvent>> {
        Some(self.snapshot())
    }
}

// ---------------------------------------------------------------------------
// FileAuditSink — date-rotated, category-split file output
// ---------------------------------------------------------------------------

/// Controls the granularity of date-rotation for [`FileAuditSink`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum RotationInterval {
    /// Rotate once per calendar day. Stamp format: `YYYY-MM-DD`.
    #[default]
    Daily,
    /// Rotate once per clock hour. Stamp format: `YYYY-MM-DD-HH`.
    Hourly,
    /// Rotate once per ISO week. Stamp format: `YYYY-Www` (e.g. `2026-W03`).
    Weekly,
}

impl RotationInterval {
    /// Derive the stamp string for the given instant and interval.
    pub fn stamp(&self, at: DateTime<Utc>) -> String {
        match self {
            RotationInterval::Daily => at.format("%Y-%m-%d").to_string(),
            RotationInterval::Hourly => at.format("%Y-%m-%d-%H").to_string(),
            RotationInterval::Weekly => {
                let iw: IsoWeek = at.iso_week();
                format!("{:04}-W{:02}", iw.year(), iw.week())
            }
        }
    }
}

/// A clock function that returns the current time. Boxed so tests can inject
/// a deterministic implementation without spawning real timers.
type ClockFn = Box<dyn Fn() -> DateTime<Utc> + Send + Sync>;

/// Writes date-rotated, category-split NDJSON audit logs into a **directory**.
///
/// On each [`record`][AuditSink::record] call:
/// - The current time is obtained from the injected clock (defaults to
///   [`Utc::now`]).
/// - The date stamp is derived from the clock and the configured
///   [`RotationInterval`].
/// - Events whose `event_type == "workflow.transition"` are routed to
///   `{stamp}-transitions.log`; all other events go to `{stamp}-audit.log`.
/// - The parent directory is created if it does not already exist.
pub struct FileAuditSink {
    /// The directory into which rotated log files are written.
    dir: PathBuf,
    rotation: RotationInterval,
    clock: ClockFn,
    fs: Arc<dyn Filesystem>,
    lock: tokio::sync::Mutex<()>,
}

impl FileAuditSink {
    /// Create a sink that writes into `dir` with daily rotation and the system
    /// clock. Uses [`RealFilesystem`] for production I/O.
    pub fn new(dir: impl Into<PathBuf>, rotation: RotationInterval) -> Self {
        Self {
            dir: dir.into(),
            rotation,
            clock: Box::new(Utc::now),
            fs: Arc::new(RealFilesystem),
            lock: tokio::sync::Mutex::new(()),
        }
    }

    /// Create a sink with a custom clock **and** a custom filesystem. Intended
    /// for unit tests that want deterministic time and no real I/O.
    pub fn with_clock_and_fs(
        dir: impl Into<PathBuf>,
        rotation: RotationInterval,
        clock: ClockFn,
        fs: Arc<dyn Filesystem>,
    ) -> Self {
        Self {
            dir: dir.into(),
            rotation,
            clock,
            fs,
            lock: tokio::sync::Mutex::new(()),
        }
    }

    /// Derive the log file path for the given event at the current clock time.
    fn log_path(&self, event: &AuditEvent) -> PathBuf {
        let stamp = self.rotation.stamp((self.clock)());
        let category = if event.event_type == "workflow.transition" {
            "transitions"
        } else {
            "audit"
        };
        self.dir.join(format!("{stamp}-{category}.log"))
    }
}

#[async_trait]
impl AuditSink for FileAuditSink {
    async fn record(&self, event: AuditEvent) -> anyhow::Result<()> {
        let _guard = self.lock.lock().await;
        // Ensure the directory exists (create on first write rather than in
        // the constructor so tests that never record don't create empty dirs).
        self.fs.create_dir_all(&self.dir).await?;
        let path = self.log_path(&event);
        let mut line = serde_json::to_vec(&event)?;
        line.push(b'\n');
        // `Filesystem::append` flushes before returning Ok — durability is
        // preserved even though we no longer call tokio::fs directly.
        self.fs.append(&path, &line).await?;
        Ok(())
    }

    /// Best-effort retrieval — see [`Self::try_list_events`] for the fallible
    /// form. A directory-read failure collapses to `None` here (back-compat for
    /// callers that only want "the events, if any").
    async fn list_events(&self) -> Option<Vec<AuditEvent>> {
        self.try_list_events().await.ok().flatten()
    }

    /// VER-001/004 — read all events from every rotated `.log` file, ordered by
    /// `timestamp`. A genuine directory-read failure PROPAGATES (so the binary's
    /// `approvals list/resolve` can tell a real error from an empty queue);
    /// per-file/per-line corruption is logged and skipped (CMP-020). `Ok(None)`
    /// means the directory doesn't exist or holds no `.log` files.
    async fn try_list_events(&self) -> anyhow::Result<Option<Vec<AuditEvent>>> {
        // `Filesystem::read_dir` maps a missing directory to `Ok(vec![])`
        // (→ Ok(None) below), so any Err here is a genuine I/O failure that
        // must propagate rather than read as an empty queue.
        let all_paths = self
            .fs
            .read_dir(&self.dir)
            .await
            .with_context(|| format!("reading audit directory {}", self.dir.display()))?;
        let mut paths: Vec<PathBuf> = all_paths
            .into_iter()
            .filter(|p| p.extension().and_then(|e| e.to_str()) == Some("log"))
            .collect();
        paths.sort();

        if paths.is_empty() {
            return Ok(None);
        }

        let mut events: Vec<AuditEvent> = Vec::new();
        for path in paths {
            let content = match self.fs.read_to_string(&path).await {
                Ok(c) => c,
                // CMP-020 — an unreadable audit file is a gap in the trail.
                // Skip it so we still return what we can, but make the gap
                // observable rather than silent.
                Err(e) => {
                    tracing::warn!(
                        path = %path.display(),
                        error = %e,
                        "audit list_events: skipping unreadable log file — audit trail gap"
                    );
                    continue;
                }
            };
            for line in content.lines() {
                if line.trim().is_empty() {
                    continue;
                }
                match serde_json::from_str(line) {
                    Ok(e) => events.push(e),
                    // CMP-020 — an unparseable line is a dropped event.
                    Err(e) => {
                        tracing::warn!(
                            path = %path.display(),
                            error = %e,
                            "audit list_events: skipping unparseable log line — audit trail gap"
                        );
                    }
                }
            }
        }
        events.sort_by_key(|e| e.timestamp);
        Ok(Some(events))
    }
}
