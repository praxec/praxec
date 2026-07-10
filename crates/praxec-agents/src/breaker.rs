//! Per-model cooldown circuit-breaker for the agent chain-walk (P12,
//! auto_drive hardening).
//!
//! The chain-walk in [`crate::executor`] is per-invocation: every agent call
//! re-resolves the model chain and walks it from the top. Without cross-call
//! memory, a persistently-down primary (e.g. a model that silently stalls
//! until the per-call `stall_timeout` fires) is re-probed — and re-times-out —
//! on EVERY agent call. This module is that memory: a passive per-model state
//! machine the walk consults *before* choosing a model, so a known-bad model
//! is skipped for a cooldown window and then re-probed once.
//!
//! States (classic breaker):
//! - **Closed** — normal; attempts allowed, consecutive failures counted.
//! - **Open** — after [`BREAKER_FAILURE_THRESHOLD`] consecutive failures;
//!   skipped until [`BREAKER_COOLDOWN`] elapses.
//! - **Half-open** — cooldown elapsed; ONE probe attempt is allowed. Probe
//!   success → Closed (count reset); probe failure → re-Open with a fresh
//!   cooldown.
//!
//! Design choices:
//! - **Local state machine, not `execution-policy`** — that crate is not a
//!   dependency of this workspace, and its breaker wraps async operations; our
//!   need is a passive per-key machine consulted synchronously while planning
//!   the walk. Classification stays on the existing
//!   [`praxec_core::model_resolver::FailureClass`] (the walk already uses it),
//!   so "what counts as a model-health failure" has exactly one definition.
//! - **Pure decision logic** — [`breaker_decision`], [`record_failure`],
//!   [`record_success`], and [`plan_chain`] take `Instant`/`Duration`
//!   arguments instead of reading a clock, so tests inject time. `Instant` is
//!   monotonic (never `SystemTime` — wall-clock jumps must not reopen or
//!   half-open a breaker).
//! - **Degrade, never zero out** — if every model in the chain is open,
//!   [`plan_chain`] still yields one attempt (the least-recently-failed
//!   model). A drive must never be left with nothing to try.

use std::collections::HashMap;
use std::sync::Mutex;
use std::time::{Duration, Instant};

/// Consecutive escalatable failures before a model's breaker opens.
// TODO: surface in models.yaml (per-model/per-provider override).
pub const BREAKER_FAILURE_THRESHOLD: u32 = 2;

/// How long an open breaker skips its model before allowing a half-open probe.
// TODO: surface in models.yaml (per-model/per-provider override).
pub const BREAKER_COOLDOWN: Duration = Duration::from_secs(30 * 60);

/// What the walk may do with a model right now.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BreakerAvailability {
    /// Closed — attempt normally.
    Available,
    /// Open and inside the cooldown — skip; walk to the next model.
    Skip,
    /// Open but the cooldown has elapsed (half-open) — one probe attempt.
    Probe,
}

/// Per-model breaker state: consecutive-failure count plus the instant the
/// breaker (last) opened. `opened_at: Some(_)` IS the open/half-open signal;
/// `None` is closed.
#[derive(Debug, Clone, Copy, Default)]
pub struct BreakerEntry {
    consecutive_failures: u32,
    opened_at: Option<Instant>,
}

/// Pure decision: is this model available, skipped, or up for a probe at
/// `now`? Takes time as data (no clock reads) so tests inject it.
pub fn breaker_decision(
    entry: &BreakerEntry,
    now: Instant,
    cooldown: Duration,
) -> BreakerAvailability {
    match entry.opened_at {
        None => BreakerAvailability::Available,
        Some(opened) if now.duration_since(opened) >= cooldown => BreakerAvailability::Probe,
        Some(_) => BreakerAvailability::Skip,
    }
}

/// Record an escalatable failure at `now`. Reaching `threshold` consecutive
/// failures opens the breaker; a failure while already open (the half-open
/// probe failing) re-opens it with a fresh cooldown anchored at `now`.
pub fn record_failure(entry: &mut BreakerEntry, now: Instant, threshold: u32) {
    entry.consecutive_failures = entry.consecutive_failures.saturating_add(1);
    if entry.consecutive_failures >= threshold {
        entry.opened_at = Some(now);
    }
}

/// Record a success: the breaker closes and the consecutive-failure count
/// resets (a success before the threshold also wipes the count).
pub fn record_success(entry: &mut BreakerEntry) {
    *entry = BreakerEntry::default();
}

