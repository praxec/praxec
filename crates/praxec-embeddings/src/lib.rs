#![cfg_attr(not(test), warn(clippy::unwrap_used))]

//! rig-backed embedding provider for praxec.
//!
//! praxec owns the [`EmbeddingProvider`] trait, `cosine_similarity`, and the
//! index in `praxec-core`; this crate is the concrete embedder, backed by
//! the `rig` crate (which treats embeddings as first-class, independent of the
//! chat SDK). It also carries the **embedding-model option catalog** the bootstrap
//! picker offers (a dated snapshot derived from models.dev — not a hard-coded
//! default; the live catalog refresh is a later slice).

use async_trait::async_trait;
use praxec_core::embeddings::{EmbeddingError, EmbeddingProvider};
use rig::client::{EmbeddingsClient, ProviderClient};
use rig::embeddings::EmbeddingModel as _;
use rig::providers::{gemini, ollama, openai, openrouter};
use serde::{Deserialize, Serialize};
use std::path::PathBuf;

/// One embedding-model option the bootstrap picker can offer. **Data, not code**:
/// loaded from the catalog file, never hard-coded — the catalog changes faster
/// than the code and must not require a release.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct EmbeddingOption {
    /// Vendor slug — matches `ProviderId::slug`.
    pub vendor: String,
    pub model: String,
    pub dims: usize,
    /// USD per million input tokens (`0.0` for local/keyless).
    #[serde(default)]
    pub input_usd_per_million: f64,
    /// True for local backends (Ollama) whose weights are an on-disk cost.
    #[serde(default)]
    pub local: bool,
    /// MTEB score (English overall average — a sourced, dated snapshot) used as
    /// the quality signal for semantic description-search. Drives the
    /// recommendation; `0.0` = unscored (ranks lowest).
    #[serde(default)]
    pub mteb_score: f64,
}

/// The shipped default catalog — a dated snapshot kept as **data**
/// (`data/embedding_models.json`), so new models ship without a code release.
const DEFAULT_EMBEDDING_MODELS: &str = include_str!("../data/embedding_models.json");

/// The shipped default catalog, ignoring any override — used where determinism
/// matters (demo / tests).
pub fn default_embedding_options() -> Vec<EmbeddingOption> {
    praxec_core::catalog::load_default(DEFAULT_EMBEDDING_MODELS)
}

/// The embedding-model catalog: a user/project override if present, else the
/// shipped default. Uses the shared `core::catalog` loader so the override
/// precedence is identical to every other catalog.
pub fn embedding_options() -> Vec<EmbeddingOption> {
    praxec_core::catalog::load_catalog(
        "PRAXEC_EMBEDDING_MODELS_FILE",
        "embedding_models.json",
        DEFAULT_EMBEDDING_MODELS,
    )
}

/// The canonical reachability check, re-exported from core (one impl across the
/// model catalog, the cockpit picker, and embeddings).
pub use praxec_core::providers::vendor_available;

/// The catalog options whose provider is reachable (key present, or local).
pub fn available_options() -> Vec<EmbeddingOption> {
    embedding_options()
        .into_iter()
        .filter(|o| vendor_available(&o.vendor))
        .collect()
}

/// The single best embedding model for **description-retrieval** among the
/// reachable (configured-key or local) options in `options`. Ranks by
/// MTEB quality — the job-relevant benchmark — preferring the cheaper
/// of near-equals. `None` when nothing is reachable. The caller passes the full
/// catalog; this filters to what's actually usable.
pub fn recommend(options: &[EmbeddingOption]) -> Option<&EmbeddingOption> {
    options
        .iter()
        .filter(|o| vendor_available(&o.vendor))
        .max_by(|a, b| {
            a.mteb_score
                .partial_cmp(&b.mteb_score)
                .unwrap_or(std::cmp::Ordering::Equal)
                // tie-break: the cheaper model wins.
                .then(
                    b.input_usd_per_million
                        .partial_cmp(&a.input_usd_per_million)
                        .unwrap_or(std::cmp::Ordering::Equal),
                )
        })
}

