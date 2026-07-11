//! Unit tests for SPEC §30.10.10 — optional semantic embeddings (Tier 3).
//!
//! Tests cover:
//! 1. `NoopEmbedder` returns empty vector with 0 dimensions.
//! 2. `HttpEmbedder` ollama adapter sends `POST {model, prompt}` and parses `embedding`.
//! 3. `HttpEmbedder` openai_compatible adapter sends `POST {model, input}` and parses `data[0].embedding`.
//! 4. Config parse: missing `embeddings:` block → `None` (noop).
//!    Invalid `backend` value fails with an error.
//! 5. `build_entry` with non-empty vector stores `_embedding`.
//! 6. `build_entry` with `None` embedding stores no `_embedding`.
//! 7. Backend failure produces `EmbeddingError::BackendFailed`.
//! 8. Tier 3 candidate ranking: cosine ≥ 0.85 → `match_kind: "semantic"`.
//! 9. Tier 3 ordering: semantic appears before fuzzy in ranked list.
//! 10. Tier 3 skipped when embedder is `None`.

use std::net::SocketAddr;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::{Duration, Instant};

use async_trait::async_trait;
use praxec_core::{
    embeddings::{
        EMBEDDING_COSINE_THRESHOLD, EmbeddingError, EmbeddingProvider, HttpEmbedder, HttpPolicy,
        NoopEmbedder, RequestFormat, cosine_similarity, entry_embed_text, parse_embeddings_config,
    },
    lexicon::build_entry,
    lexicon_candidates::rank_candidates_with_embedding,
};
use serde_json::{Map, Value, json};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;

// ─────────────────────────────────────────────────────────────────────────────
// Stub EmbeddingProvider for Tier 3 tests (no HTTP required)
// ─────────────────────────────────────────────────────────────────────────────

/// Test stub: always returns the configured fixed vector regardless of input.
struct FixedVectorEmbedder {
    vector: Vec<f32>,
}

impl FixedVectorEmbedder {
    fn returning(vector: Vec<f32>) -> Self {
        Self { vector }
    }
}

#[async_trait]
impl EmbeddingProvider for FixedVectorEmbedder {
    async fn embed(&self, _text: &str) -> Result<Vec<f32>, EmbeddingError> {
        Ok(self.vector.clone())
    }

    async fn health_check(&self) -> Result<(), EmbeddingError> {
        Ok(())
    }

    fn dimensions(&self) -> usize {
        self.vector.len()
    }

    fn backend_name(&self) -> &'static str {
        "fixed"
    }
}

// ─────────────────────────────────────────────────────────────────────────────
// Helpers
// ─────────────────────────────────────────────────────────────────────────────

/// Build a lexicon map with one entry that already has a stored `_embedding`.
fn entry_with_embedding(term: &str, definition_short: &str, vec: Vec<f32>) -> Map<String, Value> {
    let mut m = Map::new();
    m.insert(
        term.to_string(),
        json!({
            "definition_short": definition_short,
            "governance": "human-only",
            "_embedding": vec,
        }),
    );
    m
}

/// Unit-vector in R^3 pointing along [1, 0, 0].
fn unit_x() -> Vec<f32> {
    vec![1.0_f32, 0.0, 0.0]
}

/// Unit-vector in R^3 pointing along [0, 1, 0] — orthogonal to unit_x.
fn unit_y() -> Vec<f32> {
    vec![0.0_f32, 1.0, 0.0]
}

/// Near-unit-x vector (cosine similarity ≈ 0.9994 with unit_x).
fn near_x() -> Vec<f32> {
    vec![0.9994_f32, 0.035, 0.0]
}

// ─────────────────────────────────────────────────────────────────────────────
// Minimal HTTP mock server (tokio TcpListener)
// ─────────────────────────────────────────────────────────────────────────────

/// A raw 200 response carrying `json`.
fn http_ok(json: &str) -> String {
    format!(
        "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{json}",
        json.len()
    )
}

/// A raw response with no body — for the error statuses.
fn http_status(code: u16, reason: &str) -> String {
    format!("HTTP/1.1 {code} {reason}\r\nContent-Length: 0\r\nConnection: close\r\n\r\n")
}

