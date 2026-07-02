//! SPEC §33 D6 — cumulative-cap enforcement + synthetic `_llm.*` slot
//! bookkeeping for the in-runtime LLM executor.
//!
//! The executor tracks per-workflow cumulative state via reserved
//! blackboard keys, mirroring the `_fire_count.*` pattern from
//! `runtime_submit.rs`:
//!
//! - `_llm.cumulative_tokens` — `u64` sum of (input + output) tokens
//!   across all turns in this workflow.
//! - `_llm.cumulative_cost_usd` — `f64` sum of cost across all turns
//!   (cost catalogue lands in D8; for D6 this stays at 0.0).
//! - `_llm.cumulative_iterations` — `u64` count of LLM turns in this
//!   workflow.
//! - `_llm.consecutive_no_tool_call` — `u64` count of consecutive
//!   `LLM_NO_TOOL_CALL` failures. Resets on any successful turn. Per
//!   FMECA F1: after `max_iterations` consecutive failures, surface
//!   `LLM_EXECUTION_EXHAUSTED`.
//! - `_llm.session.<state>.started_at` — `RFC3339` string written on
//!   first turn at a state. Used by the `max_seconds` per-session
//!   timer. The existing `clear_state_local_slots_on_exit` mechanism
//!   does NOT scrub this prefix (it only scrubs `_fire_count.<state>.`
//!   and declared state-scope slots), so the slot can persist across
//!   states — that's acceptable for v0.6 and explicitly documented.
//!
//! The mutation pathway is **explicit output mapping**: the executor
//! returns the proposed slot values at the top level of
//! `ExecuteResult.output` and the workflow author opts each one into
//! the persisted context via the transition's `output:` block. This
//! keeps the executor stateless (no direct mutation of `next.context`)
//! and consistent with how every other executor surfaces its data.
//! The reserved-prefix check still rejects user-declared blackboard
//! slots whose names begin with `_llm.` so the synthetic namespace is
//! never shadowed by authored slots.

use chrono::{DateTime, Utc};
use praxec_core::error::{ExecutorError, LlmErrorCode};
use praxec_core::model::WorkflowInstance;
use serde_json::{json, Value};

use crate::config::LlmExecutorConfig;
use crate::response::DrainedResponse;

/// Reserved-prefix marker for the synthetic `_llm.*` slot namespace.
/// Surface for the workflow loader to consume.
pub const RESERVED_LLM_PREFIX: &str = "_llm.";

/// Snapshot of the four cumulative counters and the per-state session
/// start, read from a `WorkflowInstance.context` map. Missing keys
/// resolve to 0 / 0.0 / `None` — matches the `_fire_count.*` pattern.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct SlotSnapshot {
    pub cumulative_tokens: u64,
    pub cumulative_cost_usd: f64,
    pub cumulative_iterations: u64,
    pub consecutive_no_tool_call: u64,
    /// Parsed `_llm.session.<state>.started_at` for the requested
    /// state. `None` if no prior turn at this state wrote one or if
    /// the persisted value did not parse as RFC3339.
    pub session_started_at: Option<DateTime<Utc>>,
}

/// Read the synthetic-slot snapshot for `state` out of a
/// `WorkflowInstance`'s `context` object. Missing keys are treated as
/// zero (mirrors `_fire_count.*` convention). A persisted
/// `_llm.session.<state>.started_at` that fails RFC3339 parsing is
/// treated as `None` rather than crashing the pre-turn check — D6
/// favours forward progress over hard-failing on corrupted synthetic
/// state.
/// SPEC §33 audit fixup (F6 STUB-008) — read a typed counter slot,
/// distinguishing "absent" (returns `None` quietly) from
/// "present-but-wrong-type" (returns `None` and logs `tracing::warn`).
/// Operators can grep audit-host logs for this warning to spot
/// blackboard corruption that pre-fix silently downgraded to zero.
fn slot_or_warn_typed<T, F>(
    ctx: &serde_json::Map<String, Value>,
    key: &'static str,
    coerce: F,
) -> Option<T>
where
    F: Fn(&Value) -> Option<T>,
{
    match ctx.get(key) {
        None => None,
        Some(value) => match coerce(value) {
            Some(t) => Some(t),
            None => {
                tracing::warn!(
                    target: "praxec_llm_executor::caps",
                    key = key,
                    value = %value,
                    "synthetic _llm.* slot is present but the wrong type; treating as zero. \
                     A corrupted blackboard could otherwise bypass budget caps — \
                     investigate the workflow's `output:` mapping that wrote this slot."
                );
                None
            }
        },
    }
}

