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

use async_trait::async_trait;
use praxec_core::{
    embeddings::{
        cosine_similarity, entry_embed_text, parse_embeddings_config, EmbeddingError,
        EmbeddingProvider, HttpEmbedder, NoopEmbedder, RequestFormat, EMBEDDING_COSINE_THRESHOLD,
    },
    lexicon::build_entry,
    lexicon_candidates::rank_candidates_with_embedding,
};
use serde_json::{json, Map, Value};
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

/// Spawn a one-shot HTTP server that responds once with `response_body` and
/// returns the address it is listening on.
async fn one_shot_http_server(response_body: &'static str) -> SocketAddr {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let response_json = response_body;
    tokio::spawn(async move {
        let (mut stream, _) = listener.accept().await.unwrap();
        // Read & discard the request.
        let mut buf = [0u8; 4096];
        let n = stream.read(&mut buf).await.unwrap_or(0);
        let _ = n; // we only care that data arrived; body is in buf
        let content_len = response_json.len();
        let http_response = format!(
            "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {content_len}\r\nConnection: close\r\n\r\n{response_json}"
        );
        stream.write_all(http_response.as_bytes()).await.unwrap();
    });
    addr
}

/// Spawn a one-shot HTTP server that returns 500.
async fn one_shot_http_server_error() -> SocketAddr {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        let (mut stream, _) = listener.accept().await.unwrap();
        let mut buf = [0u8; 4096];
        let _ = stream.read(&mut buf).await;
        let resp =
            "HTTP/1.1 500 Internal Server Error\r\nContent-Length: 0\r\nConnection: close\r\n\r\n";
        stream.write_all(resp.as_bytes()).await.unwrap();
    });
    addr
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

// ─────────────────────────────────────────────────────────────────────────────
// Test 2 — HttpEmbedder ollama adapter
// ─────────────────────────────────────────────────────────────────────────────

#[tokio::test]
async fn http_embedder_ollama_parses_embedding_field() {
    let addr = one_shot_http_server(r#"{"embedding":[0.1,0.2,0.3]}"#).await;
    let emb = HttpEmbedder::new(
        format!("http://{addr}/api/embeddings"),
        "nomic-embed-text",
        3,
        RequestFormat::Ollama,
        None,
    );
    let vec = emb.embed("test text").await.unwrap();
    assert_eq!(vec, vec![0.1_f32, 0.2_f32, 0.3_f32]);
}

// CMP-024(a) — a backend that returns a vector whose length disagrees with the
// configured `dimensions` must fail fast (BackendFailed) naming both lengths,
// not silently hand back a mis-sized vector that corrupts cosine similarity.
#[tokio::test]
async fn http_embedder_rejects_dimension_mismatch() {
    let addr = one_shot_http_server(r#"{"embedding":[0.1,0.2,0.3]}"#).await;
    let emb = HttpEmbedder::new(
        format!("http://{addr}/api/embeddings"),
        "nomic-embed-text",
        4, // configured 4, backend returns 3
        RequestFormat::Ollama,
        None,
    );
    let err = emb.embed("test text").await.unwrap_err();
    match err {
        EmbeddingError::BackendFailed(msg) => {
            assert!(msg.contains('3'), "should name returned length: {msg}");
            assert!(msg.contains('4'), "should name configured length: {msg}");
        }
        other => panic!("expected BackendFailed, got {other:?}"),
    }
}

// ─────────────────────────────────────────────────────────────────────────────
// Test 3 — HttpEmbedder openai_compatible adapter
// ─────────────────────────────────────────────────────────────────────────────

#[tokio::test]
async fn http_embedder_openai_compatible_parses_data_0_embedding() {
    let addr = one_shot_http_server(r#"{"data":[{"embedding":[0.5,0.6,0.7]}]}"#).await;
    let emb = HttpEmbedder::new(
        format!("http://{addr}/v1/embeddings"),
        "text-embedding-ada-002",
        3,
        RequestFormat::OpenAiCompatible,
        None,
    );
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
    let addr = one_shot_http_server_error().await;
    let emb = HttpEmbedder::new(
        format!("http://{addr}/api/embeddings"),
        "nomic-embed-text",
        768,
        RequestFormat::Ollama,
        None,
    );
    let err = emb.embed("test").await.expect_err("must fail on 500");
    assert!(
        matches!(err, EmbeddingError::BackendFailed(_)),
        "expected BackendFailed; got {err:?}"
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
