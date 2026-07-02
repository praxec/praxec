//! SPEC §30.10.10 — Optional Tier 3 semantic embedding provider.
//!
//! This module defines the `EmbeddingProvider` trait and two built-in
//! implementations:
//!
//! - [`NoopEmbedder`]: disabled state (backend = `none`). Returns empty vectors
//!   with 0 dimensions. The system operates fully on Tiers 1/2/4 when this
//!   is the active embedder.
//!
//! - [`HttpEmbedder`]: HTTP-based embedder supporting two request formats:
//!   - `ollama` — POST `{model, prompt}` → `{embedding: [f32, ...]}`
//!   - `openai_compatible` — POST `{model, input}` → `{data: [{embedding: [f32, ...]}]}`
//!
//! ## Config
//!
//! The `embeddings:` block in `praxec.yaml` (omitting it, or setting
//! `backend: none`) leaves the `NoopEmbedder` active. A configured backend:
//!
//! ```yaml
//! embeddings:
//!   backend: ollama            # or openai_compatible
//!   url: "http://localhost:11434/api/embeddings"
//!   model: nomic-embed-text
//!   dimensions: 768
//!   api_key_env: OPENAI_API_KEY   # optional; for openai_compatible
//! ```
//!
//! ## Cosine similarity threshold
//!
//! Tier 3 fires when cosine similarity ≥ 0.85 (configurable via
//! `EMBEDDING_COSINE_THRESHOLD`).

use async_trait::async_trait;
use serde_json::Value;
use thiserror::Error;

/// Cosine similarity threshold for Tier 3 semantic candidates.
pub const EMBEDDING_COSINE_THRESHOLD: f32 = 0.85;

/// Errors returned by an [`EmbeddingProvider`].
#[derive(Debug, Error)]
pub enum EmbeddingError {
    /// The HTTP backend returned a non-success status or a network error.
    #[error("EMBEDDING_BACKEND_FAILED: {0}")]
    BackendFailed(String),

    /// The response body from the HTTP backend could not be parsed.
    #[error("EMBEDDING_BACKEND_FAILED: failed to parse response: {0}")]
    ParseError(String),
}

/// Trait for computing text embeddings.
///
/// All implementations must be `Send + Sync` so they can be stored in an
/// `Arc<dyn EmbeddingProvider>` and shared across the runtime.
#[async_trait]
pub trait EmbeddingProvider: Send + Sync {
    /// Compute a dense embedding vector for `text`.
    ///
    /// Returns `Ok(vec![])` from [`NoopEmbedder`]; returns a float vector
    /// from HTTP backends.
    async fn embed(&self, text: &str) -> Result<Vec<f32>, EmbeddingError>;

    /// Dimensionality of the embedding space. `0` for [`NoopEmbedder`].
    fn dimensions(&self) -> usize;

    /// Human-readable backend identifier. One of `"noop"`, `"ollama"`,
    /// `"openai_compatible"`.
    fn backend_name(&self) -> &'static str;
}

// ── NoopEmbedder ─────────────────────────────────────────────────────────────

/// Always-disabled embedder. Active when `backend: none` (the default).
///
/// Returns empty vectors; Tier 3 candidate ranking is skipped automatically
/// when the vector is empty.
pub struct NoopEmbedder;

#[async_trait]
impl EmbeddingProvider for NoopEmbedder {
    async fn embed(&self, _text: &str) -> Result<Vec<f32>, EmbeddingError> {
        Ok(vec![])
    }

    fn dimensions(&self) -> usize {
        0
    }

    fn backend_name(&self) -> &'static str {
        "noop"
    }
}

// ── HttpEmbedder ─────────────────────────────────────────────────────────────

/// Wire format used when serialising the HTTP request body.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RequestFormat {
    /// Ollama API: `POST {model, prompt}` → `{embedding: [f32]}`.
    Ollama,
    /// OpenAI-compatible API: `POST {model, input}` → `{data: [{embedding: [f32]}]}`.
    OpenAiCompatible,
}

/// HTTP-based embedding backend.
pub struct HttpEmbedder {
    client: reqwest::Client,
    url: String,
    model: String,
    dimensions: usize,
    request_format: RequestFormat,
    /// Optional environment-variable name whose value is used as the
    /// `Authorization: Bearer <token>` header.
    api_key_env: Option<String>,
}