pub fn read_snapshot(instance: &WorkflowInstance, state: &str) -> SlotSnapshot {
    let ctx = match instance.context.as_object() {
        Some(obj) => obj,
        None => return SlotSnapshot::default(),
    };

    // SPEC §33 audit fixup (F6 STUB-008): distinguish "key absent" (a
    // legitimate zero, mirrors the `_fire_count.*` convention) from
    // "key present but wrong type" (a corruption sign that previously
    // silently downgraded to zero — a corrupted blackboard could have
    // bypassed budget caps). Present-but-wrong-type now emits a loud
    // `tracing::warn` so operators can spot the corruption in logs.
    let cumulative_tokens =
        slot_or_warn_typed(ctx, "_llm.cumulative_tokens", |v: &Value| v.as_u64()).unwrap_or(0);
    let cumulative_cost_usd =
        slot_or_warn_typed(ctx, "_llm.cumulative_cost_usd", |v: &Value| v.as_f64()).unwrap_or(0.0);
    let cumulative_iterations =
        slot_or_warn_typed(ctx, "_llm.cumulative_iterations", |v: &Value| v.as_u64()).unwrap_or(0);
    let consecutive_no_tool_call =
        slot_or_warn_typed(ctx, "_llm.consecutive_no_tool_call", |v: &Value| v.as_u64())
            .unwrap_or(0);

    let session_key = session_started_at_key(state);
    let session_started_at = ctx
        .get(&session_key)
        .and_then(Value::as_str)
        .and_then(|s| DateTime::parse_from_rfc3339(s).ok())
        .map(|dt| dt.with_timezone(&Utc));

    SlotSnapshot {
        cumulative_tokens,
        cumulative_cost_usd,
        cumulative_iterations,
        consecutive_no_tool_call,
        session_started_at,
    }
}

/// Apply the pre-turn cumulative-cap checks defined in SPEC §33 D6.
///
/// Fail-fast order, each branch producing a typed `LlmErrorCode`:
///
/// 1. `consecutive_no_tool_call >= max_iterations` →
///    `LlmErrorCode::ExecutionExhausted` (FMECA F1).
/// 2. `max_tokens.is_some() && cumulative_tokens >= max_tokens` →
///    `LlmErrorCode::BudgetExceeded`.
/// 3. `max_cost_usd.is_some() && cumulative_cost_usd >= max_cost_usd`
///    → `LlmErrorCode::BudgetExceeded`.
/// 4. `max_seconds.is_some()` AND a recorded session start that has
///    elapsed beyond the cap → `LlmErrorCode::ExecutionExhausted`
///    (FMECA F1's session timeout).
///
/// `now` is injected so unit tests can drive a deterministic clock.
pub fn apply_caps(
    snapshot: &SlotSnapshot,
    config: &LlmExecutorConfig,
    now: DateTime<Utc>,
) -> Result<(), ExecutorError> {
    // 1. Consecutive-failure cap (FMECA F1).
    let max_iter = u64::from(config.max_iterations);
    if snapshot.consecutive_no_tool_call >= max_iter {
        return Err(ExecutorError::Llm(
            LlmErrorCode::ExecutionExhausted,
            format!(
                "LLM executor: consecutive LLM_NO_TOOL_CALL failures \
                 ({}) reached max_iterations ({}); refusing further \
                 provider calls (SPEC §33 FMECA F1)",
                snapshot.consecutive_no_tool_call, max_iter
            ),
        ));
    }

    // 2. Cumulative-token cap.
    if let Some(cap) = config.max_tokens {
        if snapshot.cumulative_tokens >= cap {
            return Err(ExecutorError::Llm(
                LlmErrorCode::BudgetExceeded,
                format!(
                    "LLM executor: cumulative token usage ({}) reached \
                     max_tokens budget ({}); refusing further provider \
                     calls",
                    snapshot.cumulative_tokens, cap
                ),
            ));
        }
    }

    // 3. Cumulative-cost cap.
    if let Some(cap) = config.max_cost_usd {
        if snapshot.cumulative_cost_usd >= cap {
            return Err(ExecutorError::Llm(
                LlmErrorCode::BudgetExceeded,
                format!(
                    "LLM executor: cumulative cost (USD {:.6}) reached \
                     max_cost_usd budget (USD {:.6}); refusing further \
                     provider calls",
                    snapshot.cumulative_cost_usd, cap
                ),
            ));
        }
    }

    // 4. Per-state session-wallclock cap. Only fires when a previous
    // turn at the same state recorded a `started_at` (the snapshot
    // resolves the per-state key in `read_snapshot`); first-turn-ever
    // is allowed through. SPEC §33 FMECA F1's session-timeout branch.
    if let Some(max_seconds) = config.max_seconds {
        if let Some(started_at) = snapshot.session_started_at {
            let elapsed = now.signed_duration_since(started_at);
            let elapsed_secs = elapsed.num_seconds();
            if elapsed_secs >= 0 && (elapsed_secs as u64) >= max_seconds {
                return Err(ExecutorError::Llm(
                    LlmErrorCode::ExecutionExhausted,
                    format!(
                        "LLM executor: LLM session has been open for {elapsed_secs}s, \
                         exceeding max_seconds ({max_seconds}) (SPEC §33 FMECA F1 \
                         session timeout)"
                    ),
                ));
            }
        }
    }

    Ok(())
}

