//! Live auth preflight for primary bindings — FMECA U2 mitigation.
//!
//! At workflow load, every distinct primary (index-0) `Binding`
//! referenced by any `delegate:` state is probed once. The probe is a
//! real `GET /models` auth check against the provider (shared with the
//! TUI doctor via `model_resolver::provider_probe`):
//!
//! - **Missing credential** (env var unset/empty) → `MissingCredential`,
//!   a startup error. No network call is made when there's no key.
//! - **401 / 403** (present-but-revoked key) → `Fail` → `PrimaryAuthFailed`,
//!   a startup error.
//! - **Transient / model-not-listed / unexpected** (timeout, DNS, 5xx,
//!   404, schema surprise) → `Warn`, logged and non-blocking.
//!
//! ## H11a / FMECA U2 — CLOSED
//!
//! The historical gap (presence-only check let a present-but-revoked key
//! reach the runtime) is closed: a 401/403 on a primary now blocks
//! startup. The per-provider base-URL table, auth headers and `/models`
//! endpoints live in `provider_probe`, which is the single source of
//! truth shared by preflight and the doctor (no duplication).
//!
//! ## FMECA U2 — only auth blocks startup
//!
//! ONLY `Fail` (401/403) and `MissingCredential` block startup. Network
//! errors, 5xx, 429, model-not-listed and unexpected-response are `Warn`:
//! startup must not be held hostage to a flaky network — the resolver's
//! runtime CoR routes around transient failures.
//!
//! `PRAXEC_SKIP_PREFLIGHT=1` is an escape hatch for CI / disconnected
//! dev. Skipped runs log a single line so the operator knows preflight
//! was bypassed.

use std::collections::BTreeSet;

use super::classify::FailureClass;
use super::config::{Binding, Provider};
use super::walk::{ModelRef, Resolver};

/// Per-binding preflight outcome.
#[derive(Debug)]
pub enum PreflightError {
    /// Primary binding's auth was rejected (401 or 403). Hard failure —
    /// startup must not proceed; the operator's API key for this
    /// provider needs fixing before any workflow can run.
    PrimaryAuthFailed {
        delegate: String,
        binding: Binding,
        class: FailureClass,
        detail: String,
    },
    /// Primary binding's preflight couldn't complete because the env
    /// var carrying the API key isn't set. Hard failure — same shape as
    /// auth-failed; we don't probe at all if there's no credential.
    MissingCredential {
        delegate: String,
        binding: Binding,
        env_var: &'static str,
    },
}

impl std::fmt::Display for PreflightError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            PreflightError::PrimaryAuthFailed {
                delegate,
                binding,
                class,
                detail,
            } => write!(
                f,
                "preflight: primary binding for `{delegate}` failed auth: provider={} model={} \
                 class={class:?} detail={detail}",
                binding.provider.display_name(),
                binding.model
            ),
            PreflightError::MissingCredential {
                delegate,
                binding,
                env_var,
            } => write!(
                f,
                "preflight: primary binding for `{delegate}` requires ${env_var} (provider={} \
                 model={}). Set the env var or use `PRAXEC_SKIP_PREFLIGHT=1` to bypass.",
                binding.provider.display_name(),
                binding.model
            ),
        }
    }
}

impl std::error::Error for PreflightError {}

