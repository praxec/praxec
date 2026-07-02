//! Boundary guard: every in-build catalog slug must be a token aether-llm's
//! parser recognizes. Catches drift like the historical `google` vs `gemini`
//! mismatch and any aether version bump that renames/drops a provider token.

use llm::LlmError;
use praxec_core::providers::ProviderId;

#[tokio::test]
async fn every_catalog_slug_is_an_aether_token() {
    // Build aether's default parser (registers every built-in provider token).
    let parser = llm::parser::ModelProviderParser::default();
    for &p in ProviderId::ALL {
        if !p.available_in_build() {
            continue; // bedrock when its feature is off
        }
        let spec = format!("{}:probe-model", p.slug());
        let result = parser.parse(&spec).await;
        // We assert the slug ROUTED to a provider — NOT that construction
        // succeeded (it may fail on missing API keys, which is fine).
        if let Err(e) = result {
            // Precise match: aether emits LlmError::Other("Unknown provider: <name>")
            // for unrecognized tokens. Any other error means the provider was
            // recognized but failed for a legitimate reason (missing key, etc.).
            // This error string is stable under the `~0.7.6` patch-only workspace
            // pin (FMECA F13); revisit this match if aether's minor is ever bumped.
            if let LlmError::Other(ref msg) = e {
                assert!(
                    !msg.starts_with("Unknown provider:"),
                    "catalog slug `{}` is not a recognized aether provider token: {e:?}",
                    p.slug()
                );
            }
            // All other LlmError variants (MissingApiKey, ApiRequest, etc.)
            // mean the provider was found — construction just failed without
            // credentials, which is expected in a test environment.
        }
    }
}