/// A plain-language rationale for recommending `opt` for semantic
/// description-search, relative to the reachable options in `among`.
pub fn rationale(opt: &EmbeddingOption, among: &[EmbeddingOption]) -> String {
    let best_reachable = among
        .iter()
        .filter(|o| vendor_available(&o.vendor))
        .all(|o| o.mteb_score <= opt.mteb_score);
    let lead = if best_reachable {
        "Best for semantic search among your providers"
    } else {
        "Strong for semantic search"
    };
    let cost = match cost_magnitude(opt) {
        CostMagnitude::Free => "free to run locally".to_string(),
        m => format!("about {} to run", m.label()),
    };
    format!(
        "{lead} (MTEB {:.0}); {cost}; {}-dimension vectors.",
        opt.mteb_score, opt.dims
    )
}

// ── cost in orders of magnitude (what users actually reason about) ───────────

/// Estimation assumptions — a typical active catalog + daily search volume.
/// These produce an *order of magnitude*, not a precise figure.
pub const EST_ITEMS: usize = 200;
pub const EST_SEARCHES_PER_DAY: usize = 1000;
const AVG_ITEM_TOKENS: f64 = 150.0;
const AVG_QUERY_TOKENS: f64 = 20.0;

/// Estimated USD/day to run semantic discovery on `opt`: re-indexing the catalog
/// + embedding the day's queries (the two streams you pay for). Local = free.
pub fn estimated_usd_per_day(opt: &EmbeddingOption, items: usize, searches_per_day: usize) -> f64 {
    if opt.local {
        return 0.0;
    }
    let per_token = opt.input_usd_per_million / 1_000_000.0;
    let index = items as f64 * AVG_ITEM_TOKENS * per_token;
    let queries = searches_per_day as f64 * AVG_QUERY_TOKENS * per_token;
    index + queries
}

/// Cost in human orders of magnitude — the bucket, not the number. Spans free
/// (local) all the way to "$10k+/day" so the same scale serves cheap embeddings
/// and an expensive, high-volume chat conductor.
/// Declared low→high so the derived `Ord` is the cost ordering — a budget
/// ceiling is then just `magnitude <= cap`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
pub enum CostMagnitude {
    Free,
    Pennies,
    TensOfCents,
    Dollars,
    TensOfDollars,
    HundredsOfDollars,
    ThousandsOfDollars,
    TensOfThousandsOrMore,
}

impl CostMagnitude {
    pub fn from_usd_per_day(usd: f64) -> Self {
        if usd <= 0.0 {
            return Self::Free;
        }
        // The bucket boundaries are configurable (`tuning.cost_magnitude_*`).
        use CostMagnitude::*;
        const BUCKETS: [CostMagnitude; 6] = [
            Pennies,
            TensOfCents,
            Dollars,
            TensOfDollars,
            HundredsOfDollars,
            ThousandsOfDollars,
        ];
        let thresholds = &praxec_core::tuning::tuning().cost_magnitude_thresholds_usd_per_day;
        for (i, &thr) in thresholds.iter().enumerate() {
            if usd < thr {
                return BUCKETS
                    .get(i)
                    .copied()
                    .unwrap_or(Self::TensOfThousandsOrMore);
            }
        }
        Self::TensOfThousandsOrMore
    }

    pub fn label(self) -> &'static str {
        match self {
            Self::Free => "free (local)",
            Self::Pennies => "pennies a day",
            Self::TensOfCents => "tens of cents a day",
            Self::Dollars => "dollars a day",
            Self::TensOfDollars => "tens of dollars a day",
            Self::HundredsOfDollars => "hundreds of dollars a day",
            Self::ThousandsOfDollars => "thousands of dollars a day",
            Self::TensOfThousandsOrMore => "tens of thousands of dollars a day or more",
        }
    }
}

/// The cost magnitude for `opt` at the default estimation assumptions.
pub fn cost_magnitude(opt: &EmbeddingOption) -> CostMagnitude {
    CostMagnitude::from_usd_per_day(estimated_usd_per_day(opt, EST_ITEMS, EST_SEARCHES_PER_DAY))
}

/// The persisted embedding-model choice — the one model shared across all of
/// praxec's semantic discovery. Stored so it survives restarts (changing it
/// invalidates the index, which must be re-embedded — a later slice).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct EmbeddingChoice {
    pub vendor: String,
    pub model: String,
    pub dims: usize,
}