/// Outcome of a single preflight probe. Public so doctor can re-use the
/// same machinery to surface preflight state without halting startup.
#[derive(Debug)]
pub enum PreflightOutcome {
    /// 200 OK — credentials valid.
    Ok,
    /// 429 / 404 / network — transient or recoverable; warn and continue.
    Warn { class: FailureClass, detail: String },
    /// 401 / 403 — hard failure; surface as `PrimaryAuthFailed`.
    Fail { class: FailureClass, detail: String },
    /// Required env var missing.
    MissingCredential { env_var: &'static str },
}

/// Classify a probe outcome into a startup error (if any) for the given
/// (label, binding). Pure function so the warn-vs-fail dispatch logic is
/// testable without HTTP plumbing.
///
/// FMECA U2: only `Fail` and `MissingCredential` block startup. `Warn`
/// (429/404/transient network) is logged at the call site and lets
/// startup proceed — the resolver's runtime CoR will route around it.
pub fn classify_outcome(
    label: &str,
    binding: &Binding,
    outcome: PreflightOutcome,
) -> Option<PreflightError> {
    match outcome {
        PreflightOutcome::Ok => None,
        PreflightOutcome::Warn { class, detail } => {
            tracing::warn!(
                target: "praxec.model_resolver",
                label = %label,
                provider = binding.provider.display_name(),
                model = %binding.model,
                ?class,
                %detail,
                "primary preflight: transient — runtime CoR will handle"
            );
            None
        }
        PreflightOutcome::Fail { class, detail } => Some(PreflightError::PrimaryAuthFailed {
            delegate: label.to_string(),
            binding: binding.clone(),
            class,
            detail,
        }),
        PreflightOutcome::MissingCredential { env_var } => {
            Some(PreflightError::MissingCredential {
                delegate: label.to_string(),
                binding: binding.clone(),
                env_var,
            })
        }
    }
}

/// Verify primary bindings for the given delegates. Returns Ok(()) if
/// every primary either probed Ok or warned; returns a list of all
/// failures otherwise.
///
/// Honors `PRAXEC_SKIP_PREFLIGHT=1` — when set, returns Ok(()) without
/// probing.
pub async fn verify_primary_bindings(
    resolver: &Resolver,
    delegates: &[ModelRef],
) -> Result<(), Vec<PreflightError>> {
    if std::env::var("PRAXEC_SKIP_PREFLIGHT").as_deref() == Ok("1") {
        tracing::info!(
            target: "praxec.model_resolver",
            "preflight skipped (PRAXEC_SKIP_PREFLIGHT=1)"
        );
        return Ok(());
    }
    let client = crate::model_resolver::provider_probe::probe_client();
    let mut errors = Vec::new();
    let mut seen: BTreeSet<(String, String)> = BTreeSet::new();
    for d in delegates {
        let (bindings, _) = match resolver.walk(d) {
            Ok(x) => x,
            Err(_) => continue, // resolution failures are doctor's problem, not preflight's
        };
        let Some(primary) = bindings.first() else {
            continue;
        };
        let key = (
            primary.provider.display_name().to_string(),
            primary.model.clone(),
        );
        if !seen.insert(key) {
            continue;
        }
        let outcome = probe_binding(&client, primary).await;
        if let Some(err) = classify_outcome(&d.to_string(), primary, outcome) {
            errors.push(err);
        }
    }
    if errors.is_empty() {
        Ok(())
    } else {
        Err(errors)
    }
}

/// Probe a single binding's credential against the provider's `/models`
/// listing — a REAL auth check, not just presence.
///
/// Logic:
/// 1. Fast-path presence: if the provider has an API-key env var and it's
///    unset/empty → `MissingCredential` with NO network call. Providers
///    with no key (Ollama, llama.cpp, unauthenticated custom) fall through
///    to the probe (which returns Skipped → `Ok`).
/// 2. Otherwise run `provider_probe::probe_binding` and map its
///    `ListingStatus` to a `PreflightOutcome`:
///    - `Ok`/`Skipped` → `Ok`
///    - `AuthFailed` (401/403) → `Fail { Auth401 }` (blocks startup)
///    - `NoCredential` → `MissingCredential` (defensive; presence checked above)
///    - `ModelNotListed` → `Warn { NotFound404 }`
///    - `Unreachable` → `Warn { NetworkTimeout }`
///    - `UnexpectedResponse` → `Warn { ContentSchema }`
///
/// FMECA U2: only `Fail` and `MissingCredential` block startup; every
/// transient/model/schema outcome is a non-blocking `Warn`.
pub async fn probe_binding(client: &reqwest::Client, binding: &Binding) -> PreflightOutcome {
    use crate::model_resolver::provider_probe::{self, ListingStatus};

    // Fast-path: no network call when there's a required-but-absent key.
    if let Some(var) = api_key_env_for(&binding.provider) {
        let present = std::env::var(var)
            .map(|v| !v.trim().is_empty())
            .unwrap_or(false);
        if !present {
            return PreflightOutcome::MissingCredential { env_var: var };
        }
    }

    let (status, detail) = provider_probe::probe_binding(client, binding).await;
    match status {
        ListingStatus::Ok | ListingStatus::Skipped => PreflightOutcome::Ok,
        ListingStatus::AuthFailed => PreflightOutcome::Fail {
            class: FailureClass::Auth401,
            detail,
        },
        ListingStatus::NoCredential => PreflightOutcome::MissingCredential {
            env_var: api_key_env_for(&binding.provider).unwrap_or("<unknown>"),
        },
        ListingStatus::ModelNotListed => PreflightOutcome::Warn {
            class: FailureClass::NotFound404,
            detail,
        },
        ListingStatus::Unreachable => PreflightOutcome::Warn {
            class: FailureClass::NetworkTimeout,
            detail,
        },
        ListingStatus::UnexpectedResponse => PreflightOutcome::Warn {
            class: FailureClass::ContentSchema,
            detail,
        },
    }
}

/// Workflow-agnostic preflight. Probes the primary binding of every
/// override list + the default's primary, dedup'd by (provider, model).
///
/// PR1 scoping: rather than parse the workflow YAML to extract its
/// `delegate:` set, this probes every declared primary in `models.yaml`.
/// Slightly broader than strictly necessary, but catches "you wrote a
/// binding for `coding-frontier` but forgot the API key" at startup
/// regardless of which workflow the operator is about to run.
///
/// A workflow-aware variant (`verify_primary_bindings(&[ModelRef])`)
/// is available for callers that already know the delegate set.
pub async fn verify_all_primary_bindings(resolver: &Resolver) -> Result<(), Vec<PreflightError>> {
    if std::env::var("PRAXEC_SKIP_PREFLIGHT").as_deref() == Ok("1") {
        tracing::info!(
            target: "praxec.model_resolver",
            "preflight skipped (PRAXEC_SKIP_PREFLIGHT=1)"
        );
        return Ok(());
    }
    let client = crate::model_resolver::provider_probe::probe_client();
    let mut errors = Vec::new();
    let mut seen: BTreeSet<(String, String)> = BTreeSet::new();
    let file = resolver.file();

    let mut all: Vec<(String, &Binding)> = Vec::new();
    if let Some(b) = file.default.first() {
        all.push(("default".to_string(), b));
    }
    for (key, list) in &file.overrides {
        if let Some(b) = list.members().first() {
            all.push((key.to_string(), b));
        }
    }

    for (label, b) in all {
        let key = (b.provider.display_name().to_string(), b.model.clone());
        if !seen.insert(key) {
            continue;
        }
        let outcome = probe_binding(&client, b).await;
        if let Some(err) = classify_outcome(&label, b, outcome) {
            errors.push(err);
        }
    }
    if errors.is_empty() {
        Ok(())
    } else {
        Err(errors)
    }
}

/// Map a provider to the env var carrying its API key. This is the
/// single source of truth for provider→API-key-env-var; callers delegate
/// here (via [`api_key_env_for_slug`]) rather than re-encoding their own
/// tables — see CMP-005 / CMP-026.
pub fn api_key_env_for(p: &Provider) -> Option<&'static str> {
    match p {
        Provider::Known(id) => id.credentials().primary(),
        Provider::Custom { .. } => None,
    }
}

