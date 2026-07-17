//! YAML loader for `models.yaml`. All types here are produced from a
//! strict deserialisation (`#[serde(deny_unknown_fields)]` per struct,
//! mandatory `default:` field with no `#[serde(default)]`).
//!
//! Per-provider feature structs (`AnthropicFeatures`, `OpenAIFeatures`,
//! `GoogleFeatures`) also use `deny_unknown_fields` so a typo like
//! `reasoning_effrt: high` fails at load with the offending key named —
//! FMECA T3 mitigation.

use std::collections::BTreeMap;
use std::fmt;
use std::path::Path;
use std::str::FromStr;

use serde::{Deserialize, Serialize};

use crate::providers::ProviderId;

// ── closed enums (locked design) ────────────────────────────────────────────

/// What the model is being asked to do. Closed by design — the resolver
/// matches on this for sparse overrides. Enum additions are minor-version
/// compatible; removals are major. See `/guides/agent-config.mdx` for the
/// versioning policy.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum Affinity {
    Coding,
    /// Scientific reasoning — math/science live here (industry-standard grouping).
    #[serde(alias = "math", alias = "science")]
    Reasoning,
    Prose,
    #[serde(alias = "search")]
    WebSearch,
    Recon,
    /// Agentic tool-driving (intent → tool calls) — what the cockpit conductor
    /// and any tool-looping `kind: llm` step needs.
    #[serde(alias = "agents", alias = "tools", alias = "tool-use")]
    Agentic,
}

impl Affinity {
    /// Every affinity, for iteration (e.g. "what is this model best at").
    pub const ALL: [Affinity; 6] = [
        Affinity::Coding,
        Affinity::Reasoning,
        Affinity::Prose,
        Affinity::WebSearch,
        Affinity::Recon,
        Affinity::Agentic,
    ];

    /// This affinity's score for a model, falling back to the model's overall
    /// `intelligence` when the affinity is unscored (so partial data still ranks).
    pub fn score(self, scores: &AffinityScores, overall: f64) -> f64 {
        let v = match self {
            Affinity::Coding => scores.coding,
            Affinity::Reasoning => scores.reasoning,
            Affinity::Prose => scores.prose,
            Affinity::WebSearch => scores.web_search,
            Affinity::Recon => scores.recon,
            Affinity::Agentic => scores.agentic,
        };
        if v > 0.0 { v } else { overall }
    }
}

impl fmt::Display for Affinity {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let s = match self {
            Affinity::Coding => "coding",
            Affinity::Reasoning => "reasoning",
            Affinity::Prose => "prose",
            Affinity::WebSearch => "web-search",
            Affinity::Recon => "recon",
            Affinity::Agentic => "agentic",
        };
        f.write_str(s)
    }
}

impl FromStr for Affinity {
    type Err = String;
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        Ok(match s {
            "coding" => Affinity::Coding,
            "reasoning" | "math" | "science" => Affinity::Reasoning,
            "prose" => Affinity::Prose,
            "web-search" | "search" => Affinity::WebSearch,
            "recon" => Affinity::Recon,
            "agentic" | "agents" | "tools" | "tool-use" => Affinity::Agentic,
            other => return Err(other.to_string()),
        })
    }
}

/// **Affinity scores** — how good a model is at each affinity (the benchmark
/// facet). Sourced data (Artificial Analysis sub-indices where published), used
/// by the model suggestor to rank models against a step's `needs:` affinities.
/// An unscored affinity falls back to the model's overall `intelligence`.
#[derive(Debug, Clone, Copy, Default, PartialEq, Serialize, Deserialize)]
pub struct AffinityScores {
    #[serde(default)]
    pub coding: f64,
    #[serde(default)]
    pub reasoning: f64,
    #[serde(default)]
    pub prose: f64,
    #[serde(default)]
    pub web_search: f64,
    #[serde(default)]
    pub recon: f64,
    #[serde(default)]
    pub agentic: f64,
}

