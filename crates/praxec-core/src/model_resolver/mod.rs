//! Layered model configuration with Chain-of-Responsibility resolution.
//!
//! Replaces v0.2's single `--agent name=provider/model` registry with:
//!
//! - **`models.yaml`** at `.praxec/models.yaml` (project) or
//!   `~/.praxec/models.yaml` (user). Project wins whole-file.
//! - **Closed enums.** `Affinity` (5: coding, reasoning, prose, web-search,
//!   recon) × `Tier` (3: frontier, standard, commoditized). Workflows
//!   reference a `ModelRef` made from one or both.
//! - **Sparse overrides** keyed by `<affinity>-<tier>`, `<affinity>`, or
//!   `<tier>`. One mandatory `default:` list catches anything unmatched.
//! - **Per-list Chain of Responsibility.** Each override list is tried in
//!   order; only *infrastructure* failures (401/403/429/404/network/timeout)
//!   trigger fall-through. Content failures surface to the caller.
//!
//! Safety properties (every one is FMECA-vetted — see
//! the design plan):
//!
//! 1. Unknown response status defaults to `ContentOther` (surface, never
//!    fall through). `classify::FailureClass::from_response` test-pins this.
//! 2. Missing `default:` field fails at load (no `#[serde(default)]`).
//! 3. Primary (index-0) bindings auth-probed once at workflow load via
//!    `preflight::verify_primary_bindings`. 401/403 → startup error.
//! 4. CLI `--agent` flag and an on-disk `models.yaml` are mutually
//!    exclusive — both set → `AmbiguousModelSource` startup error.
//! 5. `strict_specificity: true` opt-in turns specificity-walk fall-through
//!    into a load-time error (poka-yoke for operators who want exact-match
//!    semantics only).

pub mod classify;
pub mod config;
pub mod preflight;
pub mod provider_probe;
pub mod walk;

pub use classify::FailureClass;
pub use config::{
    Affinity, AffinityScores, AnthropicFeatures, Binding, GoogleFeatures, ModelConfigError,
    ModelsFile, OpenAIFeatures, OverrideKey, Provider, ProviderFeatures, Tier, affinity_fit,
};
pub use preflight::{
    PreflightError, api_key_env_for, api_key_env_for_slug, verify_all_primary_bindings,
    verify_primary_bindings,
};
pub use walk::{
    AttemptRecord, ConfigSource, ModelRef, ModelRefParseError, ModelResolutionExhausted, Resolver,
};

/// FMECA T1: refuse to start when both `--agent` CLI flags AND an
/// on-disk `models.yaml` are present. Picking one silently would mask
/// operator intent — surfacing the ambiguity is the only safe choice.
///
/// Pure function so `main.rs` can call it and tests can exercise the
/// poka-yoke without shelling out to the binary.
#[derive(Debug, thiserror::Error)]
#[error(
    "ambiguous model source: both `--agent` CLI flag(s) AND an models.yaml file are present. \
     Choose one — models.yaml takes precedence going forward; the `--agent` flag is deprecated. \
     See /guides/agent-config.mdx for the migration path."
)]
pub struct AmbiguousModelSourceError;

pub fn validate_model_source_exclusivity(
    has_yaml: bool,
    has_cli_model_flag: bool,
) -> Result<(), AmbiguousModelSourceError> {
    if has_yaml && has_cli_model_flag {
        Err(AmbiguousModelSourceError)
    } else {
        Ok(())
    }
}

/// Validate an `models.yaml` file at an arbitrary path by loading it
/// through `ModelsFile::from_path` — exactly the same path the
/// resolver uses at workflow start. Returns a stable JSON envelope for
/// round-trip validation of authored configs.
///
/// On success: `{"ok": true, "summary": "..."}`. On failure:
/// `{"ok": false, "error_kind": "<variant>", "detail": "<rendered>"}`.
/// `error_kind` is one of: `MISSING_DEFAULT`, `EMPTY_DEFAULT`,
/// `MISSING_PROVIDER_MODEL`, `UNKNOWN_OVERRIDE_KEY`,
/// `UNKNOWN_FEATURE_KEY`, `PROVIDER_ENDPOINT_REQUIRED`,
/// `VERSION_MISMATCH`, `YAML_SYNTAX`, `IO`. The kind is the
/// stable contract; the detail is for humans.
pub fn validate_models_config_envelope(path: &std::path::Path) -> serde_json::Value {
    match ModelsFile::from_path(path) {
        Ok(file) => serde_json::json!({
            "ok": true,
            "summary": format!(
                "{} default binding(s), {} override list(s), strict_specificity={}",
                file.default.len(),
                file.overrides.len(),
                file.strict_specificity,
            ),
        }),
        Err(e) => {
            let kind = match &e {
                ModelConfigError::MissingDefault => "MISSING_DEFAULT",
                ModelConfigError::EmptyDefault => "EMPTY_DEFAULT",
                ModelConfigError::MissingProviderModel => "MISSING_PROVIDER_MODEL",
                ModelConfigError::UnknownOverrideKey(_) => "UNKNOWN_OVERRIDE_KEY",
                ModelConfigError::UnknownFeatureKey { .. } => "UNKNOWN_FEATURE_KEY",
                ModelConfigError::ProviderEndpointRequired => "PROVIDER_ENDPOINT_REQUIRED",
                ModelConfigError::VersionMismatch { .. } => "VERSION_MISMATCH",
                ModelConfigError::YamlSyntax(_) => "YAML_SYNTAX",
                ModelConfigError::Io(_) => "IO",
            };
            serde_json::json!({
                "ok": false,
                "error_kind": kind,
                "detail": e.to_string(),
            })
        }
    }
}
