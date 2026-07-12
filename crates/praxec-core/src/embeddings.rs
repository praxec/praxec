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
//!   connect_timeout_ms: 2000      # optional; see HttpPolicy
//!   request_timeout_ms: 10000     # optional
//!   max_retries: 2                # optional; 0 disables retry
//!   retry_backoff_ms: 100         # optional
//! ```
//!
//! ## Cosine similarity threshold
//!
//! Tier 3 fires when cosine similarity ≥ 0.85 (configurable via
//! `EMBEDDING_COSINE_THRESHOLD`).

use std::time::Duration;

use async_trait::async_trait;
use serde_json::Value;
use thiserror::Error;

/// Cosine similarity threshold for Tier 3 semantic candidates.
pub const EMBEDDING_COSINE_THRESHOLD: f32 = 0.85;

/// The text a [`EmbeddingProvider::health_check`] embeds to prove the backend is
/// answering. Short and fixed so the probe is cheap and its cost is predictable.
pub const HEALTH_PROBE_TEXT: &str = "praxec embedding health probe";

/// Errors returned by an [`EmbeddingProvider`].
#[derive(Debug, Error)]
pub enum EmbeddingError {
    /// The HTTP backend returned a non-success status or a network error.
    #[error("EMBEDDING_BACKEND_FAILED: {0}")]
    BackendFailed(String),

    /// The response body from the HTTP backend could not be parsed.
    #[error("EMBEDDING_BACKEND_FAILED: failed to parse response: {0}")]
    ParseError(String),

    /// The backend did not answer inside its configured budget. Distinct from
    /// [`Self::BackendFailed`] because a slow endpoint and a broken one call for
    /// different operator action — and because a client with no timeout at all is
    /// the defect this variant exists to make impossible to reintroduce silently.
    #[error("EMBEDDING_BACKEND_FAILED: timed out after {0:?} — endpoint slow or unreachable")]
    Timeout(Duration),

    /// The health probe could not complete. Carries the underlying reason.
    #[error("EMBEDDING_BACKEND_FAILED: health check failed: {0}")]
    HealthCheckFailed(String),
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

    /// Prove the backend can answer *now*: a real round-trip, not a config check.
    ///
    /// Deliberately has no default body. A default of `Ok(())` would let a
    /// provider that has never been probed report itself healthy — which is
    /// precisely the lazy, discovered-at-first-query failure the contract exists
    /// to eliminate. Every implementation states its own answer.
    async fn health_check(&self) -> Result<(), EmbeddingError>;

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

    /// Trivially healthy: there is no backend to be unreachable. It embeds
    /// nothing and never claims to.
    async fn health_check(&self) -> Result<(), EmbeddingError> {
        Ok(())
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

/// Default connect budget: a reachable endpoint accepts a TCP connection in
/// milliseconds; two seconds is already generous for one that is merely slow.
pub const DEFAULT_CONNECT_TIMEOUT: Duration = Duration::from_secs(2);
/// Default whole-request budget. Embedding a short description is sub-second on
/// every catalogued backend; ten seconds bounds a stall without tripping on a
/// cold local model load.
pub const DEFAULT_REQUEST_TIMEOUT: Duration = Duration::from_secs(10);
/// Default retry budget for *transient* failures (network blip, 5xx, 429).
pub const DEFAULT_MAX_RETRIES: u32 = 2;
/// Default backoff, multiplied by the attempt number (100ms, 200ms, …).
pub const DEFAULT_RETRY_BACKOFF: Duration = Duration::from_millis(100);

/// Dependability budget for [`HttpEmbedder`]: what "fail fast" means in numbers.
///
/// A client with no timeout does not fail — it hangs, taking the caller with it.
/// That is the failure that got embeddings cut from v0.0.17, so both timeouts are
/// mandatory (there is no "unset" state) and both are set on every client we build.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct HttpPolicy {
    /// Budget for establishing the TCP/TLS connection.
    pub connect_timeout: Duration,
    /// Budget for the whole request, connect included.
    pub request_timeout: Duration,
    /// Extra attempts after the first, for transient failures only. `0` = none.
    pub max_retries: u32,
    /// Backoff before retry N, multiplied by N.
    pub retry_backoff: Duration,
}

impl Default for HttpPolicy {
    fn default() -> Self {
        Self {
            connect_timeout: DEFAULT_CONNECT_TIMEOUT,
            request_timeout: DEFAULT_REQUEST_TIMEOUT,
            max_retries: DEFAULT_MAX_RETRIES,
            retry_backoff: DEFAULT_RETRY_BACKOFF,
        }
    }
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
    policy: HttpPolicy,
}

/// How one HTTP attempt ended — the retry decision, made once, at the point where
/// we still know *why* it failed.
enum Attempt {
    Ok(Vec<f32>),
    /// Worth another attempt: the endpoint may answer differently next time.
    Transient(EmbeddingError),
    /// Deterministic (4xx, unparseable body, wrong dimensions, timeout). Retrying
    /// cannot change the answer; it only burns the latency budget.
    Fatal(EmbeddingError),
}

impl HttpEmbedder {
    /// Construct a new `HttpEmbedder` with the default [`HttpPolicy`].
    ///
    /// `api_key_env` — if `Some("MY_KEY")`, the value of the `MY_KEY`
    /// environment variable is sent as `Authorization: Bearer <value>`.
    pub fn new(
        url: impl Into<String>,
        model: impl Into<String>,
        dimensions: usize,
        request_format: RequestFormat,
        api_key_env: Option<String>,
    ) -> Result<Self, EmbeddingError> {
        Self::with_policy(
            url,
            model,
            dimensions,
            request_format,
            api_key_env,
            HttpPolicy::default(),
        )
    }