/// A model's **fit** for a group of `needs` affinities: a weighted blend of its
/// overall `intelligence` and the mean of its scores in the needed affinities —
/// a complete value that factors in general capability *and* task strength. No
/// needs → pure overall intelligence. The blend weight is configurable
/// (`tuning.affinity_weight`).
pub fn affinity_fit(scores: &AffinityScores, overall: f64, needs: &[Affinity]) -> f64 {
    if needs.is_empty() {
        return overall;
    }
    let mean: f64 =
        needs.iter().map(|a| a.score(scores, overall)).sum::<f64>() / needs.len() as f64;
    let w = crate::tuning::tuning().affinity_weight;
    w * mean + (1.0 - w) * overall
}

/// Capability tier. Same versioning policy as `Affinity`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum Tier {
    Frontier,
    Standard,
    Commoditized,
}

impl fmt::Display for Tier {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let s = match self {
            Tier::Frontier => "frontier",
            Tier::Standard => "standard",
            Tier::Commoditized => "commoditized",
        };
        f.write_str(s)
    }
}

impl FromStr for Tier {
    type Err = String;
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        Ok(match s {
            "frontier" => Tier::Frontier,
            "standard" => Tier::Standard,
            "commoditized" => Tier::Commoditized,
            other => return Err(other.to_string()),
        })
    }
}

/// Requested reasoning depth. Closed enum — maps to the existing reasoning-
/// tuning levels in `tuning.rs` so the *apply* path is unchanged; only the
/// resolution *key* is new. Same versioning policy as `Affinity` / `Tier`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum Effort {
    Fast,
    Medium,
    Deep,
}

impl fmt::Display for Effort {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let s = match self {
            Effort::Fast => "fast",
            Effort::Medium => "medium",
            Effort::Deep => "deep",
        };
        f.write_str(s)
    }
}

impl FromStr for Effort {
    type Err = String;
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        Ok(match s {
            "fast" => Effort::Fast,
            "medium" => Effort::Medium,
            "deep" => Effort::Deep,
            other => return Err(other.to_string()),
        })
    }
}

/// Distribution strategy for a pool of bindings.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum Strategy {
    Ordered,
    Distribute,
}

// ── pool (dual-form serde for activity:/overrides: values, R3) ──────────────

/// A pool of bindings paired with a distribution strategy.
///
/// Dual-form serde (R3): a bare YAML list deserializes as
/// `Pool { members: <list>, strategy: Ordered }` (backward-compatible with
/// every existing config); the explicit mapping
/// `{members: [...], strategy: "ordered"|"distribute"}` is also accepted.
/// Any other shape is an actionable error.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Pool<T> {
    pub members: Vec<T>,
    pub strategy: Strategy,
}

impl<T> Pool<T> {
    /// All members, by reference.
    pub fn members(&self) -> &[T] {
        &self.members
    }

    /// The distribution strategy.
    pub fn strategy(&self) -> Strategy {
        self.strategy
    }
}

/// A `Pool` reads as its **ordered member list**: it `Deref`s to the member
/// slice and `&Pool` iterates its members. Every consumer that reads
/// `activity:`/`overrides:` as a binding list keeps working after the
/// `Vec -> Pool` type change. Iteration is ALWAYS the members in declared order —
/// the `distribute` strategy is never applied by iterating; only the router layer
/// consults [`Pool::strategy`] to distribute. So a read-consumer can't be fooled
/// into "distributing" by accident.
impl<T> std::ops::Deref for Pool<T> {
    type Target = [T];
    fn deref(&self) -> &[T] {
        &self.members
    }
}

impl<'a, T> IntoIterator for &'a Pool<T> {
    type Item = &'a T;
    type IntoIter = std::slice::Iter<'a, T>;
    fn into_iter(self) -> Self::IntoIter {
        self.members.iter()
    }
}

/// On-disk shape for a pool — raw bindings before provider-feature typing.
#[derive(Debug, Clone)]
struct RawPool {
    members: Vec<RawBinding>,
    strategy: Strategy,
}