/// Compute the post-turn slot updates to be emitted via
/// `ExecuteResult.output`. Returns a JSON object whose keys are the
/// reserved `_llm.*` slot names and whose values are the new (already
/// incremented / reset) counter values.
///
/// The workflow author's transition `output:` block opts each key into
/// `next.context`, e.g.:
///
/// ```yaml
/// output:
///   "_llm.cumulative_tokens": "$.output[\"_llm.cumulative_tokens\"]"
///   "_llm.cumulative_iterations": "$.output[\"_llm.cumulative_iterations\"]"
///   "_llm.consecutive_no_tool_call": "$.output[\"_llm.consecutive_no_tool_call\"]"
///   "_llm.session.<state>.started_at": "$.output[\"_llm.session.<state>.started_at\"]"
/// ```
///
/// Does NOT mutate the instance — pure function over the inputs.
///
/// `no_tool_call_this_turn` is the executor's classification of the
/// drained response: `true` iff validation surfaced
/// `LlmErrorCode::NoToolCall`. On `true` the consecutive-failure
/// counter increments; on `false` (any other outcome, including
/// successful tool calls AND non-F1 failures) it resets to 0.
///
/// `cost_usd` is plumbed for D8; D6 callers pass `None` so
/// `_llm.cumulative_cost_usd` stays at the pre-turn value.
///
/// CMP-008: `cap_active` tells the function whether a token/cost cap is
/// configured for this transition (`max_tokens.is_some() ||
/// max_cost_usd.is_some()`). When a cap IS active but the drained
/// response carries NO `usage` event, silently folding `0` tokens into
/// the cumulative counter would undercount real spend and let the cap
/// be bypassed. The success path validates usage upstream, but the F1
/// no-tool-call FAILURE path reaches here without that guarantee — so
/// under an active cap with absent usage we refuse to merge a
/// zero-token update and surface a typed `UsageMissing` error instead.
/// When no cap is active, missing usage is fine (cost tracking is
/// opt-in) and folds in as `0` exactly as before.
pub fn build_post_turn_slot_updates(
    pre_turn: &SlotSnapshot,
    drained: &DrainedResponse,
    cost_usd: Option<f64>,
    no_tool_call_this_turn: bool,
    cap_active: bool,
    state: &str,
    timestamp: DateTime<Utc>,
) -> Result<Value, ExecutorError> {
    let turn_tokens = match drained
        .usage
        .as_ref()
        .map(|u| u.input_tokens + u.output_tokens)
    {
        Some(t) => t,
        None if cap_active => {
            // CMP-008: a token/cost cap is active but the provider
            // returned no usage. Merging 0 here would silently drop the
            // turn's spend and let the cumulative cap be bypassed.
            return Err(ExecutorError::Llm(
                LlmErrorCode::UsageMissing,
                format!(
                    "LLM executor: post-turn slot update at state '{state}' has no \
                     `Usage` event while a token/cost cap is active; refusing to \
                     fold a zero-token update into the cumulative counter (would \
                     undercount the cap)"
                ),
            ));
        }
        None => 0,
    };

    let new_tokens = pre_turn.cumulative_tokens.saturating_add(turn_tokens);
    let new_iters = pre_turn.cumulative_iterations.saturating_add(1);
    let new_cost = pre_turn.cumulative_cost_usd + cost_usd.unwrap_or(0.0);
    let new_consecutive = if no_tool_call_this_turn {
        pre_turn.consecutive_no_tool_call.saturating_add(1)
    } else {
        0
    };

    // Reuse the existing session_started_at if a prior turn at this
    // state already wrote one; otherwise record `timestamp` as the
    // session origin.
    let session_started_at = pre_turn
        .session_started_at
        .unwrap_or(timestamp)
        .to_rfc3339();

    // Output is emitted in NESTED form so workflow authors can address
    // each slot via the standard `$.output._llm.<key>` path syntax
    // (`read_in_scopes` treats dots as separators and has no bracket-
    // quoted-key escape). Context-side reads in [`read_snapshot`]
    // continue to use FLAT keys (`_llm.cumulative_tokens` etc.), so the
    // workflow's `output:` mapping bridges nested-output → flat-context:
    //
    // ```yaml
    // output:
    //   "_llm.cumulative_tokens":       "$.output._llm.cumulative_tokens"
    //   "_llm.cumulative_cost_usd":     "$.output._llm.cumulative_cost_usd"
    //   "_llm.cumulative_iterations":   "$.output._llm.cumulative_iterations"
    //   "_llm.consecutive_no_tool_call":"$.output._llm.consecutive_no_tool_call"
    //   "_llm.session.<state>.started_at": "$.output._llm.session.<state>.started_at"
    // ```
    //
    // The session-started-at key uses the `_llm.session.<state>.started_at`
    // flat form on the context side; the nested-output form for the
    // session key still uses the per-state path so workflows with
    // multiple LLM states can address each independently.
    let mut session_obj = serde_json::Map::new();
    let mut state_obj = serde_json::Map::new();
    state_obj.insert("started_at".into(), Value::String(session_started_at));
    session_obj.insert(state.to_string(), Value::Object(state_obj));

    let mut llm_obj = serde_json::Map::new();
    llm_obj.insert("cumulative_tokens".into(), json!(new_tokens));
    llm_obj.insert("cumulative_cost_usd".into(), json!(new_cost));
    llm_obj.insert("cumulative_iterations".into(), json!(new_iters));
    llm_obj.insert("consecutive_no_tool_call".into(), json!(new_consecutive));
    llm_obj.insert("session".into(), Value::Object(session_obj));

    Ok(json!({ "_llm": llm_obj }))
}