/// Slug-keyed form. Accepts a canonical catalog slug (e.g. `"gemini"`).
///
/// Note: aether-llm — the runtime provider — reads `GEMINI_API_KEY` for the
/// Gemini provider (see `aether-llm` `providers/gemini/provider.rs` and its
/// catalog), NOT `GOOGLE_API_KEY`. The catalog slug `"gemini"` resolves to that
/// same var so preflight, the live probe, and `set-provider-keys` agree with
/// what the actual LLM call consumes (CMP-005). Legacy slugs (`"google"`,
/// `"lmstudio"`) are not catalog members and return `None`.
pub fn api_key_env_for_slug(slug: &str) -> Option<&'static str> {
    crate::providers::ProviderId::from_slug(slug).and_then(|p| p.credentials().primary())
}

#[cfg(test)]
mod u2_auth_probe_tests {
    use super::*;
    use crate::model_resolver::provider_probe::probe_client;
    use crate::providers::ProviderId;

    fn binding(p: Provider) -> Binding {
        Binding {
            provider: p,
            model: "test-model".into(),
            features: Default::default(),
        }
    }

    /// A required-but-absent credential short-circuits to
    /// `MissingCredential` BEFORE any network call. (If a network call were
    /// made for a missing key this test would still pass, but the
    /// no-network contract is what makes preflight safe offline; the probe
    /// itself also short-circuits on absent keys.)
    #[tokio::test]
    async fn missing_credential_is_returned_without_a_network_call() {
        // FIXME: Audit that the environment access only happens in single-threaded code.
        unsafe { std::env::remove_var("OPENAI_API_KEY") };
        let client = probe_client();
        let outcome = probe_binding(&client, &binding(Provider::Known(ProviderId::Openai))).await;
        assert!(
            matches!(
                outcome,
                PreflightOutcome::MissingCredential {
                    env_var: "OPENAI_API_KEY"
                }
            ),
            "absent key → MissingCredential without a probe: {outcome:?}"
        );
    }