impl<'de> Deserialize<'de> for RawPool {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        let value = serde_yaml::Value::deserialize(deserializer)?;
        match value {
            serde_yaml::Value::Sequence(seq) => {
                let members: Vec<RawBinding> =
                    serde_yaml::from_value(serde_yaml::Value::Sequence(seq))
                        .map_err(serde::de::Error::custom)?;
                Ok(RawPool {
                    members,
                    strategy: Strategy::Ordered,
                })
            }
            serde_yaml::Value::Mapping(map) => {
                let mut members: Option<Vec<RawBinding>> = None;
                let mut strategy: Option<Strategy> = None;
                for (k, v) in map {
                    let key = match &k {
                        serde_yaml::Value::String(s) => s.as_str(),
                        _ => {
                            return Err(serde::de::Error::custom(format!(
                                "pool mapping key must be a string, got {:?}",
                                k
                            )));
                        }
                    };
                    match key {
                        "members" => {
                            if members.is_some() {
                                return Err(serde::de::Error::custom(
                                    "duplicate key `members` in pool mapping",
                                ));
                            }
                            members =
                                Some(serde_yaml::from_value(v).map_err(serde::de::Error::custom)?);
                        }
                        "strategy" => {
                            if strategy.is_some() {
                                return Err(serde::de::Error::custom(
                                    "duplicate key `strategy` in pool mapping",
                                ));
                            }
                            strategy =
                                Some(serde_yaml::from_value(v).map_err(serde::de::Error::custom)?);
                        }
                        other => {
                            return Err(serde::de::Error::custom(format!(
                                "unknown key `{other}` in pool mapping; expected only `members` and `strategy`"
                            )));
                        }
                    }
                }
                let members = members.unwrap_or_default();
                let strategy = strategy.unwrap_or(Strategy::Ordered);
                Ok(RawPool { members, strategy })
            }
            _ => Err(serde::de::Error::custom(
                "pool value must be a list of bindings or a {members, strategy} mapping",
            )),
        }
    }
}

impl RawPool {
    fn into_pool(self) -> Result<Pool<Binding>, ModelConfigError> {
        let members = self
            .members
            .into_iter()
            .map(RawBinding::into_binding)
            .collect::<Result<Vec<_>, _>>()?;
        Ok(Pool {
            members,
            strategy: self.strategy,
        })
    }
}

/// models.yaml `provider:` — a curated catalog member or the OpenAI-compatible
/// custom-endpoint escape hatch.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(from = "RawProvider")]
pub enum Provider {
    Known(ProviderId),
    /// Self-hosted / unlisted OpenAI-shaped provider (was `lmstudio` etc.).
    Custom {
        endpoint: String,
    },
}

/// On-disk shape. `name:` is the canonical slug; `custom` carries `endpoint`.
#[derive(Debug, Clone, Deserialize)]
#[serde(tag = "name", rename_all = "kebab-case", deny_unknown_fields)]
enum RawProvider {
    Anthropic,
    Openai,
    Gemini,
    Openrouter,
    Ollama,
    Llamacpp,
    Bedrock,
    Fireworks,
    Custom { endpoint: String },
}

impl From<RawProvider> for Provider {
    fn from(r: RawProvider) -> Self {
        match r {
            RawProvider::Anthropic => Provider::Known(ProviderId::Anthropic),
            RawProvider::Openai => Provider::Known(ProviderId::Openai),
            RawProvider::Gemini => Provider::Known(ProviderId::Gemini),
            RawProvider::Openrouter => Provider::Known(ProviderId::Openrouter),
            RawProvider::Ollama => Provider::Known(ProviderId::Ollama),
            RawProvider::Llamacpp => Provider::Known(ProviderId::Llamacpp),
            RawProvider::Bedrock => Provider::Known(ProviderId::Bedrock),
            RawProvider::Fireworks => Provider::Known(ProviderId::Fireworks),
            RawProvider::Custom { endpoint } => Provider::Custom { endpoint },
        }
    }
}

impl Provider {
    /// The canonical catalog slug (e.g. `"gemini"`) for a known provider, or
    /// `"custom"`. This slug equals the aether-llm parser token the runtime
    /// model-string (`provider:model`) uses.
    pub fn display_name(&self) -> &'static str {
        match self {
            Provider::Known(id) => id.slug(),
            Provider::Custom { .. } => "custom",
        }
    }
}

