//! SPEC §33 D9 — affinity → concrete-model resolution seam.
//!
//! `LlmExecutor` calls an injected [`AffinityResolver`] when a `kind: llm`
//! executor config sets `affinity:` instead of a literal `model:`. The default
//! [`RejectingAffinityResolver`] fails loud (preserving pre-D9 behavior); the
//! production resolver (wired by the binary) reuses the models.yaml resolution.

use async_trait::async_trait;
use praxec_core::error::ExecutorError;

/// Resolve an `affinity:` name to a concrete `"provider:model-id"` string.
///
/// This resolves the **agent** slot of the agent/skill/prompt contract
/// (SPEC §33.12): the model that runs, and nothing else. Instructions come
/// from skills (the system message), the task from `prompt_template` (the
/// user message) — neither passes through here.
#[async_trait]
pub trait AffinityResolver: Send + Sync {
    async fn resolve(&self, affinity: &str) -> Result<String, ExecutorError>;

    /// Resolve a behavior requirement (`affinity`/`tier`/`effort`) to a POOL of
    /// `(provider, model, account)` members for load-balanced routing (spec #2).
    /// Default: **not supported** — the production `models.yaml` resolver
    /// overrides it. Keeps the pool path opt-in without breaking existing
    /// resolvers (a `strategy:` config with a non-pool resolver fails loud here).
    async fn resolve_pool(
        &self,
        _spec: &praxec_core::model_resolver::ModelRef,
    ) -> Result<Vec<praxec_core::pool_resolver::PoolMember>, ExecutorError> {
        Err(ExecutorError::Permanent(
            "LLM executor: `strategy:` requested pool routing, but the injected \
             resolver does not support it"
                .into(),
        ))
    }
}

/// Default — affinity is not wired. Fails loud (preserves pre-D9 behavior).
pub struct RejectingAffinityResolver;

#[async_trait]
impl AffinityResolver for RejectingAffinityResolver {
    async fn resolve(&self, affinity: &str) -> Result<String, ExecutorError> {
        Err(ExecutorError::Permanent(format!(
            "LLM executor: `affinity: {affinity}` cannot be resolved — no models.yaml \
             affinity resolver is wired into this gateway; set `model:` directly or \
             configure models.yaml + inject the resolver"
        )))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn rejecting_resolver_fails_loud() {
        let r = RejectingAffinityResolver;
        let err = r.resolve("coding-frontier").await.unwrap_err();
        assert!(
            format!("{err:?}").contains("affinity"),
            "must name affinity: {err:?}"
        );
    }
}
