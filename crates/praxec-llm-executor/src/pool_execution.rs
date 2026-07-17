//! Additive requirement-driven POOL execution (spec #2 §6, executor-wiring).
//!
//! Given a resolved pool (`Vec<PoolMember>` + [`Strategy`]) and a [`TurnRequest`],
//! open a streaming turn over the pool via execution-policy's `RouterPolicy`:
//! health-aware failover (`ordered`) or weighted least-in-flight distribution
//! (`distribute`), calling the [`ProviderFactory`] once per chosen member.
//!
//! ADDITIVE by design (R6): a NEW entry point. It does not touch
//! [`LlmExecutor::execute`](crate::LlmExecutor) or the agent passive-walk — the
//! existing ordered agent path is unchanged; `distribute` is served ONLY here, by
//! `RouterPolicy`. Effort/behavior params ride in `turn.reasoning`; the
//! reliability crate stays domain-blind (it routes over opaque `PoolMember` ids).

use std::sync::Arc;

use execution_policy::core::DefaultCore;
use execution_policy::{ExecutionPolicyBuilder, Retry, RouterError, Served};
use futures::stream::BoxStream;
use praxec_core::error::ExecutorError;
use praxec_core::model_resolver::config::Strategy;
use praxec_core::pool_resolver::PoolMember;
use praxec_core::pool_router::build_router;

use crate::provider_factory::{ProviderFactory, TurnRequest};
use crate::stream_event::StreamEvent;

/// The provider event stream returned by [`ProviderFactory::stream`].
pub type TurnStream = BoxStream<'static, Result<StreamEvent, String>>;

/// Which `ExecutorError`s are *transient* (advance to the next pool member) vs
/// fail-fast. Throttle + timeout are transient (the throttle-failover case that
/// motivates distribution); `Auth`/`Permanent` fail fast — a retry re-uses the
/// same bad credential or hits the same author bug. Parity with the executor's
/// own `RateLimited`/`Timeout`-vs-`Auth`/`Permanent` classification (R7).
fn is_transient(e: &ExecutorError) -> bool {
    matches!(e, ExecutorError::RateLimited(_) | ExecutorError::Timeout(_))
}

/// Open a streaming turn over the resolved pool. Returns the served stream plus
/// provenance ([`Served::target`] = the `(provider, model, account)` member that
/// served, `attempts` = how many were tried), or a [`RouterError`] (all members
/// exhausted, a fail-fast error, or every member cooling behind a breaker).
pub async fn stream_over_pool(
    pool: &[PoolMember],
    strategy: Strategy,
    turn: TurnRequest,
    factory: Arc<dyn ProviderFactory>,
) -> Result<Served<PoolMember, TurnStream>, RouterError<PoolMember, ExecutorError>> {
    let router = build_router(
        pool,
        strategy,
        |_pm| {
            ExecutionPolicyBuilder::<TurnStream, ExecutorError>::new()
                .retry(Retry::exponential().max_attempts(1))
                .build()
        },
        is_transient,
        DefaultCore::new(),
    );
    router
        .run(async |m: &PoolMember| factory.stream(&m.model_string(), turn.clone()).await)
        .await
}

#[cfg(test)]
mod tests {
    use super::*;
    use async_trait::async_trait;
    use futures::stream;
    use rig::completion::Message;

    /// A fake factory: "fireworks" is throttled (429 → transient), everything
    /// else serves an empty stream.
    struct ThrottledFireworks;

    #[async_trait]
    impl ProviderFactory for ThrottledFireworks {
        async fn stream(
            &self,
            model_str: &str,
            _turn: TurnRequest,
        ) -> Result<TurnStream, ExecutorError> {
            if model_str.starts_with("fireworks:") {
                Err(ExecutorError::RateLimited("429".into()))
            } else {
                Ok(Box::pin(stream::empty()))
            }
        }
    }

    fn member(p: &str, m: &str) -> PoolMember {
        PoolMember {
            provider: p.into(),
            model: m.into(),
            account: None,
        }
    }

    fn turn() -> TurnRequest {
        TurnRequest {
            system: None,
            prompt: Message::user("hi"),
            tools: Vec::new(),
            history: Vec::new(),
            reasoning: None,
            tool_choice: None,
        }
    }

    #[tokio::test]
    async fn ordered_pool_fails_over_from_a_throttled_member() {
        let pool = vec![member("fireworks", "qwen"), member("openrouter", "qwen")];
        let served = stream_over_pool(
            &pool,
            Strategy::Ordered,
            turn(),
            Arc::new(ThrottledFireworks),
        )
        .await
        .expect("failover serves the second member");
        assert_eq!(served.target.provider, "openrouter");
        assert_eq!(served.attempts, 2);
    }

    #[tokio::test]
    async fn all_throttled_exhausts_loud() {
        let pool = vec![member("fireworks", "a"), member("fireworks", "b")];
        let out = stream_over_pool(
            &pool,
            Strategy::Ordered,
            turn(),
            Arc::new(ThrottledFireworks),
        )
        .await;
        assert!(
            matches!(
                out,
                Err(RouterError::Exhausted(ExecutorError::RateLimited(_)))
            ),
            "all members throttled → loud Exhausted, never a silent default"
        );
    }
}