/// Build the synthetic slot key for the per-state session origin.
/// Exposed so callers (and tests) can read/write the same key without
/// duplicating the format string.
pub fn session_started_at_key(state: &str) -> String {
    format!("_llm.session.{state}.started_at")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::stream_event::TokenUsage;
    use serde_json::json;

    fn cfg() -> LlmExecutorConfig {
        LlmExecutorConfig {
            model: Some("openai:gpt-5".into()),
            affinity: None,
            needs: vec![],
            prompt_template: "x".into(),
            max_iterations: 3,
            max_seconds: None,
            max_tokens: None,
            max_cost_usd: None,
            reasoning_effort: None,
            capture_reasoning: true,
        }
    }

    #[test]
    fn snapshot_treats_missing_keys_as_zero() {
        let instance = make_instance(json!({}));
        let snap = read_snapshot(&instance, "thinking");
        assert_eq!(snap.cumulative_tokens, 0);
        assert_eq!(snap.cumulative_cost_usd, 0.0);
        assert_eq!(snap.cumulative_iterations, 0);
        assert_eq!(snap.consecutive_no_tool_call, 0);
        assert!(snap.session_started_at.is_none());
    }

    #[test]
    fn snapshot_reads_populated_counters() {
        let instance = make_instance(json!({
            "_llm.cumulative_tokens": 5000,
            "_llm.cumulative_cost_usd": 0.42,
            "_llm.cumulative_iterations": 2,
            "_llm.consecutive_no_tool_call": 1,
        }));
        let snap = read_snapshot(&instance, "thinking");
        assert_eq!(snap.cumulative_tokens, 5000);
        assert_eq!(snap.cumulative_cost_usd, 0.42);
        assert_eq!(snap.cumulative_iterations, 2);
        assert_eq!(snap.consecutive_no_tool_call, 1);
    }

    fn make_instance(context: Value) -> WorkflowInstance {
        WorkflowInstance {
            id: "wf_caps".into(),
            definition_id: "demo".into(),
            definition_version: "1.0.0".into(),
            definition: json!({"initialState": "thinking", "states": {}}),
            state: "thinking".into(),
            version: 0,
            input: json!({}),
            context,
            started_at: Utc::now(),
            trace_id: None,
            run_id: None,
            cancelled_at: None,
            cancelled_reason: None,
            depth: 0,
            parent: None,
        }
    }

    #[test]
    fn build_post_turn_slot_updates_resets_consecutive_on_success() {
        let pre = SlotSnapshot {
            consecutive_no_tool_call: 2,
            cumulative_iterations: 4,
            ..Default::default()
        };
        let drained = DrainedResponse {
            usage: Some(TokenUsage {
                input_tokens: 50,
                output_tokens: 25,
                reasoning_tokens: None,
            }),
            ..Default::default()
        };
        let now = Utc::now();
        let out = build_post_turn_slot_updates(&pre, &drained, None, false, false, "thinking", now)
            .expect("no cap active");
        // Nested form: `$.output._llm.<key>` per the F3 fixup so the
        // workflow's output mapping can address each slot.
        assert_eq!(out["_llm"]["consecutive_no_tool_call"], json!(0));
        assert_eq!(out["_llm"]["cumulative_iterations"], json!(5));
        assert_eq!(out["_llm"]["cumulative_tokens"], json!(75));
    }

    #[test]
    fn build_post_turn_slot_updates_increments_consecutive_on_failure() {
        let pre = SlotSnapshot {
            consecutive_no_tool_call: 1,
            ..Default::default()
        };
        let drained = DrainedResponse::default();
        let out =
            build_post_turn_slot_updates(&pre, &drained, None, true, false, "thinking", Utc::now())
                .expect("no cap active so missing usage folds to 0");
        assert_eq!(out["_llm"]["consecutive_no_tool_call"], json!(2));
    }

    /// CMP-008: the no-tool-call FAILURE path can reach
    /// `build_post_turn_slot_updates` with a drained response that has
    /// NO usage event (validate rejects NoToolCall before its usage
    /// check). Folding 0 tokens under an active cap would undercount
    /// real spend, so with `cap_active = true` and absent usage the
    /// helper must surface a typed `UsageMissing` error rather than
    /// silently merge a zero-token update.
    #[test]
    fn build_post_turn_slot_updates_errors_on_missing_usage_under_active_cap() {
        let pre = SlotSnapshot {
            consecutive_no_tool_call: 1,
            ..Default::default()
        };
        let drained = DrainedResponse::default(); // no usage event
        let err = build_post_turn_slot_updates(
            &pre,
            &drained,
            None,
            true, // no_tool_call_this_turn
            true, // cap_active
            "thinking",
            Utc::now(),
        )
        .expect_err("missing usage under an active cap must fail loudly");
        match err {
            ExecutorError::Llm(LlmErrorCode::UsageMissing, msg) => {
                assert!(msg.contains("thinking"), "expected state in msg: {msg}");
            }
            other => panic!("expected UsageMissing, got {other:?}"),
        }
    }

    /// CMP-008 negative: missing usage with NO cap active is still fine
    /// (cost tracking is opt-in) — folds in as 0 exactly as before.
    #[test]
    fn build_post_turn_slot_updates_missing_usage_no_cap_folds_zero() {
        let pre = SlotSnapshot {
            cumulative_tokens: 100,
            ..Default::default()
        };
        let drained = DrainedResponse::default(); // no usage event
        let out = build_post_turn_slot_updates(
            &pre,
            &drained,
            None,
            false,
            false, // cap_active = false
            "thinking",
            Utc::now(),
        )
        .expect("no cap active so missing usage is acceptable");
        assert_eq!(out["_llm"]["cumulative_tokens"], json!(100));
    }

    #[test]
    fn build_post_turn_slot_updates_preserves_existing_session_start() {
        let earlier = Utc::now() - chrono::Duration::seconds(10);
        let pre = SlotSnapshot {
            session_started_at: Some(earlier),
            ..Default::default()
        };
        let drained = DrainedResponse::default();
        let now = Utc::now();
        let out = build_post_turn_slot_updates(&pre, &drained, None, false, false, "s1", now)
            .expect("no cap active");
        let written = out["_llm"]["session"]["s1"]["started_at"]
            .as_str()
            .expect("session key must be string");
        assert_eq!(written, earlier.to_rfc3339());
    }

    #[test]
    fn apply_caps_passes_on_empty_snapshot() {
        let snap = SlotSnapshot::default();
        apply_caps(&snap, &cfg(), Utc::now()).expect("empty snapshot must pass");
    }

    #[test]
    fn apply_caps_rejects_consecutive_no_tool_call_at_max_iterations() {
        let snap = SlotSnapshot {
            consecutive_no_tool_call: 3,
            ..Default::default()
        };
        let err = apply_caps(&snap, &cfg(), Utc::now()).unwrap_err();
        match err {
            ExecutorError::Llm(LlmErrorCode::ExecutionExhausted, msg) => {
                assert!(msg.contains("F1"), "expected F1 mention, got: {msg}");
            }
            other => panic!("expected ExecutionExhausted, got {other:?}"),
        }
    }
}
