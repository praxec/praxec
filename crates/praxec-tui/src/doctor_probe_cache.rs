//! T23 — models.yaml live-probe cache. `px doctor --refresh-agents`
//! re-probes every binding's provider `/v1/models` endpoint (or
//! equivalent for local providers), writes results to
//! `~/.praxec/agents-last-probe.json`, and surfaces stale-cache
//! warnings on subsequent doctor runs.
//!
//! The point: bindings that were valid at `models.yaml` write-time can
//! become invalid later — a model deprecated by the provider, a key
//! revoked, an endpoint moved. Eager preflight at workflow start
//! (`model_resolver::preflight`) catches credential-missing today, but
//! it doesn't probe the actual model listing. This cache gives
//! operators a "last known good" timestamp per binding so the doctor
//! can flag "you haven't re-probed in 14 days; the model might be gone."

use std::path::PathBuf;
use std::time::Duration;

use chrono::{DateTime, Utc};
use reqwest::Client;
use serde::{Deserialize, Serialize};

use praxec_core::model_resolver::{Binding, ModelsFile};

/// Per-binding probe record. Keyed (in the cache JSON) by
/// `<provider>:<model>` so cross-list duplicates (the same binding
/// listed in `default:` and `coding:`) share one entry.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct BindingProbeRecord {
    pub provider: String,
    pub model: String,
    pub probed_at: DateTime<Utc>,
    pub status: ProbeStatus,
    pub detail: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum ProbeStatus {
    /// Provider returned 200 and the model id was in the response.
    Ok,
    /// Provider returned 200 but the model id was NOT in the response.
    /// Strong signal the model is deprecated / renamed.
    ModelNotListed,
    /// Auth failed (401/403). Cache the failure so the next doctor run
    /// surfaces it.
    AuthFailed,
    /// Network-level failure (timeout, DNS, connection reset). No
    /// signal about the model itself.
    Unreachable,
    /// Provider's response shape wasn't what we expected. Likely a
    /// provider API change; needs investigation.
    UnexpectedResponse,
    /// No API key in env; the binding is unprobeable without one.
    NoCredential,
    /// Provider class (Ollama / llama.cpp / Bedrock / Custom) where we don't
    /// implement a probe. Skipped, not failed.
    Skipped,
}

/// Cache file shape. `version` lets future migrations gate on a known
/// schema; bumping it invalidates older caches without crashing.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct ProbeCache {
    pub version: u32,
    pub last_written_at: DateTime<Utc>,
    pub entries: Vec<BindingProbeRecord>,
}

impl ProbeCache {
    pub const CURRENT_VERSION: u32 = 1;

    pub fn empty() -> Self {
        Self {
            version: Self::CURRENT_VERSION,
            last_written_at: Utc::now(),
            entries: Vec::new(),
        }
    }

    /// Compute the age of the cache. `None` if the cache is empty —
    /// "no probe ever" is distinct from "stale by N days".
    pub fn age(&self) -> Option<Duration> {
        if self.entries.is_empty() {
            return None;
        }
        let now = Utc::now();
        (now - self.last_written_at).to_std().ok()
    }
}

/// Default cache path: `~/.praxec/agents-last-probe.json` — the single praxec
/// home dir (`dirs::home_dir().join(".praxec")`), consolidated with all other
/// on-disk state. Returns `None` when the platform doesn't expose a home dir
/// (rare; should never happen on the supported Linux/macOS/Windows trio).
pub fn default_cache_path() -> Option<PathBuf> {
    dirs::home_dir().map(|d| d.join(".praxec").join("agents-last-probe.json"))
}

/// Read the cache from disk. Missing file → `Ok(None)`; corrupt /
/// version-bumped file → `Ok(None)` with a logged warning so old
/// caches don't keep crashing doctor across upgrades.
pub fn read_cache(path: &std::path::Path) -> std::io::Result<Option<ProbeCache>> {
    if !path.exists() {
        return Ok(None);
    }
    let bytes = std::fs::read(path)?;
    match serde_json::from_slice::<ProbeCache>(&bytes) {
        Ok(c) if c.version == ProbeCache::CURRENT_VERSION => Ok(Some(c)),
        Ok(c) => {
            tracing::warn!(
                cache_path = %path.display(),
                cache_version = c.version,
                current = ProbeCache::CURRENT_VERSION,
                "probe cache version mismatch — treating as empty (re-run doctor --refresh-agents)"
            );
            Ok(None)
        }
        Err(e) => {
            tracing::warn!(
                cache_path = %path.display(),
                error = %e,
                "probe cache parse error — treating as empty (re-run doctor --refresh-agents)"
            );
            Ok(None)
        }
    }
}

