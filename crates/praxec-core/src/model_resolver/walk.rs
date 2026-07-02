//! Specificity walk + Chain-of-Responsibility resolver.
//!
//! Walk order, per the locked design:
//!
//! 1. `<affinity>-<tier>` (exact match)
//! 2. `<affinity>` (affinity wins tiebreaker)
//! 3. `<tier>`
//! 4. `default`
//!
//! When `strict_specificity: true` is set on the file, step 1's miss
//! short-circuits the whole walk → `ModelResolutionExhausted` (FMECA U1
//! poka-yoke).
//!
//! Per-list CoR is the caller's responsibility (they own the I/O); see
//! `try_next` for the contract.

use std::borrow::Cow;
use std::fmt;
use std::path::PathBuf;
use std::str::FromStr;

use super::classify::FailureClass;
use super::config::{Affinity, Binding, ModelsFile, OverrideKey, Tier};

// ── model reference (the policy form of a delegate: value) ──────────────────

/// A **model reference** by policy: the affinity-tier form of a workflow
/// state's `delegate:` value, resolved to a concrete `Binding` by the walk.
///
/// This separates the two concerns the `delegate:` field used to blur: the
/// field means *"delegate this state to a sub-agent"* (the worker), while its
/// value names *which model* that worker runs on. `ModelRef` is the latter —
/// the model, not the worker. (`delegate:` can also carry a named binding;
/// that path resolves through the registry, not this type.)
///
/// At least one of `affinity` / `tier` is `Some` (an empty reference makes no
/// sense — a state that doesn't delegate just omits the field).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ModelRef {
    pub affinity: Option<Affinity>,
    pub tier: Option<Tier>,
}

#[derive(Debug, thiserror::Error)]
pub enum ModelRefParseError {
    #[error("delegate string is empty")]
    Empty,
    #[error(
        "delegate `{0}` does not parse as <affinity> | <tier> | <affinity>-<tier>; \
         affinity ∈ {{coding, reasoning, prose, web-search, recon}}, \
         tier ∈ {{frontier, standard, commoditized}}"
    )]
    Unknown(String),
}

impl ModelRef {
    /// Parse forms: `coding-frontier`, `coding`, `frontier`. Empty input
    /// returns `Empty` so the caller can distinguish "no delegate" from
    /// "garbage delegate."
    pub fn parse(raw: &str) -> Result<Self, ModelRefParseError> {
        let raw = raw.trim();
        if raw.is_empty() {
            return Err(ModelRefParseError::Empty);
        }
        if let Some(idx) = raw.rfind('-') {
            let (left, right) = (&raw[..idx], &raw[idx + 1..]);
            if let (Ok(a), Ok(t)) = (Affinity::from_str(left), Tier::from_str(right)) {
                return Ok(ModelRef {
                    affinity: Some(a),
                    tier: Some(t),
                });
            }
        }
        if let Ok(a) = Affinity::from_str(raw) {
            return Ok(ModelRef {
                affinity: Some(a),
                tier: None,
            });
        }
        if let Ok(t) = Tier::from_str(raw) {
            return Ok(ModelRef {
                affinity: None,
                tier: Some(t),
            });
        }
        Err(ModelRefParseError::Unknown(raw.to_string()))
    }
}

impl fmt::Display for ModelRef {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match (self.affinity, self.tier) {
            (Some(a), Some(t)) => write!(f, "{a}-{t}"),
            (Some(a), None) => write!(f, "{a}"),
            (None, Some(t)) => write!(f, "{t}"),
            (None, None) => f.write_str("(empty)"),
        }
    }
}

// Deserialize via the same strict `parse` used for the workflow `delegate:`
// field — accepts `<affinity> | <tier> | <affinity>-<tier>` and rejects
// anything else (mirrors `OverrideKey`'s custom deserializer). This lets
// executor configs type their `affinity:` field as `ModelRef` so a typo
// fails at `check` rather than at resolve time.
impl<'de> serde::Deserialize<'de> for ModelRef {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        let raw = <String as serde::Deserialize>::deserialize(deserializer)?;
        ModelRef::parse(&raw).map_err(serde::de::Error::custom)
    }
}

// ── config source ───────────────────────────────────────────────────────────

#[derive(Debug, Clone)]
pub enum ConfigSource {
    Project(PathBuf),
    User(PathBuf),
}

impl fmt::Display for ConfigSource {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            ConfigSource::Project(p) => write!(f, "project ({})", p.display()),
            ConfigSource::User(p) => write!(f, "user ({})", p.display()),
        }
    }
}

// ── resolver ────────────────────────────────────────────────────────────────

/// Resolves a `ModelRef` to a list of `Binding`s to try in order.
#[derive(Debug, Clone)]
pub struct Resolver {
    file: ModelsFile,
    source: ConfigSource,
}

impl Resolver {
    pub fn from_loaded(file: ModelsFile, source: ConfigSource) -> Self {
        Self { file, source }
    }

    pub fn source(&self) -> &ConfigSource {
        &self.source
    }

    pub fn file(&self) -> &ModelsFile {
        &self.file
    }