// ── feature toggle structs (closed; `deny_unknown_fields`) ──────────────────

/// Anthropic-specific feature toggles. Typos like `extendd_thinking` fail
/// at load with the field named (FMECA T3 mitigation).
#[derive(Debug, Clone, Default, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields, rename_all = "snake_case")]
pub struct AnthropicFeatures {
    #[serde(default)]
    pub extended_thinking: bool,
    #[serde(default)]
    pub thinking_budget_tokens: Option<u32>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields, rename_all = "snake_case")]
pub struct OpenAIFeatures {
    /// `low` | `medium` | `high`. String, not enum, because OpenAI's API
    /// accepts a few additional values we don't want to fix in code.
    #[serde(default)]
    pub reasoning_effort: Option<String>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields, rename_all = "snake_case")]
pub struct GoogleFeatures {
    #[serde(default)]
    pub thinking_budget_tokens: Option<u32>,
}

/// Per-provider feature set on a `Binding`. Discriminated by provider so
/// a binding with `provider: anthropic` accepts only Anthropic feature
/// keys; OpenAI flags on an Anthropic binding fail at load.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub enum ProviderFeatures {
    Anthropic(AnthropicFeatures),
    OpenAI(OpenAIFeatures),
    Google(GoogleFeatures),
    /// Providers without typed feature toggles (Ollama, llama.cpp, OpenRouter, Bedrock, Custom).
    #[default]
    None,
}

// ── binding ─────────────────────────────────────────────────────────────────

/// One concrete binding: the provider + model the resolver will run a step
/// against, plus the typed feature toggles for that provider.
///
/// This is the whole of a **model** in praxec: a provider+model binding and
/// nothing more. **A model carries no instructions — only which engine runs.**
/// Instruction content lives in *skills* (SPEC §5, §33.12), which a `kind: llm`
/// step injects as its system message; the *agent* is the worker that results
/// from running a skill on a model. Do not add a persona/system-prompt field
/// here; that would conflate the model (engine) with the skill (instructions).
/// See the three-slot contract in SPEC §33.12.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Binding {
    pub provider: Provider,
    pub model: String,
    pub features: ProviderFeatures,
}

/// On-disk shape (before features are typed per-provider).
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
struct RawBinding {
    provider: Provider,
    model: String,
    #[serde(default)]
    features: Option<serde_yaml::Value>,
}

impl RawBinding {
    fn into_binding(self) -> Result<Binding, ModelConfigError> {
        if self.model.trim().is_empty() {
            return Err(ModelConfigError::MissingProviderModel);
        }
        if let Provider::Custom { endpoint } = &self.provider {
            if endpoint.trim().is_empty() {
                return Err(ModelConfigError::ProviderEndpointRequired);
            }
        }
        let features = match (&self.provider, self.features) {
            (Provider::Known(ProviderId::Anthropic), Some(v)) => ProviderFeatures::Anthropic(
                serde_yaml::from_value::<AnthropicFeatures>(v)
                    .map_err(|e| feature_error("anthropic", e))?,
            ),
            (Provider::Known(ProviderId::Openai), Some(v)) => ProviderFeatures::OpenAI(
                serde_yaml::from_value::<OpenAIFeatures>(v)
                    .map_err(|e| feature_error("openai", e))?,
            ),
            // Internal type name kept as GoogleFeatures; the slug is "gemini".
            (Provider::Known(ProviderId::Gemini), Some(v)) => ProviderFeatures::Google(
                serde_yaml::from_value::<GoogleFeatures>(v)
                    .map_err(|e| feature_error("gemini", e))?,
            ),
            (Provider::Known(_), Some(v)) | (Provider::Custom { .. }, Some(v)) => {
                if !v.is_null() && !matches!(&v, serde_yaml::Value::Mapping(m) if m.is_empty()) {
                    return Err(ModelConfigError::UnknownFeatureKey {
                        provider: "(provider without typed features)".to_string(),
                        key: "(any)".to_string(),
                    });
                }
                ProviderFeatures::None
            }
            (Provider::Known(ProviderId::Anthropic), None) => {
                ProviderFeatures::Anthropic(Default::default())
            }
            (Provider::Known(ProviderId::Openai), None) => {
                ProviderFeatures::OpenAI(Default::default())
            }
            (Provider::Known(ProviderId::Gemini), None) => {
                ProviderFeatures::Google(Default::default())
            }
            (_, None) => ProviderFeatures::None,
        };
        Ok(Binding {
            provider: self.provider,
            model: self.model,
            features,
        })
    }
}