impl EmbeddingChoice {
    /// Construct a runnable embedder from this choice.
    pub fn build(&self) -> anyhow::Result<RigEmbedder> {
        RigEmbedder::from_choice(&self.vendor, &self.model, self.dims)
    }
}

impl From<&EmbeddingOption> for EmbeddingChoice {
    fn from(o: &EmbeddingOption) -> Self {
        Self {
            vendor: o.vendor.to_string(),
            model: o.model.to_string(),
            dims: o.dims,
        }
    }
}

/// The persisted embedding decision. Semantic discovery is an **opt-in add-on**:
/// the user either registers a model or explicitly declines (lexical-only). A
/// *missing* file means "not yet decided" — that's what triggers the startup
/// gate; `Lexical` records a deliberate decline so it isn't asked again.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "mode", rename_all = "lowercase")]
pub enum EmbeddingSetting {
    /// Lexical search only — the add-on declined.
    Lexical,
    /// A registered embedding model — the add-on enabled.
    Model(EmbeddingChoice),
}

/// On-disk path for the embedding choice. Precedence: `$PRAXEC_EMBEDDING_FILE`,
/// then `~/.praxec/embedding.json`, then a CWD fallback.
pub fn choice_path() -> PathBuf {
    if let Ok(p) = std::env::var("PRAXEC_EMBEDDING_FILE") {
        if !p.trim().is_empty() {
            return PathBuf::from(p);
        }
    }
    match dirs::home_dir() {
        Some(d) => d.join(".praxec").join("embedding.json"),
        None => PathBuf::from("praxec-embedding.json"),
    }
}

/// Load the persisted setting, or `None` (not yet decided → show the gate).
pub fn load_setting() -> Option<EmbeddingSetting> {
    let raw = std::fs::read_to_string(choice_path()).ok()?;
    serde_json::from_str(&raw).ok()
}

/// Persist the setting (creating the config dir if needed).
pub fn save_setting(setting: &EmbeddingSetting) -> anyhow::Result<()> {
    let path = choice_path();
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    std::fs::write(&path, serde_json::to_string_pretty(setting)?)?;
    Ok(())
}

/// The active embedding model, if one is registered (`None` for not-decided or
/// declined-lexical). This is what consumers (the serve path) build from.
pub fn load_choice() -> Option<EmbeddingChoice> {
    match load_setting() {
        Some(EmbeddingSetting::Model(c)) => Some(c),
        _ => None,
    }
}

/// Persist a registered model (the add-on enabled).
pub fn save_choice(choice: &EmbeddingChoice) -> anyhow::Result<()> {
    save_setting(&EmbeddingSetting::Model(choice.clone()))
}

/// The provider-specific rig embedding model (rig's `EmbeddingModel` is not
/// object-safe — associated `Client`/const — so we enum-dispatch).
enum Inner {
    OpenAi(openai::EmbeddingModel),
    Gemini(gemini::embedding::EmbeddingModel),
    Ollama(ollama::EmbeddingModel),
    OpenRouter(openrouter::embedding::EmbeddingModel),
}

/// A rig-backed embedder implementing praxec's [`EmbeddingProvider`].
pub struct RigEmbedder {
    inner: Inner,
    dims: usize,
    backend: &'static str,
}

impl RigEmbedder {
    /// Construct an embedder for a chosen `(vendor, model, dims)`. The provider
    /// client is built from the process env (keys loaded from `providers.env` at
    /// startup); `dims` comes from the option catalog.
    pub fn from_choice(vendor: &str, model: &str, dims: usize) -> anyhow::Result<Self> {
        let inner = match vendor {
            "openai" => Inner::OpenAi(
                openai::Client::from_env()
                    .map_err(|e| anyhow::anyhow!("{e}"))?
                    .embedding_model_with_ndims(model, dims),
            ),
            "gemini" => Inner::Gemini(
                gemini::Client::from_env()
                    .map_err(|e| anyhow::anyhow!("{e}"))?
                    .embedding_model_with_ndims(model, dims),
            ),
            "ollama" => Inner::Ollama(
                ollama::Client::from_env()
                    .map_err(|e| anyhow::anyhow!("{e}"))?
                    .embedding_model_with_ndims(model, dims),
            ),
            "openrouter" => Inner::OpenRouter(
                openrouter::Client::from_env()
                    .map_err(|e| anyhow::anyhow!("{e}"))?
                    .embedding_model_with_ndims(model, dims),
            ),
            other => anyhow::bail!("unsupported embedding vendor '{other}'"),
        };
        Ok(Self {
            inner,
            dims,
            backend: backend_label(vendor),
        })
    }
}