    /// Returns the binding list the resolver chose for `delegate`, plus
    /// the level it was found at (for the `MODEL_RESOLVER_WALK` audit
    /// event the caller emits). `ModelResolutionExhausted` if no level
    /// matched.
    ///
    /// Honors `strict_specificity` on the file: when set, a delegate
    /// that asked for `<affinity>-<tier>` must be matched by an exact
    /// key — otherwise this returns `ModelResolutionExhausted` with the
    /// strict-mode marker in `walked_levels`.
    pub fn walk(
        &self,
        delegate: &ModelRef,
    ) -> Result<(Cow<'_, [Binding]>, String), ModelResolutionExhausted> {
        let mut walked = Vec::new();
        let strict = self.file.strict_specificity;
        let asks_full = delegate.affinity.is_some() && delegate.tier.is_some();
        let mut first_iteration = true;

        for (key, label) in candidate_keys(delegate) {
            if let Some(bindings) = self.file.overrides.get(&key) {
                walked.push(format!("{label} (matched)"));
                return Ok((Cow::Borrowed(bindings.as_slice()), label));
            }
            if first_iteration && strict && asks_full {
                // Strict mode + full delegate (affinity-tier) + first
                // (exact-match) key missed → abort. Don't walk further.
                walked.push(format!("{label} [strict: not found]"));
                return Err(ModelResolutionExhausted {
                    delegate: delegate.to_string(),
                    walked_levels: walked,
                    attempts: Vec::new(),
                });
            }
            walked.push(format!("{label} (not found)"));
            first_iteration = false;
        }
        if self.file.default.is_empty() {
            walked.push("default (empty)".to_string());
            return Err(ModelResolutionExhausted {
                delegate: delegate.to_string(),
                walked_levels: walked,
                attempts: Vec::new(),
            });
        }
        walked.push("default (matched)".to_string());
        Ok((
            Cow::Borrowed(self.file.default.as_slice()),
            "default".to_string(),
        ))
    }

    /// Pick the next binding to try given prior failures. Walks the
    /// list, skipping indices that already failed; returns the first
    /// untried binding OR a structured exhaustion error.
    ///
    /// Defense-in-depth: if any entry in `prior_failures` is a
    /// non-infrastructure (content) class, surface immediately as
    /// `ModelResolutionExhausted` rather than advancing. Callers are
    /// expected to short-circuit on content failures before re-entering
    /// `try_next`, but the check here prevents the "no silent fallback"
    /// invariant from depending on caller discipline alone (FMECA R1).
    pub fn try_next<'a>(
        &self,
        delegate: &ModelRef,
        bindings: &'a [Binding],
        prior_failures: &[(usize, FailureClass, String)],
    ) -> Result<(usize, &'a Binding), ModelResolutionExhausted> {
        let has_content_failure = prior_failures
            .iter()
            .any(|(_, class, _)| !class.is_infrastructure());
        if !has_content_failure {
            let next_idx = prior_failures
                .iter()
                .map(|(i, _, _)| *i + 1)
                .max()
                .unwrap_or(0);
            if let Some(b) = bindings.get(next_idx) {
                return Ok((next_idx, b));
            }
        }
        let attempts: Vec<AttemptRecord> = prior_failures
            .iter()
            .map(|(i, class, detail)| AttemptRecord {
                binding: bindings[*i].clone(),
                class: *class,
                detail: detail.clone(),
            })
            .collect();
        Err(ModelResolutionExhausted {
            delegate: delegate.to_string(),
            walked_levels: vec!["(see attempts)".to_string()],
            attempts,
        })
    }
}

/// Walk order for the specificity match: full, affinity-only, tier-only.
/// Tie-break: affinity beats tier (so a delegate `coding-frontier` with
/// both `coding` and `frontier` defined picks `coding`).
fn candidate_keys(delegate: &ModelRef) -> Vec<(OverrideKey, String)> {
    let mut out = Vec::new();
    if let (Some(a), Some(t)) = (delegate.affinity, delegate.tier) {
        out.push((
            OverrideKey {
                affinity: Some(a),
                tier: Some(t),
            },
            format!("{a}-{t}"),
        ));
    }
    if let Some(a) = delegate.affinity {
        out.push((
            OverrideKey {
                affinity: Some(a),
                tier: None,
            },
            format!("{a}"),
        ));
    }
    if let Some(t) = delegate.tier {
        out.push((
            OverrideKey {
                affinity: None,
                tier: Some(t),
            },
            format!("{t}"),
        ));
    }
    out
}

// ── resolution exhaustion ──────────────────────────────────────────────────

#[derive(Debug, thiserror::Error)]
#[error(
    "model resolution exhausted for delegate `{delegate}`. Walked: {walked_levels:?}. \
     Attempts: {attempts:?}"
)]
pub struct ModelResolutionExhausted {
    pub delegate: String,
    pub walked_levels: Vec<String>,
    pub attempts: Vec<AttemptRecord>,
}

#[derive(Debug, Clone)]
pub struct AttemptRecord {
    pub binding: Binding,
    pub class: FailureClass,
    pub detail: String,
}