/// Pure chain planning: keep the resolved chain's order but drop models whose
/// breaker says [`BreakerAvailability::Skip`]. Models with no entry are
/// closed. If EVERYTHING is skipped, degrade to a single attempt on the
/// least-recently-failed model (earliest `opened_at` — the one closest to its
/// cooldown expiring) rather than returning an empty plan.
pub fn plan_chain(
    chain: &[String],
    entries: &HashMap<String, BreakerEntry>,
    now: Instant,
    cooldown: Duration,
) -> Vec<String> {
    let planned: Vec<String> = chain
        .iter()
        .filter(|model| {
            let available = entries
                .get(model.as_str())
                .is_none_or(|e| breaker_decision(e, now, cooldown) != BreakerAvailability::Skip);
            if !available {
                tracing::warn!(
                    model = %model,
                    "model breaker is open; skipping in the agent model chain"
                );
            }
            available
        })
        .cloned()
        .collect();
    if !planned.is_empty() {
        return planned;
    }
    // Degrade path: never leave the drive with zero models to try. All chain
    // models are open here, so each has an `opened_at`.
    let least_recently_failed = chain
        .iter()
        .min_by_key(|model| entries.get(model.as_str()).and_then(|e| e.opened_at))
        .cloned();
    match least_recently_failed {
        Some(model) => {
            tracing::warn!(
                model = %model,
                "all models in the chain have open breakers; degrading to the \
                 least-recently-failed model rather than failing with nothing to try"
            );
            vec![model]
        }
        // Empty input chain — nothing to degrade to; the executor's existing
        // empty-chain fail-fast handles it.
        None => Vec::new(),
    }
}

/// Cross-call breaker registry, keyed by resolved model id. One per
/// [`crate::executor::AgentExecutor`]; lives as long as the process, so the
/// per-invocation chain-walk inherits memory of prior calls' failures.
#[derive(Default)]
pub struct BreakerRegistry {
    entries: Mutex<HashMap<String, BreakerEntry>>,
}

impl BreakerRegistry {
    /// Plan the walk order for `chain` at `now` (see [`plan_chain`]).
    pub fn plan(&self, chain: &[String], now: Instant) -> Vec<String> {
        let entries = self.entries.lock().expect("breaker registry poisoned");
        plan_chain(chain, &entries, now, BREAKER_COOLDOWN)
    }

    /// Record a successful attempt: the model's breaker closes.
    pub fn on_success(&self, model: &str) {
        let mut entries = self.entries.lock().expect("breaker registry poisoned");
        if let Some(entry) = entries.get_mut(model) {
            record_success(entry);
        }
    }

