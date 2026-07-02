//! Provider `/models` live auth-probe — the single source of truth for
//! "is this binding's credential actually accepted by the provider right
//! now?".
//!
//! This is the shared probe machinery used by BOTH:
//!
//! - **preflight** (`model_resolver::preflight`) — a 401/403 on a primary
//!   binding blocks startup (FMECA U2 / H11a auth gap, now closed).
//! - **the TUI doctor** (`doctor_probe_cache`) — caches the per-binding
//!   listing status so operators get a "last known good" timestamp and a
//!   stale-cache warning.
//!
//! Both used to carry their own copy of this logic; it now lives here once
//! and they delegate. Per-provider strategy:
//!
//! - **Anthropic** — `GET /v1/models` with `x-api-key`. Response body has
//!   `data: [{id, ...}]`; we check the model is listed.
//! - **OpenAI** — `GET /v1/models` with `Authorization: Bearer ...`. Same
//!   `data: [{id, ...}]` shape.
//! - **Gemini** — `GET /v1beta/models?key=...`. Response has
//!   `models: [{name: "models/<id>"}]`. Reads `GEMINI_API_KEY` — the same
//!   var the runtime request path uses (CMP-005).
//! - **OpenRouter** — `GET /api/v1/models` with `Authorization: Bearer ...`
//!   (OpenAI-compatible `data: [{id, ...}]` shape). Probed for real so a
//!   dead key can't pass as healthy (PROBE-02).
//! - **Ollama / llama.cpp / Bedrock / Custom** — skipped (no auth/listing
//!   convention we can rely on).
//!
//! Per-provider base URLs default to the const here and honor a
//! `*_BASE_URL` env override so OpenAI-compatible / self-hosted gateways
//! and tests can redirect the probe.

use crate::model_resolver::config::{Binding, Provider};
use crate::providers::ProviderId;

/// Outcome of a single provider `/models` listing probe. Variant names
/// match the doctor's on-disk `ProbeStatus` 1:1 (the doctor maps via
/// `From`), but this core enum carries NO serde — it's an in-process
/// transport type, not a persisted one.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ListingStatus {
    /// Provider returned 200 and the model id was in the response.
    Ok,
    /// Provider returned 200 but the model id was NOT in the response.
    /// Strong signal the model is deprecated / renamed.
    ModelNotListed,
    /// Auth failed (401/403). The credential is present but rejected.
    AuthFailed,
    /// Network-level failure (timeout, DNS, connection reset) or a non-2xx
    /// non-auth status. No signal about the model itself.
    Unreachable,
    /// Provider's response shape wasn't what we expected. Likely a provider
    /// API change; needs investigation.
    UnexpectedResponse,
    /// No API key in env; the binding is unprobeable without one.
    NoCredential,
    /// Provider class (Ollama / llama.cpp / Bedrock / Custom) where we don't
    /// implement a probe. Skipped, not failed.
    Skipped,
}

const ANTHROPIC_DEFAULT_BASE: &str = "https://api.anthropic.com";
const OPENAI_DEFAULT_BASE: &str = "https://api.openai.com";
const GEMINI_DEFAULT_BASE: &str = "https://generativelanguage.googleapis.com";
const OPENROUTER_DEFAULT_BASE: &str = "https://openrouter.ai/api";

/// Build a probe client with sane connect/request timeouts so a wedged
/// provider can't hang preflight or doctor indefinitely. Falls back to a
/// default client (without configured timeouts) only if the builder fails.
pub fn probe_client() -> reqwest::Client {
    reqwest::Client::builder()
        .connect_timeout(std::time::Duration::from_secs(5))
        .timeout(std::time::Duration::from_secs(15))
        .build()
        .unwrap_or_default()
}