impl HttpEmbedder {
    /// Construct a new `HttpEmbedder`.
    ///
    /// `api_key_env` — if `Some("MY_KEY")`, the value of the `MY_KEY`
    /// environment variable is sent as `Authorization: Bearer <value>`.
    pub fn new(
        url: impl Into<String>,
        model: impl Into<String>,
        dimensions: usize,
        request_format: RequestFormat,
        api_key_env: Option<String>,
    ) -> Self {
        Self {
            client: reqwest::Client::new(),
            url: url.into(),
            model: model.into(),
            dimensions,
            request_format,
            api_key_env,
        }
    }

    /// Build a request to the configured backend.
    fn build_request_body(&self, text: &str) -> Value {
        match self.request_format {
            RequestFormat::Ollama => serde_json::json!({
                "model": self.model,
                "prompt": text,
            }),
            RequestFormat::OpenAiCompatible => serde_json::json!({
                "model": self.model,
                "input": text,
            }),
        }
    }

    /// Extract the embedding vector from the response body.
    ///
    /// CMP-024(a) — after extraction the vector length is asserted against the
    /// configured `dimensions`. A backend silently returning a vector of the
    /// wrong width (model mismatch, truncation) would corrupt cosine-similarity
    /// comparisons against stored vectors, so we fail fast naming both lengths.
    fn extract_embedding(&self, body: &Value) -> Result<Vec<f32>, EmbeddingError> {
        let vec = match self.request_format {
            RequestFormat::Ollama => {
                let arr = body
                    .get("embedding")
                    .and_then(Value::as_array)
                    .ok_or_else(|| {
                        EmbeddingError::ParseError(
                            "ollama response missing `embedding` array".to_string(),
                        )
                    })?;
                parse_f32_array(arr)?
            }
            RequestFormat::OpenAiCompatible => {
                let arr = body
                    .pointer("/data/0/embedding")
                    .and_then(Value::as_array)
                    .ok_or_else(|| {
                        EmbeddingError::ParseError(
                            "openai response missing `data[0].embedding` array".to_string(),
                        )
                    })?;
                parse_f32_array(arr)?
            }
        };

        if self.dimensions != 0 && vec.len() != self.dimensions {
            return Err(EmbeddingError::BackendFailed(format!(
                "embedding dimension mismatch: backend returned {} dimensions, \
                 configured `dimensions` is {}",
                vec.len(),
                self.dimensions
            )));
        }

        Ok(vec)
    }
}

fn parse_f32_array(arr: &[Value]) -> Result<Vec<f32>, EmbeddingError> {
    arr.iter()
        .map(|v| {
            v.as_f64().map(|f| f as f32).ok_or_else(|| {
                EmbeddingError::ParseError(format!("embedding element is not a number: {v}"))
            })
        })
        .collect()
}

#[async_trait]
impl EmbeddingProvider for HttpEmbedder {
    async fn embed(&self, text: &str) -> Result<Vec<f32>, EmbeddingError> {
        let body = self.build_request_body(text);

        let mut req = self.client.post(&self.url).json(&body);

        if let Some(ref env_var) = self.api_key_env {
            if let Ok(key_val) = std::env::var(env_var) {
                req = req.header("Authorization", format!("Bearer {key_val}"));
            }
        }

        let resp = req
            .send()
            .await
            .map_err(|e| EmbeddingError::BackendFailed(format!("request failed: {e}")))?;

        if !resp.status().is_success() {
            return Err(EmbeddingError::BackendFailed(format!(
                "HTTP {} from embedding backend",
                resp.status()
            )));
        }

        let response_body: Value = resp
            .json()
            .await
            .map_err(|e| EmbeddingError::ParseError(format!("invalid JSON: {e}")))?;

        self.extract_embedding(&response_body)
    }

    fn dimensions(&self) -> usize {
        self.dimensions
    }

    fn backend_name(&self) -> &'static str {
        match self.request_format {
            RequestFormat::Ollama => "ollama",
            RequestFormat::OpenAiCompatible => "openai_compatible",
        }
    }
}