/// Guard a backend-returned vector against the catalog dimension `dims` that
/// `dimensions()` advertises (and that the index/cosine math assumes). A
/// mismatched or empty vector is a fail-fast `BackendFailed`, never silently
/// indexed. Pure so it is unit-testable without a live backend.
fn checked_dims(result: Vec<f32>, dims: usize, backend: &str) -> Result<Vec<f32>, EmbeddingError> {
    if result.len() != dims {
        return Err(EmbeddingError::BackendFailed(format!(
            "dimension mismatch from {backend}: got {}, expected {dims}",
            result.len()
        )));
    }
    Ok(result)
}

fn backend_label(vendor: &str) -> &'static str {
    match vendor {
        "openai" => "rig:openai",
        "gemini" => "rig:gemini",
        "ollama" => "rig:ollama",
        "openrouter" => "rig:openrouter",
        _ => "rig",
    }
}

#[async_trait]
impl EmbeddingProvider for RigEmbedder {
    async fn embed(&self, text: &str) -> Result<Vec<f32>, EmbeddingError> {
        let emb = match &self.inner {
            Inner::OpenAi(m) => m.embed_text(text).await,
            Inner::Gemini(m) => m.embed_text(text).await,
            Inner::Ollama(m) => m.embed_text(text).await,
            Inner::OpenRouter(m) => m.embed_text(text).await,
        }
        .map_err(|e| EmbeddingError::BackendFailed(e.to_string()))?;
        // praxec's index is f32; rig returns f64.
        let result: Vec<f32> = emb.vec.into_iter().map(|v| v as f32).collect();
        // H8: fail fast on a dimension mismatch — a backend that returns a
        // shorter/longer/empty vector would silently corrupt cosine/the index.
        checked_dims(result, self.dims, self.backend)
    }

    fn dimensions(&self) -> usize {
        self.dims
    }

    fn backend_name(&self) -> &'static str {
        self.backend
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use praxec_core::providers::ProviderId;

    #[test]
    fn shipped_catalog_parses_and_is_well_formed() {
        let opts = default_embedding_options();
        assert!(!opts.is_empty(), "the shipped catalog data must parse");
        for o in &opts {
            assert!(o.dims > 0, "{} has no dims", o.model);
            assert!(
                ProviderId::from_slug(&o.vendor).is_some(),
                "unknown vendor {}",
                o.vendor
            );
            assert!(o.input_usd_per_million >= 0.0);
        }
    }

    #[test]
    fn local_ollama_is_always_available() {
        // Keyless/local — no env key required.
        assert!(vendor_available("ollama"));
    }

    #[test]
    fn available_options_always_include_the_local_ones() {
        let avail = available_options();
        assert!(avail.iter().any(|o| o.vendor == "ollama"));
    }

    #[test]
    fn recommend_picks_highest_retrieval_among_reachable() {
        // Local options are always reachable; nomic (52) beats all-minilm (42).
        let opts: Vec<EmbeddingOption> = default_embedding_options()
            .into_iter()
            .filter(|o| o.local)
            .collect();
        assert_eq!(recommend(&opts).unwrap().model, "nomic-embed-text");
    }

    #[test]
    fn recommend_breaks_ties_by_cost() {
        let opts = vec![
            EmbeddingOption {
                vendor: "ollama".into(),
                model: "pricey".into(),
                dims: 1,
                input_usd_per_million: 5.0,
                local: true,
                mteb_score: 50.0,
            },
            EmbeddingOption {
                vendor: "ollama".into(),
                model: "cheap".into(),
                dims: 1,
                input_usd_per_million: 1.0,
                local: true,
                mteb_score: 50.0,
            },
        ];
        assert_eq!(recommend(&opts).unwrap().model, "cheap");
    }