/// Single-binding live probe against the provider's `/models` listing.
/// Returns the listing status plus a human-readable detail string.
pub async fn probe_binding(client: &reqwest::Client, binding: &Binding) -> (ListingStatus, String) {
    let id = match &binding.provider {
        Provider::Known(id) => *id,
        Provider::Custom { .. } => {
            return (
                ListingStatus::Skipped,
                "custom provider — no listing convention".into(),
            );
        }
    };
    match id {
        ProviderId::Anthropic => {
            let Ok(key) = std::env::var("ANTHROPIC_API_KEY") else {
                return (
                    ListingStatus::NoCredential,
                    "ANTHROPIC_API_KEY not set".into(),
                );
            };
            let base = std::env::var("ANTHROPIC_BASE_URL")
                .unwrap_or_else(|_| ANTHROPIC_DEFAULT_BASE.into());
            classify_listing(
                client
                    .get(format!("{base}/v1/models"))
                    .header("x-api-key", key)
                    .header("anthropic-version", "2023-06-01")
                    .send()
                    .await,
                &binding.model,
                |body: &serde_json::Value| {
                    body.pointer("/data").and_then(|d| d.as_array()).map(|arr| {
                        arr.iter()
                            .any(|m| m.get("id").and_then(|v| v.as_str()) == Some(&binding.model))
                    })
                },
            )
            .await
        }
        ProviderId::Openai => {
            let Ok(key) = std::env::var("OPENAI_API_KEY") else {
                return (ListingStatus::NoCredential, "OPENAI_API_KEY not set".into());
            };
            let base =
                std::env::var("OPENAI_BASE_URL").unwrap_or_else(|_| OPENAI_DEFAULT_BASE.into());
            classify_listing(
                client
                    .get(format!("{base}/v1/models"))
                    .bearer_auth(key)
                    .send()
                    .await,
                &binding.model,
                |body: &serde_json::Value| {
                    body.pointer("/data").and_then(|d| d.as_array()).map(|arr| {
                        arr.iter()
                            .any(|m| m.get("id").and_then(|v| v.as_str()) == Some(&binding.model))
                    })
                },
            )
            .await
        }
        ProviderId::Gemini => {
            // aether-llm's gemini provider reads GEMINI_API_KEY (CMP-005),
            // so the live probe must check the same var the request path uses.
            let Ok(key) = std::env::var("GEMINI_API_KEY") else {
                return (ListingStatus::NoCredential, "GEMINI_API_KEY not set".into());
            };
            let base =
                std::env::var("GOOGLE_BASE_URL").unwrap_or_else(|_| GEMINI_DEFAULT_BASE.into());
            classify_listing(
                client
                    .get(format!("{base}/v1beta/models"))
                    .query(&[("key", &key)])
                    .send()
                    .await,
                &binding.model,
                |body: &serde_json::Value| {
                    body.pointer("/models")
                        .and_then(|m| m.as_array())
                        .map(|arr| {
                            arr.iter().any(|m| {
                                m.get("name")
                                    .and_then(|v| v.as_str())
                                    .map(|n| n.ends_with(&binding.model))
                                    .unwrap_or(false)
                            })
                        })
                },
            )
            .await
        }
        ProviderId::Openrouter => {
            // PROBE-02 — OpenRouter exposes a live, OpenAI-compatible
            // `GET /api/v1/models` (Authorization: Bearer; body
            // `data: [{id, ...}]`). Reporting Skipped/green here masked a
            // dead key behind a passing health check, so we probe it for
            // real against the same `OPENROUTER_API_KEY` the request path uses.
            let Ok(key) = std::env::var("OPENROUTER_API_KEY") else {
                return (
                    ListingStatus::NoCredential,
                    "OPENROUTER_API_KEY not set".into(),
                );
            };
            let base = std::env::var("OPENROUTER_BASE_URL")
                .unwrap_or_else(|_| OPENROUTER_DEFAULT_BASE.into());
            classify_listing(
                client
                    .get(format!("{base}/v1/models"))
                    .bearer_auth(key)
                    .send()
                    .await,
                &binding.model,
                |body: &serde_json::Value| {
                    body.pointer("/data").and_then(|d| d.as_array()).map(|arr| {
                        arr.iter()
                            .any(|m| m.get("id").and_then(|v| v.as_str()) == Some(&binding.model))
                    })
                },
            )
            .await
        }
        ProviderId::Ollama | ProviderId::Llamacpp | ProviderId::Bedrock => (
            ListingStatus::Skipped,
            "no live model-listing probe wired for this provider yet".into(),
        ),
    }
}

async fn classify_listing(
    result: Result<reqwest::Response, reqwest::Error>,
    model_name: &str,
    listed: impl FnOnce(&serde_json::Value) -> Option<bool>,
) -> (ListingStatus, String) {
    let resp = match result {
        Ok(r) => r,
        Err(e) => {
            return (ListingStatus::Unreachable, format!("transport error: {e}"));
        }
    };
    let status = resp.status();
    match status.as_u16() {
        401 | 403 => (
            ListingStatus::AuthFailed,
            format!("HTTP {} — credential rejected", status.as_u16()),
        ),
        200 => {
            let body: serde_json::Value = match resp.json().await {
                Ok(v) => v,
                Err(e) => {
                    return (
                        ListingStatus::UnexpectedResponse,
                        format!("body not JSON: {e}"),
                    );
                }
            };
            match listed(&body) {
                Some(true) => (ListingStatus::Ok, format!("`{model_name}` listed")),
                Some(false) => (
                    ListingStatus::ModelNotListed,
                    format!("`{model_name}` NOT in /models response"),
                ),
                None => (
                    ListingStatus::UnexpectedResponse,
                    "no `data` / `models` array in response".into(),
                ),
            }
        }
        other => (
            ListingStatus::Unreachable,
            format!("HTTP {other} from provider"),
        ),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model_resolver::config::ProviderFeatures;

    fn binding(p: Provider, model: &str) -> Binding {
        Binding {
            provider: p,
            model: model.into(),
            features: ProviderFeatures::None,
        }
    }

    /// Custom providers carry no listing convention → Skipped without I/O.
    #[tokio::test]
    async fn custom_provider_is_skipped() {
        let client = probe_client();
        let b = binding(
            Provider::Custom {
                endpoint: "https://x.example".into(),
            },
            "any",
        );
        let (status, _) = probe_binding(&client, &b).await;
        assert_eq!(status, ListingStatus::Skipped);
    }

    /// Local providers have no auth/listing convention → Skipped, no I/O.
    #[tokio::test]
    async fn local_providers_are_skipped() {
        let client = probe_client();
        for p in [
            Provider::Known(ProviderId::Ollama),
            Provider::Known(ProviderId::Llamacpp),
            Provider::Known(ProviderId::Bedrock),
        ] {
            let b = binding(p.clone(), "any");
            let (status, _) = probe_binding(&client, &b).await;
            assert_eq!(status, ListingStatus::Skipped, "{p:?} should skip");
        }
    }

    /// A cloud provider with no credential short-circuits to NoCredential
    /// before any network call.
    #[tokio::test]
    async fn cloud_provider_without_credential_is_no_credential() {
        std::env::remove_var("ANTHROPIC_API_KEY");
        let client = probe_client();
        let b = binding(Provider::Known(ProviderId::Anthropic), "claude-sonnet-4-6");
        let (status, detail) = probe_binding(&client, &b).await;
        assert_eq!(status, ListingStatus::NoCredential);
        assert!(detail.contains("ANTHROPIC_API_KEY"));
    }
}
