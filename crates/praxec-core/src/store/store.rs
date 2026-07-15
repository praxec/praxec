use std::collections::HashMap;
use std::sync::{Arc, Mutex, RwLock};

use anyhow::{anyhow, bail};
use async_trait::async_trait;
use serde_json::Value;

use crate::model::{Evidence, WorkflowInstance};
use crate::ports::{DefinitionStore, EvidenceStore, WorkflowStore};
use crate::proxy_workflow::{DEFAULT_PROXY_WORKFLOW_ID, compile_proxy_workflow};

/// In-memory workflow store with optimistic concurrency on `version`.
///
/// Maintains two maps under separate `Mutex` guards:
/// - `inner`: primary index by `workflow_id`
/// - `by_run_id`: secondary index mapping `run_id → workflow_id`, populated
///   at `create` time when the instance carries a `run_id`. The `run_id` is
///   immutable after creation, so `save_if_version` never needs to update it.
#[derive(Clone)]
pub struct InMemoryWorkflowStore {
    inner: Arc<Mutex<HashMap<String, WorkflowInstance>>>,
    by_run_id: Arc<Mutex<HashMap<String, String>>>,
}

impl Default for InMemoryWorkflowStore {
    fn default() -> Self {
        Self {
            inner: Arc::new(Mutex::new(HashMap::new())),
            by_run_id: Arc::new(Mutex::new(HashMap::new())),
        }
    }
}

impl InMemoryWorkflowStore {
    pub fn new() -> Self {
        Self::default()
    }
}

#[async_trait]
impl WorkflowStore for InMemoryWorkflowStore {
    async fn create(&self, instance: WorkflowInstance) -> anyhow::Result<WorkflowInstance> {
        let mut g = self.inner.lock().expect("LOCK_POISONED: workflow store");
        if g.contains_key(&instance.id) {
            bail!("workflow id collision: {}", instance.id);
        }
        if let Some(rid) = &instance.run_env.run_id {
            // Hold the run_id index lock across the check AND the insert so the
            // uniqueness check is atomic: two concurrent creates with the same
            // run_id can't both pass the check (closing the start-path TOCTOU).
            let mut idx = self.by_run_id.lock().expect("LOCK_POISONED: run_id index");
            if idx.contains_key(rid) {
                bail!("RUN_ID_ALREADY_RUNNING: run_id already has an in-flight instance");
            }
            // Populate the secondary index before inserting into the primary map
            // so we never have a window where the primary map has the instance
            // but the index does not.
            idx.insert(rid.clone(), instance.id.clone());
        }
        g.insert(instance.id.clone(), instance.clone());
        Ok(instance)
    }

    async fn load(&self, workflow_id: &str) -> anyhow::Result<WorkflowInstance> {
        self.inner
            .lock()
            .expect("LOCK_POISONED: workflow store")
            .get(workflow_id)
            .cloned()
            .ok_or_else(|| anyhow!("workflow {} not found", workflow_id))
    }

    async fn list_waiting_on_lock(&self) -> anyhow::Result<Vec<WorkflowInstance>> {
        let g = self.inner.lock().expect("LOCK_POISONED: workflow store");
        Ok(g.values()
            .filter(|i| i.context.get("_lock_wait").is_some())
            .cloned()
            .collect())
    }

    async fn list_waiting_on_subworkflow(&self) -> anyhow::Result<Vec<WorkflowInstance>> {
        let g = self.inner.lock().expect("LOCK_POISONED: workflow store");
        Ok(g.values()
            .filter(|i| i.context.get("_subworkflow_wait").is_some())
            .cloned()
            .collect())
    }

    async fn list_all(&self) -> anyhow::Result<Vec<WorkflowInstance>> {
        let g = self.inner.lock().expect("LOCK_POISONED: workflow store");
        Ok(g.values().cloned().collect())
    }