// ── Config parsing ────────────────────────────────────────────────────────────

/// Parse the top-level `embeddings:` block from a gateway config value.
///
/// Returns `None` (equivalent to `NoopEmbedder`) when:
/// - the `embeddings:` block is absent
/// - `backend: none`
///
/// Returns `Some(HttpEmbedder)` for `backend: ollama` or
/// `backend: openai_compatible`.
///
/// Returns an error for an unrecognised `backend:` value or an invalid
/// `request_format:` value.
pub fn parse_embeddings_config(config: &Value) -> Result<Option<HttpEmbedder>, anyhow::Error> {
    let Some(block) = config.get("embeddings") else {
        return Ok(None); // block absent → noop
    };

    let backend = block
        .get("backend")
        .and_then(Value::as_str)
        .unwrap_or("none");

    if backend == "none" {
        return Ok(None);
    }

    let format = match backend {
        "ollama" => RequestFormat::Ollama,
        "openai_compatible" => RequestFormat::OpenAiCompatible,
        other => {
            anyhow::bail!(
                "INVALID_EMBEDDINGS_CONFIG: unknown backend '{other}'; \
                 supported: none | ollama | openai_compatible"
            );
        }
    };

    let url = block.get("url").and_then(Value::as_str).ok_or_else(|| {
        anyhow::anyhow!("INVALID_EMBEDDINGS_CONFIG: `url` is required for backend '{backend}'")
    })?;

    let model = block.get("model").and_then(Value::as_str).ok_or_else(|| {
        anyhow::anyhow!("INVALID_EMBEDDINGS_CONFIG: `model` is required for backend '{backend}'")
    })?;

    // A real backend with 0 (or missing) dimensions produces empty embedding
    // vectors, which makes `cosine_similarity` return 0.0 for *every* pair —
    // silently disabling Tier-3 semantic matching. Require a positive value.
    let dimensions = block
        .get("dimensions")
        .and_then(Value::as_u64)
        .filter(|&d| d > 0)
        .ok_or_else(|| {
            anyhow::anyhow!(
                "INVALID_EMBEDDINGS_CONFIG: `dimensions` is required and must be a \
                 positive integer for backend '{backend}'; a zero or missing value \
                 yields empty vectors and 0.0 cosine similarity on every comparison."
            )
        })? as usize;

    let api_key_env = block
        .get("api_key_env")
        .and_then(Value::as_str)
        .map(str::to_owned);

    Ok(Some(HttpEmbedder::new(
        url,
        model,
        dimensions,
        format,
        api_key_env,
    )))
}

// ── Cosine similarity ─────────────────────────────────────────────────────────

/// Compute cosine similarity between two equal-length vectors.
///
/// Returns `0.0` when either vector is zero-length or the norms are zero
/// (degenerate — never reached in practice for well-formed embeddings).
pub fn cosine_similarity(a: &[f32], b: &[f32]) -> f32 {
    if a.len() != b.len() || a.is_empty() {
        return 0.0;
    }
    let dot: f32 = a.iter().zip(b.iter()).map(|(x, y)| x * y).sum();
    let norm_a: f32 = a.iter().map(|x| x * x).sum::<f32>().sqrt();
    let norm_b: f32 = b.iter().map(|x| x * x).sum::<f32>().sqrt();
    if norm_a == 0.0 || norm_b == 0.0 {
        return 0.0;
    }
    dot / (norm_a * norm_b)
}

// ── Text to embed ──────────────────────────────────────────────────────────────

/// Build the text string to embed for a lexicon entry.
///
/// Format: `<canonical> <aliases joined by space> <definition_short> <definition_long>`.
/// Matches what the write path stores so query vectors are comparable.
pub fn entry_embed_text(
    canonical: &str,
    aliases: &[String],
    definition_short: &str,
    definition_long: Option<&str>,
) -> String {
    let mut parts: Vec<&str> = Vec::with_capacity(4 + aliases.len());
    parts.push(canonical);
    for alias in aliases {
        parts.push(alias.as_str());
    }
    parts.push(definition_short);
    if let Some(long) = definition_long {
        parts.push(long);
    }
    parts.join(" ")
}