fn feature_error(provider: &str, e: serde_yaml::Error) -> ModelConfigError {
    let msg = e.to_string();
    // serde_yaml's deny_unknown_fields error includes the offending key
    // verbatim — we surface the whole message rather than re-parsing it.
    ModelConfigError::UnknownFeatureKey {
        provider: provider.to_string(),
        key: msg,
    }
}

// ── override key ────────────────────────────────────────────────────────────

/// YAML key in the `overrides:` map. One of `<affinity>-<tier>`,
/// `<affinity>`, or `<tier>`. Parsed strictly — `affinity-only` collides
/// with `affinity-tier` only when both segments parse cleanly.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct OverrideKey {
    pub affinity: Option<Affinity>,
    pub tier: Option<Tier>,
}

impl OverrideKey {
    pub fn parse(raw: &str) -> Result<Self, ModelConfigError> {
        // Try affinity-tier first (the only form with `-` between two
        // closed-enum members — `web-search` is itself hyphenated, so we
        // look for the LAST `-` and try both halves).
        if let Some(idx) = raw.rfind('-') {
            let (left, right) = (&raw[..idx], &raw[idx + 1..]);
            if let (Ok(a), Ok(t)) = (Affinity::from_str(left), Tier::from_str(right)) {
                return Ok(OverrideKey {
                    affinity: Some(a),
                    tier: Some(t),
                });
            }
        }
        if let Ok(a) = Affinity::from_str(raw) {
            return Ok(OverrideKey {
                affinity: Some(a),
                tier: None,
            });
        }
        if let Ok(t) = Tier::from_str(raw) {
            return Ok(OverrideKey {
                affinity: None,
                tier: Some(t),
            });
        }
        Err(ModelConfigError::UnknownOverrideKey(raw.to_string()))
    }
}

impl fmt::Display for OverrideKey {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match (self.affinity, self.tier) {
            (Some(a), Some(t)) => write!(f, "{a}-{t}"),
            (Some(a), None) => write!(f, "{a}"),
            (None, Some(t)) => write!(f, "{t}"),
            (None, None) => f.write_str("default"),
        }
    }
}

impl<'de> Deserialize<'de> for OverrideKey {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        let raw = String::deserialize(deserializer)?;
        OverrideKey::parse(&raw).map_err(serde::de::Error::custom)
    }
}

// ── top-level file ──────────────────────────────────────────────────────────

/// `models.yaml` on-disk shape. Mandatory: `version`, `default`. Optional:
/// `strict_specificity`, `overrides`.
///
/// FMECA U4 mitigation: `default` has NO `#[serde(default)]`. Missing →
/// load error.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
struct RawModelsFile {
    version: u8,
    #[serde(default)]
    strict_specificity: bool,
    default: Vec<RawBinding>,
    #[serde(default)]
    overrides: BTreeMap<OverrideKey, RawPool>,
    /// OPEN activity-keyed pools (any string key, e.g. `review`,
    /// `marketing-copy`) — lets a flow give any activity its own escalation
    /// path without a core change. Distinct from `overrides` (the closed
    /// affinity/tier keys), so OverrideKey stays Copy (no cascade).
    #[serde(default)]
    activity: BTreeMap<String, RawPool>,
}

