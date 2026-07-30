//! pack-provenance-recording (P2) — `discovery::home()` surfaces a
//! `loaded_packs` section built from the SAME `/praxec/_packProvenance`
//! config stamp the gateway's `pack.provenance` audit event (P1) reads (see
//! `crates/praxec-core/src/config.rs`'s `stamp_pack_provenance` and
//! `crates/praxec/src/gateway.rs`'s `record_pack_provenance`).
//!
//! `build_discovery_index` is the one seam both gateway startup and reload
//! construct their index through, so exercising it directly here proves the
//! wiring without needing a full gateway boot.

use std::sync::Arc;

use async_trait::async_trait;
use serde_json::{Value, json};

use praxec_core::audit::MemoryAuditSink;
use praxec_core::discovery::build_discovery_index;
use praxec_core::embeddings::{EmbeddingError, EmbeddingProvider, NoopEmbedder};

fn config_with_provenance() -> Value {
    json!({
        "workflows": {},
        "praxec": {
            "_packProvenance": [
                {
                    "namespace": "acme",
                    "source": "/repos/acme",
                    "sha": "deadbeefdeadbeefdeadbeefdeadbeefdeadbeef",
                    "ref": "dev",
                    "dirty": false
                },
                {
                    "namespace": "plain",
                    "source": "/repos/plain"
                }
            ]
        }
    })
}

#[tokio::test]
async fn lexical_index_home_surfaces_loaded_packs_from_the_config_stamp() {
    let embedder: Arc<dyn EmbeddingProvider> = Arc::new(NoopEmbedder);
    let audit: Arc<dyn praxec_core::audit::AuditSink> = Arc::new(MemoryAuditSink::new());

    let index = build_discovery_index(&config_with_provenance(), None, &embedder, &audit)
        .await
        .expect("lexical index builds");

    let home = index.home().await.expect("home");
    let packs = home
        .get("loaded_packs")
        .and_then(Value::as_array)
        .expect("loaded_packs present");
    assert_eq!(packs.len(), 2);
    let acme = packs
        .iter()
        .find(|p| p.get("namespace").and_then(Value::as_str) == Some("acme"))
        .expect("acme record present");
    assert_eq!(
        acme.get("sha").and_then(Value::as_str),
        Some("deadbeefdeadbeefdeadbeefdeadbeefdeadbeef")
    );
    assert_eq!(acme.get("ref").and_then(Value::as_str), Some("dev"));
    assert_eq!(acme.get("dirty").and_then(Value::as_bool), Some(false));

    let plain = packs
        .iter()
        .find(|p| p.get("namespace").and_then(Value::as_str) == Some("plain"))
        .expect("plain record present");
    assert!(
        plain.get("sha").is_none(),
        "a non-git pack's absent fields must stay absent, not null-ified: {plain:?}"
    );

    // The base home() shape is untouched — same links/resource as ever.
    assert_eq!(home["resource"]["type"], "gateway");
    assert!(home["links"].as_array().is_some());
}

#[tokio::test]
async fn no_provenance_stamp_means_no_loaded_packs_key_at_all() {
    let embedder: Arc<dyn EmbeddingProvider> = Arc::new(NoopEmbedder);
    let audit: Arc<dyn praxec_core::audit::AuditSink> = Arc::new(MemoryAuditSink::new());

    let index = build_discovery_index(&json!({ "workflows": {} }), None, &embedder, &audit)
        .await
        .expect("index builds");

    let home = index.home().await.expect("home");
    assert!(
        home.get("loaded_packs").is_none(),
        "a host-only config with no repos: must read exactly as before this field existed"
    );
}

/// An embedder that always fails health — forces `build_discovery_index`'s
/// `degrade()` path, which must ALSO carry `loaded_packs` forward (provenance
/// is independent of whether the index ended up semantic or degraded-lexical).
struct AlwaysUnhealthyEmbedder;

#[async_trait]
impl EmbeddingProvider for AlwaysUnhealthyEmbedder {
    async fn embed(&self, _text: &str) -> Result<Vec<f32>, EmbeddingError> {
        Ok(vec![0.0])
    }
    async fn health_check(&self) -> Result<(), EmbeddingError> {
        Err(EmbeddingError::BackendFailed("down for this test".into()))
    }
    fn dimensions(&self) -> usize {
        1
    }
    fn backend_name(&self) -> &'static str {
        "always-unhealthy-fake"
    }
}

#[tokio::test]
async fn a_degraded_index_still_surfaces_loaded_packs() {
    let embedder: Arc<dyn EmbeddingProvider> = Arc::new(AlwaysUnhealthyEmbedder);
    let audit: Arc<dyn praxec_core::audit::AuditSink> = Arc::new(MemoryAuditSink::new());

    let index = build_discovery_index(&config_with_provenance(), None, &embedder, &audit)
        .await
        .expect("build completes (degrades to lexical, never errors)");

    let home = index.home().await.expect("home");
    let packs = home
        .get("loaded_packs")
        .and_then(Value::as_array)
        .expect("loaded_packs present even on a degraded (lexical-fallback) index");
    assert_eq!(packs.len(), 2);
}
