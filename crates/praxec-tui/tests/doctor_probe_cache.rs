//! T23 — doctor's live-probe cache. Tests focus on cache round-trip
//! on disk (read/write/age), cache version mismatch handling,
//! distinct-bindings dedup, and per-provider probe skipping for local
//! providers. Live HTTP probing against real provider endpoints
//! requires network + credentials and is covered by the nightly
//! live-integration workflow, not unit tests.

use chrono::{Duration, Utc};
use praxec_core::model_resolver::{Binding, ModelsFile, Provider, ProviderFeatures};
use praxec_core::providers::ProviderId;
use praxec_tui::doctor_probe_cache::{
    BindingProbeRecord, ProbeCache, ProbeStatus, probe_binding, read_cache, refresh_cache,
    write_cache,
};
use std::time::Duration as StdDuration;

// ── cache round-trip ──────────────────────────────────────────────────────

#[test]
fn cache_roundtrip_through_disk() {
    let tmp = tempfile::tempdir().unwrap();
    let path = tmp.path().join("cache.json");
    let now = Utc::now();
    let cache = ProbeCache {
        version: ProbeCache::CURRENT_VERSION,
        last_written_at: now,
        entries: vec![BindingProbeRecord {
            provider: "anthropic".into(),
            model: "claude-sonnet-4-6".into(),
            probed_at: now,
            status: ProbeStatus::Ok,
            detail: "listed".into(),
        }],
    };
    write_cache(&cache, &path).unwrap();
    let read = read_cache(&path).unwrap().expect("cache exists");
    assert_eq!(read.version, ProbeCache::CURRENT_VERSION);
    assert_eq!(read.entries.len(), 1);
    assert_eq!(read.entries[0].status, ProbeStatus::Ok);
}

#[test]
fn cache_read_missing_file_returns_none() {
    let tmp = tempfile::tempdir().unwrap();
    let path = tmp.path().join("nope.json");
    assert!(read_cache(&path).unwrap().is_none());
}

#[test]
fn cache_read_version_mismatch_returns_none() {
    let tmp = tempfile::tempdir().unwrap();
    let path = tmp.path().join("cache.json");
    let bogus = serde_json::json!({
        "version": 999,
        "last_written_at": Utc::now(),
        "entries": []
    });
    std::fs::write(&path, serde_json::to_vec_pretty(&bogus).unwrap()).unwrap();
    // Future-version caches are tolerated by treating as empty so a
    // doctor upgrade doesn't crash on the operator's old cache.
    assert!(read_cache(&path).unwrap().is_none());
}

#[test]
fn cache_read_corrupt_json_returns_none() {
    let tmp = tempfile::tempdir().unwrap();
    let path = tmp.path().join("cache.json");
    std::fs::write(&path, "not json at all { ] [").unwrap();
    assert!(read_cache(&path).unwrap().is_none());
}

#[test]
fn cache_write_atomic_creates_parent_dir() {
    let tmp = tempfile::tempdir().unwrap();
    let path = tmp.path().join("nested/dir/cache.json");
    let cache = ProbeCache::empty();
    write_cache(&cache, &path).unwrap();
    assert!(path.exists());
}

#[test]
fn empty_cache_age_is_none() {
    let cache = ProbeCache::empty();
    assert!(cache.age().is_none(), "no entries → no meaningful age");
}

#[test]
fn cache_age_reports_elapsed_seconds() {
    let one_hour_ago = Utc::now() - Duration::hours(1);
    let cache = ProbeCache {
        version: ProbeCache::CURRENT_VERSION,
        last_written_at: one_hour_ago,
        entries: vec![BindingProbeRecord {
            provider: "anthropic".into(),
            model: "claude-sonnet-4-6".into(),
            probed_at: one_hour_ago,
            status: ProbeStatus::Ok,
            detail: "x".into(),
        }],
    };
    let age = cache.age().expect("age");
    // 3600s ± slack for the test process.
    assert!(age >= StdDuration::from_secs(3500));
    assert!(age <= StdDuration::from_secs(3700));
}