/// Atomic write the cache to disk. Same tempfile + rename pattern as
/// `migrate::write_atomic`.
pub fn write_cache(cache: &ProbeCache, path: &std::path::Path) -> std::io::Result<()> {
    if let Some(parent) = path.parent() {
        if !parent.as_os_str().is_empty() && !parent.exists() {
            std::fs::create_dir_all(parent)?;
        }
    }
    let json = serde_json::to_vec_pretty(cache)
        .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e))?;
    let tmp = path.with_extension(format!("tmp.{}", std::process::id()));
    {
        use std::io::Write;
        let mut f = std::fs::File::create(&tmp)?;
        f.write_all(&json)?;
        f.sync_all()?;
    }
    std::fs::rename(&tmp, path)
}

/// Probe every distinct (provider, model) pair declared in
/// `models.yaml`. Returns a freshly-stamped cache. Per-binding
/// failures are recorded in the cache as their classifier outcome
/// rather than aborting the whole refresh — operators want to see the
/// state of EVERY binding, not just the first failure.
pub async fn refresh_cache(file: &ModelsFile) -> ProbeCache {
    let bindings = distinct_bindings(file);
    let client = build_client();
    let mut entries = Vec::with_capacity(bindings.len());
    for binding in bindings {
        let (status, detail) = probe_binding(&client, &binding).await;
        entries.push(BindingProbeRecord {
            provider: binding.provider.display_name().to_string(),
            model: binding.model.clone(),
            probed_at: Utc::now(),
            status,
            detail,
        });
    }
    ProbeCache {
        version: ProbeCache::CURRENT_VERSION,
        last_written_at: Utc::now(),
        entries,
    }
}

fn build_client() -> Client {
    Client::builder()
        .connect_timeout(Duration::from_secs(5))
        .timeout(Duration::from_secs(15))
        .build()
        .unwrap_or_else(|e| {
            // The default `Client::new()` drops our connect/request
            // timeouts, so a probe could hang indefinitely. Surface the
            // build failure instead of swallowing it (CMP-049).
            tracing::warn!(
                error = %e,
                "reqwest client build failed; falling back to default client \
                 WITHOUT configured connect/request timeouts"
            );
            Client::new()
        })
}

/// Distinct bindings from every list (default + overrides), dedup'd
/// by (provider, model). The probe doesn't care which list a binding
/// lives in — it's per-(provider, model).
fn distinct_bindings(file: &ModelsFile) -> Vec<Binding> {
    use std::collections::BTreeSet;
    let mut seen: BTreeSet<(String, String)> = BTreeSet::new();
    let mut out = Vec::new();
    let mut consider = |b: &Binding, out: &mut Vec<Binding>| {
        let key = (b.provider.display_name().to_string(), b.model.clone());
        if seen.insert(key) {
            out.push(b.clone());
        }
    };
    for b in &file.default {
        consider(b, &mut out);
    }
    for list in file.overrides.values() {
        for b in list {
            consider(b, &mut out);
        }
    }
    out
}

/// Single-binding live probe. The per-provider strategy (base URLs, auth
/// headers, `/models` endpoints, model-presence checks) lives in
/// `praxec_core::model_resolver::provider_probe` — the single source
/// of truth shared with startup preflight. This is a thin delegate that
/// maps the core `ListingStatus` onto the doctor's persisted `ProbeStatus`.
pub async fn probe_binding(client: &Client, binding: &Binding) -> (ProbeStatus, String) {
    let (status, detail) =
        praxec_core::model_resolver::provider_probe::probe_binding(client, binding).await;
    (status.into(), detail)
}

impl From<praxec_core::model_resolver::provider_probe::ListingStatus> for ProbeStatus {
    fn from(s: praxec_core::model_resolver::provider_probe::ListingStatus) -> Self {
        use praxec_core::model_resolver::provider_probe::ListingStatus as L;
        match s {
            L::Ok => ProbeStatus::Ok,
            L::ModelNotListed => ProbeStatus::ModelNotListed,
            L::AuthFailed => ProbeStatus::AuthFailed,
            L::Unreachable => ProbeStatus::Unreachable,
            L::UnexpectedResponse => ProbeStatus::UnexpectedResponse,
            L::NoCredential => ProbeStatus::NoCredential,
            L::Skipped => ProbeStatus::Skipped,
        }
    }
}
