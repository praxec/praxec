//! Wire a resolved pool (`Vec<PoolMember>` + [`Strategy`]) onto the
//! `execution-policy` [`RouterPolicy`] (spec #2 §6).
//!
//! Each [`PoolMember`] becomes an `execution_policy::Member` carrying a
//! per-member [`ExecutionPolicy`] (retry + breaker) the caller supplies; the pool
//! is served by ONE `RouterPolicy`, no per-algorithm branch. The distribution
//! strategy maps to a [`Pick`]:
//! - `ordered` → [`Pick::first_healthy`] (health-aware failover, unchanged from
//!   today's cost-ascending chain behavior),
//! - `distribute` → [`Pick::weighted_least_in_flight`] (even spread, focus-in on
//!   the healthy survivors when one is throttled — over the value band the
//!   resolver already narrowed to, R2).
//!
//! Effort/behavior params are applied by the caller **inside the run-closure**
//! (the crate stays domain-blind); `advance_when` is praxec's
//! `FailureClass::is_infrastructure` classification, passed in — never
//! reimplemented (R7).

use execution_policy::{Core, ExecutionPolicy, Member, Pick, RouterPolicy};

use crate::model_resolver::config::Strategy;
use crate::pool_resolver::PoolMember;

/// The `Pick` for a pool's distribution [`Strategy`].
pub fn strategy_pick(strategy: Strategy) -> Pick<PoolMember> {
    match strategy {
        Strategy::Ordered => Pick::first_healthy(),
        Strategy::Distribute => Pick::weighted_least_in_flight(),
    }
}

/// Build a `RouterPolicy` over a resolved pool. `make_policy` mints the
/// per-member `ExecutionPolicy` (retry/breaker), `advance_when` classifies
/// transient errors, and `core` is the shared clock/RNG (`DefaultCore` in
/// production, `TestCore` in tests).
///
/// The router's target id is the typed [`PoolMember`] triad, so `Served` /
/// per-member breaker keying are `(provider, model, account)` by construction.
pub fn build_router<C, T, E>(
    pool: &[PoolMember],
    strategy: Strategy,
    mut make_policy: impl FnMut(&PoolMember) -> ExecutionPolicy<C, T, E>,
    advance_when: impl Fn(&E) -> bool + Send + Sync + 'static,
    core: C,
) -> RouterPolicy<PoolMember, C, T, E>
where
    C: Core,
{
    let mut builder = RouterPolicy::builder()
        .select(strategy_pick(strategy))
        .advance_when(advance_when);
    for pm in pool {
        let policy = make_policy(pm);
        builder = builder.target(Member::new(pm.clone(), policy));
    }
    builder.build_with(core)
}

#[cfg(test)]
mod tests {
    use super::*;
    use execution_policy::core::{ManualClock, TestCore};
    use execution_policy::{ExecutionPolicyBuilder, Retry};

    fn member(provider: &str, model: &str) -> PoolMember {
        PoolMember {
            provider: provider.into(),
            model: model.into(),
            account: None,
        }
    }

    fn policy(clock: &ManualClock) -> ExecutionPolicy<TestCore, u32, u16> {
        ExecutionPolicyBuilder::<u32, u16>::new()
            .retry(Retry::exponential().max_attempts(1))
            .build_with(TestCore::new(clock.clone()))
    }

    #[tokio::test]
    async fn ordered_pool_fails_over_to_the_next_member() {
        let clock = ManualClock::new();
        let pool = vec![member("fireworks", "qwen"), member("openrouter", "qwen")];
        let router = build_router(
            &pool,
            Strategy::Ordered,
            |_pm| policy(&clock),
            |e: &u16| *e == 429, // throttle is transient
            TestCore::new(clock.clone()),
        );
        // First member 429s (throttled) → router advances to the second.
        let served = router
            .run(async |m: &PoolMember| {
                if m.provider == "fireworks" {
                    Err::<u32, u16>(429)
                } else {
                    Ok(7)
                }
            })
            .await
            .expect("second member serves");
        assert_eq!(served.value, 7);
        assert_eq!(served.target.provider, "openrouter");
        assert_eq!(served.attempts, 2);
    }

    #[tokio::test]
    async fn distribute_pool_serves_from_a_healthy_member() {
        let clock = ManualClock::new();
        let pool = vec![member("a", "m"), member("b", "m")];
        let router = build_router(
            &pool,
            Strategy::Distribute,
            |_pm| policy(&clock),
            |_e: &u16| true,
            TestCore::new(clock.clone()),
        );
        let served = router
            .run(|_m: &PoolMember| async { Ok::<u32, u16>(1) })
            .await
            .expect("served");
        assert_eq!(served.value, 1);
    }
}