// ── distinct-bindings dedup via refresh_cache ─────────────────────────────

#[tokio::test]
async fn refresh_cache_dedupes_same_provider_model() {
    // Same (provider, model) appearing in default + override should
    // only produce one entry in the cache. Uses local providers so we
    // don't hit the network — they all return Skipped without I/O.
    let yaml = r#"
version: 1
default:
  - provider: { name: ollama }
    model: llama3
  - provider: { name: ollama }
    model: llama3
overrides:
  coding:
    - provider: { name: ollama }
      model: llama3
"#;
    let file = ModelsFile::from_yaml(yaml).expect("parses");
    let cache = refresh_cache(&file).await;
    assert_eq!(cache.entries.len(), 1, "duplicates collapse");
    assert_eq!(cache.entries[0].status, ProbeStatus::Skipped);
    assert_eq!(cache.entries[0].model, "llama3");
}

// ── per-provider probe behavior (no network) ──────────────────────────────

#[tokio::test]
async fn local_providers_are_skipped() {
    let client = reqwest::Client::new();
    for p in [
        Provider::Known(ProviderId::Ollama),
        Provider::Known(ProviderId::Llamacpp),
    ] {
        let b = Binding {
            provider: p.clone(),
            model: "any".into(),
            features: ProviderFeatures::None,
        };
        let (status, _detail) = probe_binding(&client, &b).await;
        assert_eq!(status, ProbeStatus::Skipped, "{:?} should skip", p);
    }
}

#[tokio::test]
async fn custom_provider_is_skipped() {
    let client = reqwest::Client::new();
    let b = Binding {
        provider: Provider::Custom {
            endpoint: "https://x.example".into(),
        },
        model: "any".into(),
        features: ProviderFeatures::None,
    };
    let (status, _) = probe_binding(&client, &b).await;
    assert_eq!(status, ProbeStatus::Skipped);
}

#[tokio::test]
async fn cloud_provider_without_credential_reports_no_credential() {
    // Make sure these are unset for this test — env mutations are
    // serial-unsafe with other env tests in the same file, so we keep
    // this one explicit + local.
    // FIXME: Audit that the environment access only happens in single-threaded code.
    unsafe { std::env::remove_var("ANTHROPIC_API_KEY") };
    let client = reqwest::Client::new();
    let b = Binding {
        provider: Provider::Known(ProviderId::Anthropic),
        model: "claude-sonnet-4-6".into(),
        features: ProviderFeatures::None,
    };
    let (status, detail) = probe_binding(&client, &b).await;
    assert_eq!(status, ProbeStatus::NoCredential);
    assert!(detail.contains("ANTHROPIC_API_KEY"));
}

#[tokio::test]
async fn openrouter_is_probed_not_skipped() {
    // PROBE-02 — OpenRouter has a live `/api/v1/models` listing, so it must
    // NOT be reported as Skipped (which reads as green/healthy and would mask
    // a dead key). Without a key it reports NoCredential — an honest
    // "unverified", never a passing Skipped. We assert it is no longer
    // Skipped; the no-key path drives the classification without network I/O.
    // FIXME: Audit that the environment access only happens in single-threaded code.
    unsafe { std::env::remove_var("OPENROUTER_API_KEY") };
    let client = reqwest::Client::new();
    let b = Binding {
        provider: Provider::Known(ProviderId::Openrouter),
        model: "openai/gpt-4o".into(),
        features: ProviderFeatures::None,
    };
    let (status, detail) = probe_binding(&client, &b).await;
    assert_ne!(
        status,
        ProbeStatus::Skipped,
        "OpenRouter must be probed (or reported unverified), never Skipped/green"
    );
    assert_eq!(status, ProbeStatus::NoCredential);
    assert!(detail.contains("OPENROUTER_API_KEY"));
}