/// Spawn an HTTP server that serves `responses` in order (the last one repeating
/// forever), counting requests. The count is how retry behaviour is asserted —
/// deterministically, without leaning on wall-clock timing.
async fn scripted_http_server(responses: Vec<String>) -> (SocketAddr, Arc<AtomicUsize>) {
    assert!(!responses.is_empty(), "script at least one response");
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let hits = Arc::new(AtomicUsize::new(0));
    let counter = Arc::clone(&hits);
    tokio::spawn(async move {
        while let Ok((mut stream, _)) = listener.accept().await {
            let n = counter.fetch_add(1, Ordering::SeqCst);
            let body = responses[n.min(responses.len() - 1)].clone();
            let mut buf = [0u8; 4096];
            let _ = stream.read(&mut buf).await;
            let _ = stream.write_all(body.as_bytes()).await;
        }
    });
    (addr, hits)
}

/// Spawn a server that accepts the connection and then answers *nothing*, holding
/// it open — the flaky-endpoint shape that hangs a client with no timeout (the
/// v0.0.17 failure). Counts connections, so a retry would be visible.
async fn stalled_http_server() -> (SocketAddr, Arc<AtomicUsize>) {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let hits = Arc::new(AtomicUsize::new(0));
    let counter = Arc::clone(&hits);
    tokio::spawn(async move {
        // Streams are held, never answered: dropping them would send a FIN and let
        // the client fail early. Only the timeout may end this wait.
        let mut held = Vec::new();
        while let Ok((stream, _)) = listener.accept().await {
            counter.fetch_add(1, Ordering::SeqCst);
            held.push(stream);
        }
    });
    (addr, hits)
}

/// An address nothing is listening on: bind, learn the port, drop the listener.
async fn dead_port() -> SocketAddr {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    drop(listener);
    addr
}

/// Fast, deterministic bounds — CI must not wait out production-sized budgets.
fn fast_policy() -> HttpPolicy {
    HttpPolicy {
        connect_timeout: Duration::from_millis(500),
        request_timeout: Duration::from_millis(300),
        max_retries: 2,
        retry_backoff: Duration::from_millis(10),
    }
}

fn embedder_at(addr: SocketAddr, dimensions: usize, policy: HttpPolicy) -> HttpEmbedder {
    HttpEmbedder::with_policy(
        format!("http://{addr}/api/embeddings"),
        "nomic-embed-text",
        dimensions,
        RequestFormat::Ollama,
        None,
        policy,
    )
    .expect("client must build")
}

// ─────────────────────────────────────────────────────────────────────────────
// Test 1 — NoopEmbedder returns empty vector with 0 dimensions
// ─────────────────────────────────────────────────────────────────────────────

#[tokio::test]
async fn noop_embedder_returns_empty_vector() {
    let emb = NoopEmbedder;
    let result = emb.embed("hello").await.unwrap();
    assert!(
        result.is_empty(),
        "NoopEmbedder must return an empty vector"
    );
}

#[test]
fn noop_embedder_dimensions_is_zero() {
    let emb = NoopEmbedder;
    assert_eq!(emb.dimensions(), 0);
}

/// The noop embedder is trivially healthy — it embeds nothing, so there is no
/// backend that can be down. It must never be the thing that fails a health gate.
#[tokio::test]
async fn noop_embedder_health_check_is_ok() {
    NoopEmbedder
        .health_check()
        .await
        .expect("NoopEmbedder is always healthy");
}

// ─────────────────────────────────────────────────────────────────────────────
// Test 2 — HttpEmbedder ollama adapter
// ─────────────────────────────────────────────────────────────────────────────