    async fn save_if_version(
        &self,
        instance: WorkflowInstance,
        expected_version: u64,
    ) -> anyhow::Result<WorkflowInstance> {
        let mut g = self.inner.lock().expect("LOCK_POISONED: workflow store");
        match g.get(&instance.id) {
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
        g.insert(instance.id.clone(), instance.clone());
        Ok(instance)
    }

    async fn find_by_run_id(&self, run_id: &str) -> anyhow::Result<Option<String>> {
        Ok(self
            .by_run_id
            .lock()
            .expect("LOCK_POISONED: run_id index")
            .get(run_id)
            .cloned())
    }
}

/// Definition store backed by an in-memory map of workflow JSON values. Built
/// from a parsed gateway config: every `workflows.*` entry is registered, and
/// if the config has any `proxy.expose` entries, a `proxy_default` workflow is
/// compiled and registered.
/// Ensure a workflow definition carries a STRING `version` (SPEC §8.2).
///
/// The runtime stamps `definitionVersion` onto every transition record and
/// requires a string here (see `runtime::start`). The full config loader
/// already `or_insert`s a default, but definitions can also be materialized
/// straight through `ConfigDefinitionStore::from_config` (tests, embedded
/// callers) which bypasses that pass — so the store self-guarantees the
/// invariant: absent → `"0"`, numeric → stringified (never silently collapsed
/// to `"0"`, per the CMP-051 finding), string → left as-is.
fn stamp_definition_version(def: &mut Value) {
    let Some(obj) = def.as_object_mut() else {
        return;
    };
    match obj.get("version") {
        Some(Value::String(_)) => {}
        Some(Value::Number(n)) => {
            let s = n.to_string();
            obj.insert("version".to_string(), Value::String(s));
        }
        _ => {
            obj.insert("version".to_string(), Value::String("0".to_string()));
        }
    }
}

#[derive(Clone, Default)]
pub struct ConfigDefinitionStore {
    // Interior-mutable so the live snapshot can be hot-reloaded atomically.
    // The inner `Arc<HashMap>` is swapped wholesale; a `load()` clones the
    // current snapshot Arc under a brief read lock, so an in-flight reload never
    // tears a read. Clones of the store share the swap cell (the runtime holds
    // one `Arc<dyn DefinitionStore>` over this), so a reload through any handle
    // is visible everywhere.
    defs: Arc<std::sync::RwLock<Arc<HashMap<String, Value>>>>,
}

impl ConfigDefinitionStore {
    pub fn new(defs: HashMap<String, Value>) -> Self {
        Self {
            defs: Arc::new(std::sync::RwLock::new(Arc::new(defs))),
        }
    }

    /// Compile the `workflows.*` map (+ the proxy workflow) from a parsed
    /// gateway config into the loadable definition set. Pure: no validation,
    /// no install — `from_config`/`reload` decide whether to install it.
    fn build_defs(config: &Value) -> HashMap<String, Value> {
        let mut defs = HashMap::new();

        if let Some(map) = config.pointer("/workflows").and_then(Value::as_object) {
            for (id, def) in map {
                let mut def = def.clone();
                stamp_definition_version(&mut def);
                defs.insert(id.clone(), def);
            }
        }

        if let Some(mut proxy) = compile_proxy_workflow(config) {
            stamp_definition_version(&mut proxy);
            defs.insert(DEFAULT_PROXY_WORKFLOW_ID.to_string(), proxy);
        }

        defs
    }

    /// Build a definition store from a parsed gateway config Value.
    /// - registers every `workflows.<id>` definition
    /// - if `proxy.expose` is non-empty, compiles a `proxy_default` workflow
    pub fn from_config(config: &Value) -> Self {
        Self::new(Self::build_defs(config))
    }

    /// Hot-reload the live definition snapshot from a new gateway config —
    /// the "fix the JSON on the fly, no process restart" path. **Mint/reload
    /// gate:** the whole config is validated first and the swap happens ONLY if
    /// there are zero Error-level diagnostics; otherwise the current snapshot is
    /// left untouched and the full Error list is returned (fail-safe — a broken
    /// edit can never replace a working config, and "no execution until the
    /// issue is resolved" holds because the broken defs are never installed).
    /// Running instances are unaffected: each resolves from the snapshot it
    /// captured at start (SPEC §8.3), so only subsequent `start`s see the swap.
    pub fn reload(&self, config: &Value) -> Result<(), Vec<String>> {
        let errors: Vec<String> = crate::validate::validate_workflows(config)
            .into_iter()
            .filter(crate::validate::Diagnostic::is_error)
            .map(|d| d.message().to_string())
            .collect();
        if !errors.is_empty() {
            return Err(errors);
        }
        let next = Arc::new(Self::build_defs(config));
        *self
            .defs
            .write()
            .expect("LOCK_POISONED: config definition store") = next;
        Ok(())
    }