    /// Construct a new `HttpEmbedder` with an explicit dependability budget.
    ///
    /// Fallible because the client is built with timeouts applied: a TLS backend
    /// that cannot initialise is a startup failure, not something to paper over
    /// with a timeout-less client.
    pub fn with_policy(
        url: impl Into<String>,
        model: impl Into<String>,
        dimensions: usize,
        request_format: RequestFormat,
        api_key_env: Option<String>,
        policy: HttpPolicy,
    ) -> Result<Self, EmbeddingError> {
        let client = reqwest::Client::builder()
            .connect_timeout(policy.connect_timeout)
            .timeout(policy.request_timeout)
            .build()
            .map_err(|e| {
                EmbeddingError::BackendFailed(format!("could not build HTTP client: {e}"))
            })?;
        Ok(Self {
            client,
            url: url.into(),
            model: model.into(),
            dimensions,
            request_format,
            api_key_env,
            policy,
        })
    }

    /// The dependability budget this embedder runs under.
    pub fn policy(&self) -> HttpPolicy {
        self.policy
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

impl HttpEmbedder {
    /// One request/response round-trip, classified for the retry loop.
    async fn attempt_embed(&self, text: &str) -> Attempt {
        let body = self.build_request_body(text);
        let mut req = self.client.post(&self.url).json(&body);

        if let Some(ref env_var) = self.api_key_env {
            if let Ok(key_val) = std::env::var(env_var) {
                req = req.header("Authorization", format!("Bearer {key_val}"));
            }
        }

        let resp = match req.send().await {
            Ok(r) => r,
            // A timeout is FATAL, not transient: the timeout is the latency bound
            // the caller was promised, and retrying would multiply the very wait it
            // exists to cap. The endpoint is slow — say so, immediately.
            Err(e) if e.is_timeout() => return Attempt::Fatal(self.timeout_error(&e)),
            // Connection refused / reset / DNS blip — the classic transient.
            Err(e) => {
                return Attempt::Transient(EmbeddingError::BackendFailed(format!(
                    "request failed: {e}"
                )));
            }
        };

        let status = resp.status();
        if !status.is_success() {
            let err = EmbeddingError::BackendFailed(format!(
                "HTTP {status} from embedding backend at {}",
                self.url
            ));
            // 5xx / 429: the backend is overloaded or restarting — try again.
            // Any other 4xx is a deterministic verdict on THIS request (bad model,
            // bad key, wrong URL); repeating it just gets the same 4xx slower.
            return if status.is_server_error() || status == reqwest::StatusCode::TOO_MANY_REQUESTS {
                Attempt::Transient(err)
            } else {
                Attempt::Fatal(err)
            };
        }

        let response_body: Value = match resp.json().await {
            Ok(b) => b,
            // Headers arrived, the body stalled — still the timeout budget.
            Err(e) if e.is_timeout() => return Attempt::Fatal(self.timeout_error(&e)),
            Err(e) => {
                return Attempt::Fatal(EmbeddingError::ParseError(format!("invalid JSON: {e}")));
            }
        };

        // Wrong shape or wrong width (CMP-024(a)) is a property of the backend's
        // configuration, not of the moment — never retried.
        match self.extract_embedding(&response_body) {
            Ok(vec) => Attempt::Ok(vec),
            Err(e) => Attempt::Fatal(e),
        }
    }

    /// Name the budget that was actually exceeded, so the operator knows which
    /// knob to turn.
    fn timeout_error(&self, e: &reqwest::Error) -> EmbeddingError {
        EmbeddingError::Timeout(if e.is_connect() {
            self.policy.connect_timeout
        } else {
            self.policy.request_timeout
        })
    }
}

#[async_trait]
impl EmbeddingProvider for HttpEmbedder {
    async fn embed(&self, text: &str) -> Result<Vec<f32>, EmbeddingError> {
        let mut attempt: u32 = 0;
        loop {
            match self.attempt_embed(text).await {
                Attempt::Ok(vec) => return Ok(vec),
                Attempt::Fatal(e) => return Err(e),
                Attempt::Transient(e) => {
                    // Bounded by construction: the loop can only exit through a
                    // return, and the counter only rises.
                    if attempt >= self.policy.max_retries {
                        return Err(e);
                    }
                    attempt += 1;
                    tokio::time::sleep(self.policy.retry_backoff * attempt).await;
                }
            }
        }
    }

    /// Embed a fixed short string against the live backend. This is the strongest
    /// cheap probe available: it proves the endpoint is reachable, authenticated,
    /// answering in a parseable shape, AND returning the configured width — the
    /// last of which no connection-level ping can tell you.
    async fn health_check(&self) -> Result<(), EmbeddingError> {
        match self.embed(HEALTH_PROBE_TEXT).await {
            Ok(_) => Ok(()),
            // A timeout is already the most actionable thing we can report;
            // re-wrapping it would bury the one signal the operator needs.
            Err(e @ EmbeddingError::Timeout(_)) => Err(e),
            Err(e) => Err(EmbeddingError::HealthCheckFailed(format!(
                "{} at {}: {e}",
                self.backend_name(),
                self.url
            ))),
        }
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
/// Returns an error for an unrecognised `backend:` value, an invalid
/// `request_format:` value, or a malformed dependability knob (see [`HttpPolicy`]).
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

    let defaults = HttpPolicy::default();
    let policy = HttpPolicy {
        connect_timeout: parse_millis(block, "connect_timeout_ms", defaults.connect_timeout)?,
        request_timeout: parse_millis(block, "request_timeout_ms", defaults.request_timeout)?,
        max_retries: parse_count(block, "max_retries", defaults.max_retries)?,
        retry_backoff: parse_millis(block, "retry_backoff_ms", defaults.retry_backoff)?,
    };

    Ok(Some(HttpEmbedder::with_policy(
        url,
        model,
        dimensions,
        format,
        api_key_env,
        policy,
    )?))
}

/// Read an optional duration knob, in milliseconds.
///
/// A key that is present but not a *positive* integer is an error, never a silent
/// fall-back to the default: a typo'd or zeroed timeout would restore exactly the
/// unbounded-wait behaviour these knobs exist to prevent.
fn parse_millis(block: &Value, key: &str, default: Duration) -> Result<Duration, anyhow::Error> {
    let Some(raw) = block.get(key) else {
        return Ok(default);
    };
    let ms = raw.as_u64().filter(|&m| m > 0).ok_or_else(|| {
        anyhow::anyhow!(
            "INVALID_EMBEDDINGS_CONFIG: `{key}` must be a positive integer number of \
             milliseconds; got {raw}"
        )
    })?;
    Ok(Duration::from_millis(ms))
}

/// Read an optional non-negative count knob. `0` is meaningful here (retries off).
fn parse_count(block: &Value, key: &str, default: u32) -> Result<u32, anyhow::Error> {
    let Some(raw) = block.get(key) else {
        return Ok(default);
    };
    let n = raw
        .as_u64()
        .and_then(|n| u32::try_from(n).ok())
        .ok_or_else(|| {
            anyhow::anyhow!(
                "INVALID_EMBEDDINGS_CONFIG: `{key}` must be a non-negative integer; got {raw}"
            )
        })?;
    Ok(n)
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