#[tokio::test]
async fn http_embedder_ollama_parses_embedding_field() {
    let (addr, _) = scripted_http_server(vec![http_ok(r#"{"embedding":[0.1,0.2,0.3]}"#)]).await;
    let emb = embedder_at(addr, 3, fast_policy());
    let vec = emb.embed("test text").await.unwrap();
    assert_eq!(vec, vec![0.1_f32, 0.2_f32, 0.3_f32]);
}

// CMP-024(a) — a backend that returns a vector whose length disagrees with the
// configured `dimensions` must fail fast (BackendFailed) naming both lengths,
// not silently hand back a mis-sized vector that corrupts cosine similarity.
#[tokio::test]
async fn http_embedder_rejects_dimension_mismatch() {
    let (addr, hits) = scripted_http_server(vec![http_ok(r#"{"embedding":[0.1,0.2,0.3]}"#)]).await;
    // configured 4, backend returns 3
    let emb = embedder_at(addr, 4, fast_policy());
    let err = emb.embed("test text").await.unwrap_err();
    match err {
        EmbeddingError::BackendFailed(msg) => {
            assert!(msg.contains('3'), "should name returned length: {msg}");
            assert!(msg.contains('4'), "should name configured length: {msg}");
        }
        other => panic!("expected BackendFailed, got {other:?}"),
    }
    // A dimension mismatch is deterministic — the same backend will return the
    // same wrong width forever. Retrying it is pure latency, so we must not.
    assert_eq!(hits.load(Ordering::SeqCst), 1, "must not retry a mismatch");
}

// ─────────────────────────────────────────────────────────────────────────────
// Test 3 — HttpEmbedder openai_compatible adapter
// ─────────────────────────────────────────────────────────────────────────────

#[tokio::test]
async fn http_embedder_openai_compatible_parses_data_0_embedding() {
    let (addr, _) =
        scripted_http_server(vec![http_ok(r#"{"data":[{"embedding":[0.5,0.6,0.7]}]}"#)]).await;
    let emb = HttpEmbedder::with_policy(
        format!("http://{addr}/v1/embeddings"),
        "text-embedding-ada-002",
        3,
        RequestFormat::OpenAiCompatible,
        None,
        fast_policy(),
    )
    .expect("client must build");
    let vec = emb.embed("test text").await.unwrap();
    assert_eq!(vec, vec![0.5_f32, 0.6_f32, 0.7_f32]);
}

// ─────────────────────────────────────────────────────────────────────────────
// Test 4 — Config parse
// ─────────────────────────────────────────────────────────────────────────────

#[test]
fn config_parse_missing_embeddings_block_returns_none() {
    let config = json!({ "version": "1" });
    let result = parse_embeddings_config(&config).unwrap();
    assert!(
        result.is_none(),
        "missing embeddings block should yield None (noop)"
    );
}

#[test]
fn config_parse_backend_none_returns_none() {
    let config = json!({ "version": "1", "embeddings": { "backend": "none" } });
    let result = parse_embeddings_config(&config).unwrap();
    assert!(result.is_none(), "backend: none should yield None (noop)");
}

#[test]
fn config_parse_invalid_backend_fails() {
    let config = json!({ "version": "1", "embeddings": { "backend": "bogus_provider" } });
    // Map to () to avoid requiring Debug on Option<HttpEmbedder>.
    let err = parse_embeddings_config(&config).map(|_| ()).unwrap_err();
    let msg = format!("{err}");
    assert!(
        msg.contains("bogus_provider"),
        "error must mention the invalid backend: {msg}"
    );
}

#[test]
fn config_parse_ollama_backend_returns_some() {
    let config = json!({
        "version": "1",
        "embeddings": {
            "backend": "ollama",
            "url": "http://localhost:11434/api/embeddings",
            "model": "nomic-embed-text",
            "dimensions": 768
        }
    });
    let result = parse_embeddings_config(&config).unwrap();
    assert!(result.is_some(), "ollama backend should parse successfully");
    let emb = result.unwrap();
    assert_eq!(emb.backend_name(), "ollama");
    assert_eq!(emb.dimensions(), 768);
}

#[test]
fn config_parse_real_backend_missing_dimensions_fails() {
    // STUB-107 — a real backend with missing dimensions used to default to 0,
    // yielding empty vectors and 0.0 cosine on every comparison. Must fail loud.
    let config = json!({
        "version": "1",
        "embeddings": {
            "backend": "ollama",
            "url": "http://localhost:11434/api/embeddings",
            "model": "nomic-embed-text"
        }
    });
    let err = parse_embeddings_config(&config).map(|_| ()).unwrap_err();
    assert!(
        err.to_string().contains("dimensions"),
        "error must mention dimensions: {err}"
    );
}

#[test]
fn config_parse_real_backend_zero_dimensions_fails() {
    // STUB-107 — an explicit `dimensions: 0` is just as broken as a missing one.
    let config = json!({
        "version": "1",
        "embeddings": {
            "backend": "openai_compatible",
            "url": "http://localhost:1234/v1/embeddings",
            "model": "text-embedding-3-small",
            "dimensions": 0
        }
    });
    assert!(parse_embeddings_config(&config).map(|_| ()).is_err());
}

// ─────────────────────────────────────────────────────────────────────────────
// Test 5 — build_entry with non-None embedding stores _embedding
// ─────────────────────────────────────────────────────────────────────────────

#[test]
fn build_entry_with_embedding_stores_underscore_embedding_field() {
    let vec = vec![0.1_f32, 0.2_f32, 0.3_f32];
    let entry = build_entry("a real def", None, None, None, Some(vec.clone()))
        .expect("build_entry must succeed");
    let stored = entry
        .pointer("/_embedding")
        .and_then(Value::as_array)
        .expect("_embedding must be present");
    let parsed: Vec<f32> = stored.iter().map(|v| v.as_f64().unwrap() as f32).collect();
    assert_eq!(parsed, vec);
}

// ─────────────────────────────────────────────────────────────────────────────
// Test 6 — build_entry with None embedding stores no _embedding field
// ─────────────────────────────────────────────────────────────────────────────

#[test]
fn build_entry_with_none_embedding_stores_no_embedding_field() {
    let entry =
        build_entry("a real def", None, None, None, None).expect("build_entry must succeed");
    assert!(
        entry.pointer("/_embedding").is_none(),
        "_embedding must not be present when None is passed"
    );
}

// ─────────────────────────────────────────────────────────────────────────────
// Test 7 — Backend failure produces EmbeddingError::BackendFailed
// ─────────────────────────────────────────────────────────────────────────────

#[tokio::test]
async fn http_embedder_backend_failure_returns_backend_failed_error() {
    let (addr, _) = scripted_http_server(vec![http_status(500, "Internal Server Error")]).await;
    let emb = embedder_at(addr, 768, fast_policy());
    let err = emb.embed("test").await.expect_err("must fail on 500");
    assert!(
        matches!(err, EmbeddingError::BackendFailed(_)),
        "expected BackendFailed; got {err:?}"
    );
}

// ─────────────────────────────────────────────────────────────────────────────
// D2 — dependability: timeout, bounded retry, health probe
//
// The grounded defect: `HttpEmbedder` used to build `reqwest::Client::new()` with
// no timeout, so a flaky endpoint HUNG the caller instead of failing. These tests
// are what make that irreproducible.
// ─────────────────────────────────────────────────────────────────────────────

/// D2-T1 — a backend that accepts the connection and then never answers must
/// produce a typed `Timeout` inside the budget, not hang.
#[tokio::test]
async fn http_embedder_times_out_instead_of_hanging() {
    let (addr, hits) = stalled_http_server().await;
    let policy = fast_policy(); // request_timeout = 300ms
    let emb = embedder_at(addr, 3, policy);

    let started = Instant::now();
    let err = emb
        .embed("test")
        .await
        .expect_err("a stalled backend must fail");
    let elapsed = started.elapsed();

    match err {
        EmbeddingError::Timeout(after) => assert_eq!(after, policy.request_timeout),
        other => panic!("expected Timeout, got {other:?}"),
    }
    // Generous upper bound: the point is that it *terminates* — the pre-fix client
    // would still be waiting. Tight enough to catch a retry-multiplied wait.
    assert!(
        elapsed < Duration::from_secs(2),
        "must fail fast; took {elapsed:?}"
    );
    // A timeout is not retried: retrying would multiply the very latency the
    // timeout exists to bound.
    assert_eq!(
        hits.load(Ordering::SeqCst),
        1,
        "a timeout must not be retried"
    );
}

/// D2-T2 — a transient 5xx is retried, and the retry's success is returned.
#[tokio::test]
async fn http_embedder_retries_a_transient_5xx_then_succeeds() {
    let (addr, hits) = scripted_http_server(vec![
        http_status(503, "Service Unavailable"),
        http_ok(r#"{"embedding":[0.1,0.2,0.3]}"#),
    ])
    .await;
    let emb = embedder_at(addr, 3, fast_policy());

    let vec = emb.embed("test").await.expect("the retry must succeed");
    assert_eq!(vec, vec![0.1_f32, 0.2_f32, 0.3_f32]);
    assert_eq!(hits.load(Ordering::SeqCst), 2, "one failure, one retry");
}

/// D2-T2 — retries are bounded: a permanently-5xx backend fails after exactly
/// `1 + max_retries` attempts. No infinite loop.
#[tokio::test]
async fn http_embedder_bounds_retries_on_a_persistent_5xx() {
    let (addr, hits) = scripted_http_server(vec![http_status(500, "Internal Server Error")]).await;
    let policy = fast_policy(); // max_retries = 2
    let emb = embedder_at(addr, 3, policy);

    let err = emb
        .embed("test")
        .await
        .expect_err("persistent 500 must fail");
    assert!(
        err.to_string().contains("500"),
        "the last failure must be reported verbatim: {err}"
    );
    assert_eq!(
        hits.load(Ordering::SeqCst) as u32,
        1 + policy.max_retries,
        "attempts must be bounded by the policy"
    );
}

/// D2-T2 — a 4xx is a deterministic verdict (bad model, bad key, wrong URL).
/// Retrying only gets the same answer slower.
#[tokio::test]
async fn http_embedder_does_not_retry_a_4xx() {
    let (addr, hits) = scripted_http_server(vec![http_status(400, "Bad Request")]).await;
    let emb = embedder_at(addr, 3, fast_policy());

    let err = emb.embed("test").await.expect_err("400 must fail");
    assert!(
        matches!(err, EmbeddingError::BackendFailed(_)),
        "expected BackendFailed; got {err:?}"
    );
    assert_eq!(hits.load(Ordering::SeqCst), 1, "a 4xx must not be retried");
}

/// A 429 is the one 4xx that *is* transient — the backend is telling us to slow
/// down, not that the request is wrong.
#[tokio::test]
async fn http_embedder_retries_a_429() {
    let (addr, hits) = scripted_http_server(vec![
        http_status(429, "Too Many Requests"),
        http_ok(r#"{"embedding":[0.1,0.2,0.3]}"#),
    ])
    .await;
    let emb = embedder_at(addr, 3, fast_policy());

    emb.embed("test").await.expect("the retry must succeed");
    assert_eq!(hits.load(Ordering::SeqCst), 2, "429 is retryable");
}

/// D1-T2 / D2-T3 — the health probe answers Ok against a live backend.
#[tokio::test]
async fn health_check_succeeds_against_a_healthy_backend() {
    let (addr, hits) = scripted_http_server(vec![http_ok(r#"{"embedding":[0.1,0.2,0.3]}"#)]).await;
    let emb = embedder_at(addr, 3, fast_policy());

    emb.health_check().await.expect("a live backend is healthy");
    assert_eq!(
        hits.load(Ordering::SeqCst),
        1,
        "the probe is one round-trip"
    );
}

/// D2-T3 — the probe against a dead port fails fast with a diagnosable error.
/// Fail-fast, not a silent lexical downgrade: the embedder reports; the caller decides.
#[tokio::test]
async fn health_check_fails_fast_against_a_dead_port() {
    let addr = dead_port().await;
    let emb = embedder_at(addr, 3, fast_policy());

    let started = Instant::now();
    let err = emb
        .health_check()
        .await
        .expect_err("a dead port is not healthy");
    let elapsed = started.elapsed();

    match &err {
        EmbeddingError::HealthCheckFailed(msg) => {
            assert!(
                msg.contains(&addr.to_string()),
                "the error must name the endpoint it probed: {msg}"
            );
        }
        other => panic!("expected HealthCheckFailed, got {other:?}"),
    }
    assert!(
        elapsed < Duration::from_secs(3),
        "the probe must fail fast; took {elapsed:?}"
    );
}

/// The probe is a real embed, so it also proves the *dimension contract* — the
/// thing no connection-level ping can tell you. A backend that is reachable but
/// returns the wrong width is not healthy.
#[tokio::test]
async fn health_check_fails_on_a_dimension_mismatch() {
    let (addr, _) = scripted_http_server(vec![http_ok(r#"{"embedding":[0.1,0.2,0.3]}"#)]).await;
    let emb = embedder_at(addr, 768, fast_policy()); // backend returns 3

    let err = emb
        .health_check()
        .await
        .expect_err("a wrong-width backend is not healthy");
    assert!(
        err.to_string().contains("dimension mismatch"),
        "the probe must surface the contract breach: {err}"
    );
}

/// A stalled backend surfaces as a typed `Timeout` through the probe too — the
/// timeout is the most actionable thing we can report, so it is not re-wrapped.
#[tokio::test]
async fn health_check_times_out_on_a_stalled_backend() {
    let (addr, _) = stalled_http_server().await;
    let emb = embedder_at(addr, 3, fast_policy());

    let err = emb
        .health_check()
        .await
        .expect_err("a stalled backend is not healthy");
    assert!(
        matches!(err, EmbeddingError::Timeout(_)),
        "expected a typed Timeout; got {err:?}"
    );
}

// ─────────────────────────────────────────────────────────────────────────────
// D2 — the dependability knobs are config-wired (no knob that does nothing)
// ─────────────────────────────────────────────────────────────────────────────

fn embeddings_config(extra: Value) -> Value {
    let mut block = json!({
        "backend": "ollama",
        "url": "http://localhost:11434/api/embeddings",
        "model": "nomic-embed-text",
        "dimensions": 768
    });
    if let (Some(b), Some(e)) = (block.as_object_mut(), extra.as_object()) {
        for (k, v) in e {
            b.insert(k.clone(), v.clone());
        }
    }
    json!({ "version": "1", "embeddings": block })
}

#[test]
fn config_parse_applies_the_default_policy_when_knobs_are_absent() {
    let emb = parse_embeddings_config(&embeddings_config(json!({})))
        .unwrap()
        .expect("ollama backend parses");
    assert_eq!(emb.policy(), HttpPolicy::default());
    // The whole point: the default is never "no timeout".
    assert!(emb.policy().request_timeout > Duration::ZERO);
    assert!(emb.policy().connect_timeout > Duration::ZERO);
}

#[test]
fn config_parse_wires_every_policy_knob_through() {
    let emb = parse_embeddings_config(&embeddings_config(json!({
        "connect_timeout_ms": 250,
        "request_timeout_ms": 1500,
        "max_retries": 5,
        "retry_backoff_ms": 25
    })))
    .unwrap()
    .expect("ollama backend parses");
    assert_eq!(
        emb.policy(),
        HttpPolicy {
            connect_timeout: Duration::from_millis(250),
            request_timeout: Duration::from_millis(1500),
            max_retries: 5,
            retry_backoff: Duration::from_millis(25),
        }
    );
}

#[test]
fn config_parse_allows_zero_retries() {
    // 0 is meaningful (retry off) — unlike a 0 timeout, which is nonsense.
    let emb = parse_embeddings_config(&embeddings_config(json!({ "max_retries": 0 })))
        .unwrap()
        .expect("ollama backend parses");
    assert_eq!(emb.policy().max_retries, 0);
}

#[test]
fn config_parse_rejects_a_zero_timeout() {
    let err = parse_embeddings_config(&embeddings_config(json!({ "request_timeout_ms": 0 })))
        .map(|_| ())
        .unwrap_err();
    assert!(
        err.to_string().contains("request_timeout_ms"),
        "error must name the offending knob: {err}"
    );
}

#[test]
fn config_parse_rejects_a_malformed_timeout_rather_than_defaulting() {
    // Silently defaulting a typo'd timeout would restore the unbounded wait these
    // knobs exist to prevent — the failure must be loud.
    let err = parse_embeddings_config(&embeddings_config(json!({ "connect_timeout_ms": "soon" })))
        .map(|_| ())
        .unwrap_err();
    assert!(
        err.to_string().contains("connect_timeout_ms"),
        "error must name the offending knob: {err}"
    );
}

#[test]
fn config_parse_rejects_a_malformed_retry_count() {
    let err = parse_embeddings_config(&embeddings_config(json!({ "max_retries": -1 })))
        .map(|_| ())
        .unwrap_err();
    assert!(
        err.to_string().contains("max_retries"),
        "error must name the offending knob: {err}"
    );
}

// ─────────────────────────────────────────────────────────────────────────────
// Test 8 — Tier 3 candidate ranking: cosine ≥ 0.85 → match_kind: "semantic"
// ─────────────────────────────────────────────────────────────────────────────

#[tokio::test]
async fn tier3_ranks_semantically_similar_entry_with_match_kind_semantic() {
    // unknown subject embedding = unit_x.
    // "close-entry" has stored near_x (cosine ≈ 0.9994 ≥ 0.85) → semantic match.
    // "far-entry" has stored unit_y (cosine = 0.0 < 0.85) → no match.
    let mut lexicon: Map<String, Value> = Map::new();
    lexicon.insert(
        "close-entry".to_string(),
        json!({
            "definition_short": "Near the query.",
            "governance": "human-only",
            "_embedding": near_x(),
        }),
    );
    lexicon.insert(
        "far-entry".to_string(),
        json!({
            "definition_short": "Orthogonal to the query.",
            "governance": "human-only",
            "_embedding": unit_y(),
        }),
    );

    let embedder = FixedVectorEmbedder::returning(unit_x());
    let results = rank_candidates_with_embedding(
        "unknown-subject",
        &lexicon,
        None,
        Some(&embedder as &dyn EmbeddingProvider),
    )
    .await;

    let semantic: Vec<_> = results
        .iter()
        .filter(|c| c.match_kind == "semantic")
        .collect();
    assert_eq!(
        semantic.len(),
        1,
        "expected exactly one semantic candidate; got {results:?}"
    );
    assert_eq!(semantic[0].term, "close-entry");
}

#[tokio::test]
async fn tier3_does_not_surface_orthogonal_entry() {
    // unit_y is orthogonal to unit_x — cosine = 0.0 < 0.85.
    let lexicon = entry_with_embedding("far-entry", "Orthogonal.", unit_y());
    let embedder = FixedVectorEmbedder::returning(unit_x());
    let results = rank_candidates_with_embedding(
        "unknown-subject",
        &lexicon,
        None,
        Some(&embedder as &dyn EmbeddingProvider),
    )
    .await;

    assert!(
        results.iter().all(|c| c.match_kind != "semantic"),
        "orthogonal entry must not surface as semantic; got {results:?}"
    );
}

// CMP-024(b) — a stored `_embedding` containing a non-numeric element must NOT
// be silently shortened into a mis-sized vector. The entry is skipped entirely
// (not surfaced as a semantic candidate), so a corrupt vector never produces a
// meaningless similarity score.
#[tokio::test]
async fn tier3_skips_entry_with_corrupt_embedding_element() {
    let mut lexicon: Map<String, Value> = Map::new();
    // near_x would match semantically, but one element is a string → corrupt.
    let mut corrupt = near_x();
    let corrupt_arr: Vec<Value> = corrupt
        .drain(..)
        .enumerate()
        .map(|(i, f)| {
            if i == 1 {
                json!("not-a-number")
            } else {
                json!(f)
            }
        })
        .collect();
    lexicon.insert(
        "corrupt-entry".to_string(),
        json!({
            "definition_short": "Would match but vector is corrupt.",
            "governance": "human-only",
            "_embedding": corrupt_arr,
        }),
    );

    let embedder = FixedVectorEmbedder::returning(unit_x());
    let results = rank_candidates_with_embedding(
        "unknown-subject",
        &lexicon,
        None,
        Some(&embedder as &dyn EmbeddingProvider),
    )
    .await;

    assert!(
        results.iter().all(|c| c.match_kind != "semantic"),
        "entry with corrupt _embedding must be skipped, not silently shortened; got {results:?}"
    );
}

// ─────────────────────────────────────────────────────────────────────────────
// Test 9 — Tier 3 ordering: semantic appears before fuzzy
// ─────────────────────────────────────────────────────────────────────────────

#[tokio::test]
async fn tier3_semantic_appears_before_fuzzy_in_ranked_list() {
    // "xyzzy" is 1 edit from "xyzzz" → fuzzy_close.
    // "semantic-entry" has near_x stored; query is unit_x → semantic match.
    let mut lexicon: Map<String, Value> = Map::new();
    // Fuzzy-close entry (Tier 4).
    lexicon.insert(
        "xyzzy".to_string(),
        json!({
            "definition_short": "One edit away.",
            "governance": "human-only",
        }),
    );
    // Semantic entry (Tier 3).
    lexicon.insert(
        "semantic-entry".to_string(),
        json!({
            "definition_short": "Semantically similar.",
            "governance": "human-only",
            "_embedding": near_x(),
        }),
    );

    let embedder = FixedVectorEmbedder::returning(unit_x());
    let results = rank_candidates_with_embedding(
        "xyzzz",
        &lexicon,
        None,
        Some(&embedder as &dyn EmbeddingProvider),
    )
    .await;

    let semantic_pos = results.iter().position(|c| c.match_kind == "semantic");
    let fuzzy_pos = results
        .iter()
        .position(|c| c.match_kind == "fuzzy_close" || c.match_kind == "fuzzy_loose");

    // Both must be present.
    assert!(
        semantic_pos.is_some(),
        "semantic candidate must be present; got {results:?}"
    );
    assert!(
        fuzzy_pos.is_some(),
        "fuzzy candidate must be present; got {results:?}"
    );

    assert!(
        semantic_pos.unwrap() < fuzzy_pos.unwrap(),
        "semantic must appear before fuzzy; order: {results:?}"
    );
}

// ─────────────────────────────────────────────────────────────────────────────
// Test 10 — Tier 3 skipped when embedder is None
// ─────────────────────────────────────────────────────────────────────────────

#[tokio::test]
async fn tier3_skipped_when_embedder_is_none() {
    // Lexicon has one entry with a stored embedding.  Without an embedder
    // that entry is below any Levenshtein threshold for the query "unknown".
    let lexicon = entry_with_embedding("semantic-entry", "Has embedding.", near_x());
    let results = rank_candidates_with_embedding(
        "unknown-subject-zyx",
        &lexicon,
        None,
        None, // no embedder
    )
    .await;

    assert!(
        results.iter().all(|c| c.match_kind != "semantic"),
        "no semantic candidates expected when embedder is None; got {results:?}"
    );
}

// ─────────────────────────────────────────────────────────────────────────────
// Cosine similarity unit tests
// ─────────────────────────────────────────────────────────────────────────────

#[test]
fn cosine_identical_unit_vectors_is_one() {
    let v = unit_x();
    let sim = cosine_similarity(&v, &v);
    assert!(
        (sim - 1.0_f32).abs() < 1e-6,
        "identical unit vectors must have cosine 1.0; got {sim}"
    );
}

#[test]
fn cosine_orthogonal_vectors_is_zero() {
    let sim = cosine_similarity(&unit_x(), &unit_y());
    assert!(
        sim.abs() < 1e-6,
        "orthogonal vectors must have cosine 0.0; got {sim}"
    );
}

#[test]
fn cosine_threshold_is_point_85() {
    // Sanity-check the constant is the value SPEC requires.
    assert!(
        (EMBEDDING_COSINE_THRESHOLD - 0.85_f32).abs() < 1e-6,
        "threshold must be 0.85; got {EMBEDDING_COSINE_THRESHOLD}"
    );
}

#[test]
fn cosine_near_x_and_unit_x_exceeds_threshold() {
    let sim = cosine_similarity(&near_x(), &unit_x());
    assert!(
        sim >= EMBEDDING_COSINE_THRESHOLD,
        "near_x cosine with unit_x ({sim}) must be ≥ threshold {EMBEDDING_COSINE_THRESHOLD}"
    );
}

// ─────────────────────────────────────────────────────────────────────────────
// entry_embed_text helper
// ─────────────────────────────────────────────────────────────────────────────

#[test]
fn entry_embed_text_joins_all_parts_with_spaces() {
    let text = entry_embed_text(
        "canonical",
        &["alias1".to_string(), "alias2".to_string()],
        "short def",
        Some("long def"),
    );
    assert_eq!(text, "canonical alias1 alias2 short def long def");
}

#[test]
fn entry_embed_text_with_no_aliases_or_long_def() {
    let text = entry_embed_text("term", &[], "definition", None);
    assert_eq!(text, "term definition");
}