#[derive(Debug, Clone)]
pub struct ModelsFile {
    pub version: u8,
    pub strict_specificity: bool,
    pub default: Vec<Binding>,
    pub overrides: BTreeMap<OverrideKey, Pool<Binding>>,
    /// Open activity-keyed pools (see [`RawModelsFile::activity`]).
    pub activity: BTreeMap<String, Pool<Binding>>,
}

/// Forward-compat: the loader accepts only version 1. Higher versions
/// surface explicitly so an older praxec against a newer config gives
/// a clear "upgrade" message instead of silently mis-parsing.
pub const CURRENT_MODELS_FILE_VERSION: u8 = 1;

impl ModelsFile {
    /// Parse from a YAML string. Returns the typed in-memory shape;
    /// every High-risk FMECA row's check fires here at load time.
    pub fn from_yaml(input: &str) -> Result<Self, ModelConfigError> {
        let raw: RawModelsFile = serde_yaml::from_str(input)
            .map_err(|e| ModelConfigError::YamlSyntax(e).refine_missing_default())?;
        if raw.version != CURRENT_MODELS_FILE_VERSION {
            return Err(ModelConfigError::VersionMismatch {
                got: raw.version,
                expected: CURRENT_MODELS_FILE_VERSION,
            });
        }
        if raw.default.is_empty() {
            // serde succeeded (the field was present) but the list is
            // empty — operator wrote `default: []`. Treat as missing-by-
            // intent: an empty default cannot resolve anything.
            return Err(ModelConfigError::EmptyDefault);
        }
        let default = raw
            .default
            .into_iter()
            .map(RawBinding::into_binding)
            .collect::<Result<Vec<_>, _>>()?;
        let mut overrides = BTreeMap::new();
        for (k, v) in raw.overrides {
            let pool = v.into_pool()?;
            overrides.insert(k, pool);
        }
        let mut activity = BTreeMap::new();
        for (k, v) in raw.activity {
            let pool = v.into_pool()?;
            activity.insert(k, pool);
        }
        Ok(ModelsFile {
            version: raw.version,
            strict_specificity: raw.strict_specificity,
            default,
            overrides,
            activity,
        })
    }

    /// Convenience wrapper: read a file from disk.
    pub fn from_path(path: &Path) -> Result<Self, ModelConfigError> {
        let bytes = std::fs::read_to_string(path).map_err(ModelConfigError::Io)?;
        ModelsFile::from_yaml(&bytes)
    }
}

// ── error type ──────────────────────────────────────────────────────────────

#[derive(Debug, thiserror::Error)]
pub enum ModelConfigError {
    #[error("models.yaml is missing required `default:` section")]
    MissingDefault,

    #[error("models.yaml `default:` is present but empty — at least one binding is required")]
    EmptyDefault,

    #[error("binding is missing `provider` and/or `model`")]
    MissingProviderModel,