    #[test]
    fn rationale_cites_the_benchmark() {
        let opts: Vec<EmbeddingOption> = default_embedding_options()
            .into_iter()
            .filter(|o| o.local)
            .collect();
        let rec = recommend(&opts).unwrap().clone();
        let r = rationale(&rec, &opts);
        assert!(r.contains("MTEB"));
    }

    #[test]
    fn cost_buckets_by_order_of_magnitude() {
        use CostMagnitude::*;
        assert_eq!(CostMagnitude::from_usd_per_day(0.0), Free);
        assert_eq!(CostMagnitude::from_usd_per_day(0.05), Pennies);
        assert_eq!(CostMagnitude::from_usd_per_day(0.50), TensOfCents);
        assert_eq!(CostMagnitude::from_usd_per_day(5.0), Dollars);
        assert_eq!(CostMagnitude::from_usd_per_day(50.0), TensOfDollars);
        assert_eq!(CostMagnitude::from_usd_per_day(500.0), HundredsOfDollars);
    }

    #[test]
    fn local_is_free_and_typical_embedding_cost_is_pennies() {
        let local = default_embedding_options()
            .into_iter()
            .find(|o| o.local)
            .unwrap();
        assert_eq!(cost_magnitude(&local), CostMagnitude::Free);

        let small = default_embedding_options()
            .into_iter()
            .find(|o| o.vendor == "openai" && o.model == "text-embedding-3-small")
            .unwrap();
        // Embeddings on a typical catalog are cheap — pennies a day.
        assert_eq!(cost_magnitude(&small), CostMagnitude::Pennies);
    }

    #[test]
    fn checked_dims_passes_a_conforming_vector() {
        let v = vec![0.1_f32, 0.2, 0.3];
        assert_eq!(checked_dims(v.clone(), 3, "rig:test").unwrap(), v);
    }

    #[test]
    fn checked_dims_rejects_a_short_vector() {
        // H8: a backend returning fewer dims than advertised must fail, not
        // silently corrupt the index.
        let err = checked_dims(vec![0.1_f32, 0.2], 768, "rig:openai").unwrap_err();
        let msg = err.to_string();
        assert!(msg.contains("dimension mismatch"), "got: {msg}");
        assert!(msg.contains("got 2"), "got: {msg}");
        assert!(msg.contains("expected 768"), "got: {msg}");
    }

    #[test]
    fn checked_dims_rejects_an_empty_vector() {
        let err = checked_dims(vec![], 384, "rig:gemini").unwrap_err();
        assert!(err.to_string().contains("dimension mismatch"));
    }

    #[test]
    fn checked_dims_rejects_a_longer_vector() {
        let err = checked_dims(vec![0.0_f32; 1024], 768, "rig:openrouter").unwrap_err();
        assert!(err.to_string().contains("got 1024"));
    }

    #[test]
    fn from_choice_rejects_an_unsupported_vendor() {
        let result = RigEmbedder::from_choice("frobnicate", "x", 768);
        assert!(result.is_err());
        assert!(result.err().unwrap().to_string().contains("unsupported"));
    }

    #[test]
    fn setting_round_trips_through_disk() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("embedding.json");
        // SAFETY: test-local env var, single-threaded within this test.
        unsafe { std::env::set_var("PRAXEC_EMBEDDING_FILE", &path) };

        // Not decided yet.
        assert!(load_setting().is_none());
        assert!(load_choice().is_none());

        // Register a model.
        let choice = EmbeddingChoice::from(&default_embedding_options()[0]);
        save_choice(&choice).unwrap();
        assert_eq!(load_choice(), Some(choice.clone()));
        assert_eq!(load_setting(), Some(EmbeddingSetting::Model(choice)));

        // Decline → lexical only; load_choice is None but the decision sticks.
        save_setting(&EmbeddingSetting::Lexical).unwrap();
        assert_eq!(load_setting(), Some(EmbeddingSetting::Lexical));
        assert!(load_choice().is_none());

        unsafe { std::env::remove_var("PRAXEC_EMBEDDING_FILE") };
    }
}