    /// Keyless providers (Ollama and friends) have no env var and the
    /// provider probe Skips them → `Ok`, no network dependency.
    #[tokio::test]
    async fn keyless_provider_is_ok() {
        let client = probe_client();
        let outcome = probe_binding(&client, &binding(Provider::Known(ProviderId::Ollama))).await;
        assert!(
            matches!(outcome, PreflightOutcome::Ok),
            "keyless provider → Ok: {outcome:?}"
        );
    }

    /// Custom (keyless) providers similarly resolve to `Ok` via Skipped.
    #[tokio::test]
    async fn custom_keyless_provider_is_ok() {
        let client = probe_client();
        let b = Binding {
            provider: Provider::Custom {
                endpoint: "https://x.example".into(),
            },
            model: "any".into(),
            features: Default::default(),
        };
        let outcome = probe_binding(&client, &b).await;
        assert!(
            matches!(outcome, PreflightOutcome::Ok),
            "custom keyless provider → Ok: {outcome:?}"
        );
    }

    // ── classify_outcome: pure warn-vs-fail dispatch (no network) ──────────

    fn any_binding() -> Binding {
        binding(Provider::Known(ProviderId::Anthropic))
    }

    /// An auth-failure-derived `Fail` blocks startup (PrimaryAuthFailed).
    #[test]
    fn classify_fail_blocks_startup() {
        let out = PreflightOutcome::Fail {
            class: FailureClass::Auth401,
            detail: "HTTP 401 — credential rejected".into(),
        };
        let err = classify_outcome("default", &any_binding(), out);
        assert!(
            matches!(err, Some(PreflightError::PrimaryAuthFailed { .. })),
            "401 Fail must block startup: {err:?}"
        );
    }

    /// `MissingCredential` blocks startup.
    #[test]
    fn classify_missing_credential_blocks_startup() {
        let out = PreflightOutcome::MissingCredential {
            env_var: "ANTHROPIC_API_KEY",
        };
        let err = classify_outcome("default", &any_binding(), out);
        assert!(
            matches!(err, Some(PreflightError::MissingCredential { .. })),
            "missing credential must block startup: {err:?}"
        );
    }

    /// `Warn` is non-blocking (transient/model/schema → runtime CoR).
    #[test]
    fn classify_warn_does_not_block_startup() {
        for class in [
            FailureClass::NetworkTimeout,
            FailureClass::NotFound404,
            FailureClass::ContentSchema,
        ] {
            let out = PreflightOutcome::Warn {
                class,
                detail: "transient".into(),
            };
            assert!(
                classify_outcome("default", &any_binding(), out).is_none(),
                "Warn({class:?}) must not block startup"
            );
        }
    }

    /// `Ok` never blocks startup.
    #[test]
    fn classify_ok_does_not_block_startup() {
        assert!(classify_outcome("default", &any_binding(), PreflightOutcome::Ok).is_none());
    }
}