    #[error(
        "models.yaml override key `{0}` is not a valid <affinity> | <tier> | <affinity>-<tier>; \
         affinity ∈ {{coding, reasoning, prose, web-search, recon}}, \
         tier ∈ {{frontier, standard, commoditized}}"
    )]
    UnknownOverrideKey(String),

    #[error("provider `{provider}` rejected feature key(s): {key}")]
    UnknownFeatureKey { provider: String, key: String },

    #[error("provider `custom` requires a non-empty `endpoint` field")]
    ProviderEndpointRequired,

    #[error(
        "models.yaml version mismatch: got {got}, this praxec supports {expected}. \
         Upgrade praxec or downgrade the config."
    )]
    VersionMismatch { got: u8, expected: u8 },

    #[error("models.yaml syntax error: {0}")]
    YamlSyntax(#[source] serde_yaml::Error),

    #[error("models.yaml I/O error: {0}")]
    Io(#[source] std::io::Error),
}

// Serde's `missing field "default"` error is a `serde_yaml::Error`; we
// translate it to the more specific `MissingDefault` variant at the call
// site that constructs `ModelsFile`. Done in `from_yaml` above via an
// inspection of the error message — but tests rely on the exact variant.
// Implement a translation helper:
impl ModelConfigError {
    /// Inspect a `YamlSyntax` error and re-extract the typed inner variant
    /// when serde's wrapping has lost it. Idempotent — pass-through when
    /// the inner cause isn't recognized.
    ///
    /// Why: custom deserializers (e.g. `OverrideKey::deserialize`) emit
    /// our typed errors via `serde::de::Error::custom`, which serde wraps
    /// in its own error chain. The string survives intact; the typed
    /// variant doesn't. This refiner reconstructs the variant by matching
    /// the stable marker strings embedded in each variant's `Display`
    /// impl. Tests in `model_resolver_config.rs` pin the marker strings.
    pub fn refine_missing_default(self) -> Self {
        if let ModelConfigError::YamlSyntax(e) = &self {
            let msg = e.to_string();
            if msg.contains("missing field `default`") {
                return ModelConfigError::MissingDefault;
            }
            // `OverrideKey::parse` emits its key in the form:
            // ``models.yaml override key `<KEY>` is not a valid``.
            if let Some(key) = extract_between(&msg, "override key `", "` is not a valid") {
                return ModelConfigError::UnknownOverrideKey(key.to_string());
            }
        }
        self
    }
}

fn extract_between<'a>(haystack: &'a str, start: &str, end: &str) -> Option<&'a str> {
    let s = haystack.find(start)? + start.len();
    let rest = &haystack[s..];
    let e = rest.find(end)?;
    Some(&rest[..e])
}

#[cfg(test)]
mod provider_parse_tests {
    use super::*;
    use crate::providers::ProviderId;

    #[test]
    fn fireworks_provider_name_parses_to_known() {
        let p: Provider = serde_yaml::from_str("name: fireworks").unwrap();
        assert_eq!(p, Provider::Known(ProviderId::Fireworks));
    }
}

#[cfg(test)]
mod effort_tests {
    use super::*;
    use std::str::FromStr;

    #[test]
    fn effort_from_str_and_display_roundtrip() {
        assert_eq!(Effort::from_str("fast"), Ok(Effort::Fast));
        assert_eq!(Effort::from_str("medium"), Ok(Effort::Medium));
        assert_eq!(Effort::from_str("deep"), Ok(Effort::Deep));
        assert_eq!(Effort::Fast.to_string(), "fast");
        assert_eq!(Effort::Medium.to_string(), "medium");
        assert_eq!(Effort::Deep.to_string(), "deep");
    }

    #[test]
    fn effort_unknown_rejected() {
        assert!(Effort::from_str("slow").is_err());
        assert!(Effort::from_str("").is_err());
    }

    #[test]
    fn effort_serde_kebab_case() {
        let e: Effort = serde_yaml::from_str("fast").unwrap();
        assert_eq!(e, Effort::Fast);
        let e: Effort = serde_yaml::from_str("medium").unwrap();
        assert_eq!(e, Effort::Medium);
        let e: Effort = serde_yaml::from_str("deep").unwrap();
        assert_eq!(e, Effort::Deep);
    }
}

#[cfg(test)]
mod strategy_tests {
    use super::*;

    #[test]
    fn strategy_serde_kebab_case() {
        let s: Strategy = serde_yaml::from_str("ordered").unwrap();
        assert_eq!(s, Strategy::Ordered);
        let s: Strategy = serde_yaml::from_str("distribute").unwrap();
        assert_eq!(s, Strategy::Distribute);
    }
}

#[cfg(test)]
mod pool_serde_tests {
    use super::*;

    fn raw_pool_from_yaml(yaml: &str) -> Result<RawPool, String> {
        serde_yaml::from_str::<RawPool>(yaml).map_err(|e| e.to_string())
    }

    #[test]
    fn bare_list_deserializes_as_ordered() {
        let pool = raw_pool_from_yaml(
            "- provider:\n    name: openai\n  model: gpt-4o\n- provider:\n    name: anthropic\n  model: claude-sonnet-4-20250514\n"
        ).unwrap();
        assert_eq!(pool.members.len(), 2);
        assert_eq!(pool.strategy, Strategy::Ordered);
    }