    /// Record an escalatable failure at `now` (may open the breaker).
    pub fn on_failure(&self, model: &str, now: Instant) {
        let mut entries = self.entries.lock().expect("breaker registry poisoned");
        record_failure(
            entries.entry(model.to_string()).or_default(),
            now,
            BREAKER_FAILURE_THRESHOLD,
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const COOLDOWN: Duration = Duration::from_secs(30 * 60);
    const THRESHOLD: u32 = 2;

    fn fail_n(entry: &mut BreakerEntry, n: u32, at: Instant) {
        for _ in 0..n {
            record_failure(entry, at, THRESHOLD);
        }
    }

    /// (a) The breaker opens after THRESHOLD consecutive failures — and not
    /// one failure earlier.
    #[test]
    fn opens_after_threshold_consecutive_failures() {
        let t0 = Instant::now();
        let mut entry = BreakerEntry::default();

        record_failure(&mut entry, t0, THRESHOLD);
        assert_eq!(
            breaker_decision(&entry, t0, COOLDOWN),
            BreakerAvailability::Available,
            "one failure below the threshold must not open the breaker"
        );

        record_failure(&mut entry, t0, THRESHOLD);
        assert_eq!(
            breaker_decision(&entry, t0, COOLDOWN),
            BreakerAvailability::Skip,
            "reaching the threshold must open the breaker"
        );
    }

    /// (b) An open breaker within its cooldown → Skip.
    #[test]
    fn open_within_cooldown_skips() {
        let t0 = Instant::now();
        let mut entry = BreakerEntry::default();
        fail_n(&mut entry, THRESHOLD, t0);

        let just_before = t0 + COOLDOWN - Duration::from_secs(1);
        assert_eq!(
            breaker_decision(&entry, just_before, COOLDOWN),
            BreakerAvailability::Skip
        );
    }

    /// (c) After the cooldown elapses → Probe (half-open).
    #[test]
    fn open_after_cooldown_probes() {
        let t0 = Instant::now();
        let mut entry = BreakerEntry::default();
        fail_n(&mut entry, THRESHOLD, t0);

        assert_eq!(
            breaker_decision(&entry, t0 + COOLDOWN, COOLDOWN),
            BreakerAvailability::Probe
        );
    }

    /// (d) Probe success → Closed, failure count reset (a single follow-up
    /// failure does not re-open).
    #[test]
    fn probe_success_closes_and_resets_count() {
        let t0 = Instant::now();
        let mut entry = BreakerEntry::default();
        fail_n(&mut entry, THRESHOLD, t0);

        // Half-open probe succeeds.
        record_success(&mut entry);
        let after = t0 + COOLDOWN + Duration::from_secs(1);
        assert_eq!(
            breaker_decision(&entry, after, COOLDOWN),
            BreakerAvailability::Available
        );

        // The count was reset: one new failure stays below the threshold.
        record_failure(&mut entry, after, THRESHOLD);
        assert_eq!(
            breaker_decision(&entry, after, COOLDOWN),
            BreakerAvailability::Available,
            "post-success count must restart from zero"
        );
    }

    /// (e) Probe failure → re-Open with a FRESH cooldown anchored at the
    /// probe's failure time.
    #[test]
    fn probe_failure_reopens_with_fresh_cooldown() {
        let t0 = Instant::now();
        let mut entry = BreakerEntry::default();
        fail_n(&mut entry, THRESHOLD, t0);

        let probe_at = t0 + COOLDOWN;
        assert_eq!(
            breaker_decision(&entry, probe_at, COOLDOWN),
            BreakerAvailability::Probe
        );
        record_failure(&mut entry, probe_at, THRESHOLD);

        // Inside the NEW window (would have been past the old one) → Skip.
        assert_eq!(
            breaker_decision(&entry, probe_at + Duration::from_secs(1), COOLDOWN),
            BreakerAvailability::Skip,
            "a failed probe must restart the cooldown, not reuse the old anchor"
        );
        // And the fresh window eventually yields another probe.
        assert_eq!(
            breaker_decision(&entry, probe_at + COOLDOWN, COOLDOWN),
            BreakerAvailability::Probe
        );
    }

    /// (f) A success before the threshold resets the consecutive count.
    #[test]
    fn success_before_threshold_resets_count() {
        let t0 = Instant::now();
        let mut entry = BreakerEntry::default();

        record_failure(&mut entry, t0, THRESHOLD);
        record_success(&mut entry);
        record_failure(&mut entry, t0, THRESHOLD);
        assert_eq!(
            breaker_decision(&entry, t0, COOLDOWN),
            BreakerAvailability::Available,
            "fail/success/fail must not open a threshold-2 breaker"
        );
    }

    /// (g) All models open → the plan still yields one attempt (the
    /// least-recently-failed model), never an empty walk.
    #[test]
    fn all_open_degrades_to_least_recently_failed() {
        let t0 = Instant::now();
        let chain = vec!["m:a".to_string(), "m:b".to_string()];
        let mut entries = HashMap::new();

        // m:a failed longer ago (t0); m:b failed more recently (t0 + 60s).
        let mut a = BreakerEntry::default();
        fail_n(&mut a, THRESHOLD, t0);
        let mut b = BreakerEntry::default();
        fail_n(&mut b, THRESHOLD, t0 + Duration::from_secs(60));
        entries.insert("m:a".to_string(), a);
        entries.insert("m:b".to_string(), b);

        let now = t0 + Duration::from_secs(120); // both still inside cooldown
        let planned = plan_chain(&chain, &entries, now, COOLDOWN);
        assert_eq!(
            planned,
            vec!["m:a".to_string()],
            "degrade path must pick the least-recently-failed model"
        );
    }

    /// plan_chain keeps chain order and drops only Skip models; a half-open
    /// (Probe) model stays in the plan.
    #[test]
    fn plan_drops_skips_keeps_probes_and_order() {
        let t0 = Instant::now();
        let chain = vec!["m:a".to_string(), "m:b".to_string(), "m:c".to_string()];
        let mut entries = HashMap::new();

        // m:a open inside cooldown → Skip; m:b open past cooldown → Probe;
        // m:c untracked → Available.
        let mut a = BreakerEntry::default();
        fail_n(&mut a, THRESHOLD, t0 + COOLDOWN); // recent → still cooling at `now`
        let mut b = BreakerEntry::default();
        fail_n(&mut b, THRESHOLD, t0); // old → cooldown elapsed at `now`
        entries.insert("m:a".to_string(), a);
        entries.insert("m:b".to_string(), b);

        let now = t0 + COOLDOWN + Duration::from_secs(1);
        let planned = plan_chain(&chain, &entries, now, COOLDOWN);
        assert_eq!(planned, vec!["m:b".to_string(), "m:c".to_string()]);
    }

    /// The registry wires the pure pieces together across calls: failures
    /// accumulate per model, successes reset, planning consults the state.
    #[test]
    fn registry_accumulates_across_calls() {
        let reg = BreakerRegistry::default();
        let chain = vec!["m:weak".to_string(), "m:strong".to_string()];
        let t0 = Instant::now();

        reg.on_failure("m:weak", t0);
        assert_eq!(reg.plan(&chain, t0), chain, "below threshold: full chain");

        reg.on_failure("m:weak", t0);
        assert_eq!(
            reg.plan(&chain, t0),
            vec!["m:strong".to_string()],
            "at threshold: the weak model is skipped"
        );

        reg.on_success("m:weak");
        assert_eq!(reg.plan(&chain, t0), chain, "success closes the breaker");
    }
}