    pub fn ids(&self) -> Vec<String> {
        self.snapshot().keys().cloned().collect()
    }

    /// Clone the current snapshot Arc under a brief read lock.
    fn snapshot(&self) -> Arc<HashMap<String, Value>> {
        self.defs
            .read()
            .expect("LOCK_POISONED: config definition store")
            .clone()
    }
}

#[async_trait]
impl DefinitionStore for ConfigDefinitionStore {
    async fn load(&self, definition_id: &str) -> anyhow::Result<Value> {
        self.snapshot()
            .get(definition_id)
            .cloned()
            .ok_or_else(|| anyhow!("workflow definition '{}' not found", definition_id))
    }
}

/// SPEC §8.4 — in-memory writable definition store, intended for the
/// authoring workflow's `registry` executor when
/// `praxec.authoring.write_enabled` is true.
///
/// Audit-before-commit: `register` emits `definition.published` via the
/// supplied audit sink BEFORE the new snapshot becomes loadable. If audit
/// fails, the commit aborts and the new definition is NOT visible.
#[derive(Clone)]
pub struct InMemoryWritableDefinitionStore {
    inner: Arc<RwLock<HashMap<String, Value>>>,
    audit: Arc<dyn crate::audit::AuditSink>,
}

impl InMemoryWritableDefinitionStore {
    pub fn new(audit: Arc<dyn crate::audit::AuditSink>) -> Self {
        Self {
            inner: Arc::new(RwLock::new(HashMap::new())),
            audit,
        }
    }

    /// Seed the store with an existing definition map (e.g. the resolved
    /// config at startup). Useful for tests and bootstrap.
    pub fn with_seed(
        audit: Arc<dyn crate::audit::AuditSink>,
        seed: HashMap<String, Value>,
    ) -> Self {
        Self {
            inner: Arc::new(RwLock::new(seed)),
            audit,
        }
    }

    pub fn known_ids(&self) -> Vec<String> {
        self.inner
            .read()
            .expect("LOCK_POISONED: writable definition store")
            .keys()
            .cloned()
            .collect()
    }
}

#[async_trait]
impl DefinitionStore for InMemoryWritableDefinitionStore {
    async fn load(&self, definition_id: &str) -> anyhow::Result<Value> {
        self.inner
            .read()
            .expect("LOCK_POISONED: writable definition store")
            .get(definition_id)
            .cloned()
            .ok_or_else(|| anyhow!("workflow definition '{}' not found", definition_id))
    }
}

/// SPEC §5.9 — in-memory implementation of `GuidanceAcknowledgmentStore`.
/// Suitable for single-process gateways; replace with a persistent store
/// when ack must survive restarts.
#[derive(Default, Clone)]
pub struct InMemoryGuidanceAcknowledgmentStore {
    inner: Arc<RwLock<HashMap<(String, String), String>>>,
}

impl InMemoryGuidanceAcknowledgmentStore {
    pub fn new() -> Self {
        Self::default()
    }
}

#[async_trait]
impl crate::ports::GuidanceAcknowledgmentStore for InMemoryGuidanceAcknowledgmentStore {
    async fn record(
        &self,
        workflow_id: &str,
        subject: &str,
        body_hash: &str,
    ) -> anyhow::Result<()> {
        self.inner
            .write()
            .expect("LOCK_POISONED: guidance acknowledgment store")
            .insert(
                (workflow_id.to_string(), subject.to_string()),
                body_hash.to_string(),
            );
        Ok(())
    }

    async fn last_acknowledged_hash(
        &self,
        workflow_id: &str,
        subject: &str,
    ) -> anyhow::Result<Option<String>> {
        Ok(self
            .inner
            .read()
            .expect("LOCK_POISONED: guidance acknowledgment store")
            .get(&(workflow_id.to_string(), subject.to_string()))
            .cloned())
    }
}

/// SPEC §22 — in-memory implementation of `ScriptAcknowledgmentStore`.
/// Same shape as [`InMemoryGuidanceAcknowledgmentStore`] but in a distinct
/// keyspace so script acks don't pollute guidance acks (the two surfaces
/// can be wired independently).
#[derive(Default, Clone)]
pub struct InMemoryScriptAcknowledgmentStore {
    inner: Arc<RwLock<HashMap<(String, String), String>>>,
}

impl InMemoryScriptAcknowledgmentStore {
    pub fn new() -> Self {
        Self::default()
    }
}

#[async_trait]
impl crate::ports::ScriptAcknowledgmentStore for InMemoryScriptAcknowledgmentStore {
    async fn record(
        &self,
        workflow_id: &str,
        subject: &str,
        body_hash: &str,
    ) -> anyhow::Result<()> {
        self.inner
            .write()
            .expect("LOCK_POISONED: script acknowledgment store")
            .insert(
                (workflow_id.to_string(), subject.to_string()),
                body_hash.to_string(),
            );
        Ok(())
    }