    #[test]
    fn mapping_with_strategy_distribute() {
        let pool = raw_pool_from_yaml(
            "members:\n  - provider:\n      name: openai\n    model: gpt-4o\nstrategy: distribute\n"
        ).unwrap();
        assert_eq!(pool.members.len(), 1);
        assert_eq!(pool.strategy, Strategy::Distribute);
    }

    #[test]
    fn mapping_without_strategy_defaults_to_ordered() {
        let pool =
            raw_pool_from_yaml("members:\n  - provider:\n      name: openai\n    model: gpt-4o\n")
                .unwrap();
        assert_eq!(pool.strategy, Strategy::Ordered);
    }

    #[test]
    fn mapping_with_empty_members_and_no_strategy() {
        let pool = raw_pool_from_yaml("members: []\n").unwrap();
        assert_eq!(pool.members.len(), 0);
        assert_eq!(pool.strategy, Strategy::Ordered);
    }

    #[test]
    fn mapping_unknown_key_errors() {
        let err = raw_pool_from_yaml(
            "members:\n  - provider:\n      name: openai\n    model: gpt-4o\nextra: bad\n",
        )
        .unwrap_err();
        assert!(err.contains("unknown key `extra`"), "got: {err}");
    }

    #[test]
    fn mapping_with_ordered_strategy_explicit() {
        let pool = raw_pool_from_yaml(
            "members:\n  - provider:\n      name: openai\n    model: gpt-4o\nstrategy: ordered\n",
        )
        .unwrap();
        assert_eq!(pool.members.len(), 1);
        assert_eq!(pool.strategy, Strategy::Ordered);
    }

    #[test]
    fn scalar_value_errors_with_dual_form_message() {
        let err = raw_pool_from_yaml("not-a-sequence-or-mapping").unwrap_err();
        assert!(
            err.contains("pool value must be a list of bindings or a {members, strategy} mapping"),
            "got: {err}"
        );
    }

    #[test]
    fn empty_list_deserializes_as_empty_ordered() {
        let pool = raw_pool_from_yaml("[]\n").unwrap();
        assert_eq!(pool.members.len(), 0);
        assert_eq!(pool.strategy, Strategy::Ordered);
    }

    #[test]
    fn into_pool_converts_raw_bindings() {
        let raw = RawPool {
            members: vec![RawBinding {
                provider: Provider::Known(ProviderId::Openai),
                model: "gpt-4o".to_string(),
                features: None,
            }],
            strategy: Strategy::Distribute,
        };
        let pool: Pool<Binding> = raw.into_pool().unwrap();
        assert_eq!(pool.members.len(), 1);
        assert_eq!(pool.strategy, Strategy::Distribute);
        assert_eq!(pool.members[0].model, "gpt-4o");
    }
}

#[cfg(test)]
mod affinity_scores_tests {
    use super::*;
    use std::str::FromStr;

    #[test]
    fn agentic_round_trips_and_aliases() {
        assert_eq!(Affinity::from_str("agentic"), Ok(Affinity::Agentic));
        assert_eq!(Affinity::from_str("agents"), Ok(Affinity::Agentic));
        assert_eq!(Affinity::from_str("math"), Ok(Affinity::Reasoning));
        assert_eq!(Affinity::Agentic.to_string(), "agentic");
    }

    #[test]
    fn score_falls_back_to_overall_when_unscored() {
        let s = AffinityScores {
            coding: 64.0,
            ..Default::default()
        };
        assert_eq!(Affinity::Coding.score(&s, 50.0), 64.0); // scored
        assert_eq!(Affinity::Prose.score(&s, 50.0), 50.0); // unscored → overall
    }

    #[test]
    fn fit_blends_overall_with_the_needed_affinities() {
        // overall 50, coding 70 → half/half = 60. No needs → pure overall.
        let s = AffinityScores {
            coding: 70.0,
            ..Default::default()
        };
        assert_eq!(affinity_fit(&s, 50.0, &[Affinity::Coding]), 60.0);
        assert_eq!(affinity_fit(&s, 50.0, &[]), 50.0);
    }
}