    async fn last_acknowledged_hash(
        &self,
        workflow_id: &str,
        subject: &str,
    ) -> anyhow::Result<Option<String>> {
        Ok(self
            .inner
            .read()
            .expect("LOCK_POISONED: script acknowledgment store")
            .get(&(workflow_id.to_string(), subject.to_string()))
            .cloned())
    }
}

#[async_trait]
impl crate::ports::DefinitionStoreWritable for InMemoryWritableDefinitionStore {
    async fn register(
        &self,
        definition_id: &str,
        definition: Value,
        expected_prior_hash: Option<&str>,
    ) -> anyhow::Result<()> {
        // SPEC §8.4 optimistic concurrency: reject CONFLICT_STALE if the current
        // definition doesn't hash to the basis the author edited. Checked before
        // the audit + commit; `None` skips it (create-or-overwrite).
        if let Some(expected) = expected_prior_hash {
            let current_hash = {
                let guard = self
                    .inner
                    .read()
                    .expect("LOCK_POISONED: writable definition store");
                guard
                    .get(definition_id)
                    .map(crate::config::compute_definition_hash)
            };
            if current_hash.as_deref() != Some(expected) {
                anyhow::bail!(
                    "CONFLICT_STALE: '{definition_id}' has changed since you read it \
                     (expected {expected}, found {}). Re-read and re-apply your edit.",
                    current_hash.as_deref().unwrap_or("<absent>")
                );
            }
        }
        // Audit-before-commit (SPEC §8.4). If this fails, abort.
        let event =
            crate::audit::AuditEvent::new("definition.published").with_payload(serde_json::json!({
                "definitionId": definition_id,
                "outcome":      "pending_commit",
            }));
        if let Err(e) = self.audit.record(event).await {
            anyhow::bail!(
                "RECORD_WRITE_FAILED: audit of definition.published for '{definition_id}' failed: {e}"
            );
        }
        // Commit becomes visible only after audit succeeded.
        {
            let mut guard = self
                .inner
                .write()
                .expect("LOCK_POISONED: writable definition store");
            guard.insert(definition_id.to_string(), definition);
        }
        // Post-commit best-effort event (mirrors §5.8 non-critical semantics).
        // A self-event surfaces audit-write failure; we can't use
        // `record_or_self_event` here because that helper lives on
        // `WorkflowRuntime`, not on the store. Inline the pattern.
        let post =
            crate::audit::AuditEvent::new("definition.loadable").with_payload(serde_json::json!({
                "definitionId": definition_id,
                "outcome":      "loadable",
            }));
        if let Err(primary_err) = self.audit.record(post).await {
            let self_event = crate::audit::AuditEvent::new("audit.write_failed").with_payload(
                serde_json::json!({
                    "originalEvent": "definition.loadable",
                    "definitionId":  definition_id,
                    "error":         primary_err.to_string(),
                }),
            );
            if let Err(inner) = self.audit.record(self_event).await {
                tracing::warn!(
                    definition_id = %definition_id,
                    primary_err = %primary_err,
                    selfevt_err = %inner,
                    "non-critical definition.loadable audit failed; \
                     self-event also failed"
                );
            }
        }
        Ok(())
    }
}

/// In-memory evidence store. Each workflow id maps to its append-only list
/// of evidence records.
#[derive(Default, Clone)]
pub struct InMemoryEvidenceStore {
    inner: Arc<Mutex<HashMap<String, Vec<Evidence>>>>,
}

impl InMemoryEvidenceStore {
    pub fn new() -> Self {
        Self::default()
    }
}

#[async_trait]
impl EvidenceStore for InMemoryEvidenceStore {
    async fn record(&self, workflow_id: &str, evidence: Evidence) -> anyhow::Result<()> {
        let mut g = self.inner.lock().expect("LOCK_POISONED: evidence store");
        g.entry(workflow_id.to_string()).or_default().push(evidence);
        Ok(())
    }

    async fn list(&self, workflow_id: &str) -> anyhow::Result<Vec<Evidence>> {
        Ok(self
            .inner
            .lock()
            .expect("LOCK_POISONED: evidence store")
            .get(workflow_id)
            .cloned()
            .unwrap_or_default())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ports::WorkflowStore;
    use serde_json::json;

    fn make_instance(workflow_id: &str, run_id: Option<&str>) -> WorkflowInstance {
        WorkflowInstance {
            id: workflow_id.to_string(),
            definition_id: "demo".into(),
            definition_version: "1.0.0".into(),
            definition: serde_json::json!({"initialState": "pending", "states": {}}),
            state: "pending".into(),
            version: 0,
            input: serde_json::json!({}),
            context: serde_json::json!({}),
            started_at: chrono::Utc::now(),
            run_env: crate::RunEnv::new(
                crate::RepoRoot::for_test(),
                run_id.map(str::to_string),
                None,
            ),
            cancelled_at: None,
            cancelled_reason: None,
            depth: 0,
            parent: None,
        }
    }

    #[tokio::test]
    async fn find_by_run_id_returns_workflow_id_when_indexed() {
        let store = InMemoryWorkflowStore::default();
        let instance = make_instance("wf_test", Some("r-abc"));
        store.create(instance).await.unwrap();

        let found = store.find_by_run_id("r-abc").await.unwrap();
        assert_eq!(found.as_deref(), Some("wf_test"));
    }

    #[tokio::test]
    async fn find_by_run_id_returns_none_when_missing() {
        let store = InMemoryWorkflowStore::default();
        let found = store.find_by_run_id("r-not-there").await.unwrap();
        assert_eq!(found, None);
    }

    #[tokio::test]
    async fn find_by_run_id_returns_none_when_instance_has_no_run_id() {
        let store = InMemoryWorkflowStore::default();
        let instance = make_instance("wf_norunid", None);
        store.create(instance).await.unwrap();
        let found = store.find_by_run_id("anything").await.unwrap();
        assert_eq!(found, None);
    }

    #[tokio::test]
    async fn reload_with_a_clean_config_swaps_in_new_definitions() {
        let store = ConfigDefinitionStore::from_config(&json!({ "workflows": {} }));
        store
            .reload(&json!({
                "workflows": {
                    "fresh": {
                        "initialState": "s",
                        "states": { "s": { "terminal": true } }
                    }
                }
            }))
            .expect("a clean config must reload");
        assert!(
            store.load("fresh").await.is_ok(),
            "a definition added by hot-reload must become loadable without a restart"
        );
    }

    #[tokio::test]
    async fn reload_refuses_a_config_with_an_error_and_keeps_the_old_snapshot() {
        // Seed a working store, then attempt to reload a config whose only
        // workflow trips V23 (a string switch with no default). The reload must
        // be refused (Err) AND the prior good definition must remain loadable.
        let store = ConfigDefinitionStore::from_config(&json!({
            "workflows": { "good": { "initialState": "s", "states": { "s": { "terminal": true } } } }
        }));
        let result = store.reload(&json!({
            "workflows": { "broken": {
                "initialState": "gate",
                "states": {
                    "gate": { "transitions": {
                        "go": { "actor": "deterministic", "target": "nowhere" }
                    }},
                    "done": { "terminal": true }
                }
            }}
        }));
        assert!(
            result.is_err(),
            "a config with an Error diagnostic must be refused, not installed"
        );
    }

    #[tokio::test]
    async fn a_refused_reload_leaves_the_prior_definition_loadable() {
        let store = ConfigDefinitionStore::from_config(&json!({
            "workflows": { "good": { "initialState": "s", "states": { "s": { "terminal": true } } } }
        }));
        let _ = store.reload(&json!({
            "workflows": { "broken": {
                "initialState": "gate",
                "states": {
                    "gate": { "transitions": {
                        "go": { "actor": "deterministic", "target": "nowhere" }
                    }},
                    "done": { "terminal": true }
                }
            }}
        }));
        assert!(
            store.load("good").await.is_ok(),
            "a refused reload must leave the prior good snapshot intact (fail-safe swap)"
        );
    }
}
