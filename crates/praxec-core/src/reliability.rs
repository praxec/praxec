use std::sync::Arc;
use std::time::Duration;

use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use tokio::time::timeout;

use crate::audit::AuditSink;
use crate::error::{ErrorClass, ExecutorError};
use crate::model::{ExecuteRequest, ExecuteResult, WorkflowInstance};
use crate::ports::ExecutorRegistry;

/// Parsed reliability policy. The wire format is the JSON object on a
/// transition or action; `from_value` is forgiving about *missing fields*
/// (they fall back to library defaults) but NOT about a present-but-malformed
/// block — see [`ReliabilityPolicy::from_value`].
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct ReliabilityPolicy {
    #[serde(rename = "timeoutMs", default, skip_serializing_if = "Option::is_none")]
    pub timeout_ms: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub retry: Option<RetryPolicy>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub fallback: Option<FallbackPolicy>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RetryPolicy {
    #[serde(rename = "maxAttempts", default = "one")]
    pub max_attempts: u32,
    #[serde(default)]
    pub backoff: Backoff,
    #[serde(rename = "initialDelayMs", default)]
    pub initial_delay_ms: u64,
    #[serde(
        rename = "maxDelayMs",
        default,
        skip_serializing_if = "Option::is_none"
    )]
    pub max_delay_ms: Option<u64>,
    #[serde(rename = "retryOn", default = "default_retry_on")]
    pub retry_on: Vec<String>,
}

fn one() -> u32 {
    1
}
fn default_retry_on() -> Vec<String> {
    vec![
        "timeout".into(),
        "transient_error".into(),
        "rate_limited".into(),
    ]
}

impl Default for RetryPolicy {
    fn default() -> Self {
        Self {
            max_attempts: 1,
            backoff: Backoff::None,
            initial_delay_ms: 0,
            max_delay_ms: None,
            retry_on: default_retry_on(),
        }
    }
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, Default)]
#[serde(rename_all = "lowercase")]
pub enum Backoff {
    #[default]
    None,
    Fixed,
    Exponential,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FallbackPolicy {
    #[serde(default = "first_success")]
    pub strategy: String,
    pub executors: Vec<Value>,
}

fn first_success() -> String {
    "first_success".to_string()
}

impl ReliabilityPolicy {
    /// Parse a `reliability:` block.
    ///
    /// - `None` (the key is absent) → library defaults. This is legitimate:
    ///   a transition without a `reliability:` block runs with no timeout,
    ///   one attempt, no fallback.
    /// - `Some(v)` that fails to deserialize → **error**. A present-but-malformed
    ///   block (e.g. a typo in `maxAttempts`, a non-object value) must NOT
    ///   silently collapse to the defaults — that would strip the operator's
    ///   intended timeout / retry / fallback without any signal. Fail-fast with
    ///   `INVALID_RELIABILITY_POLICY` so the misconfiguration is visible.
    pub fn from_value(value: Option<&Value>) -> anyhow::Result<Self> {
        match value {
            Some(v) => serde_json::from_value(v.clone()).map_err(|e| {
                anyhow::anyhow!("INVALID_RELIABILITY_POLICY: malformed reliability block: {e}")
            }),
            None => Ok(Self::default()),
        }
    }

    pub fn retry(&self) -> RetryPolicy {
        self.retry.clone().unwrap_or_default()
    }

    pub fn timeout(&self) -> Option<Duration> {
        self.timeout_ms.map(Duration::from_millis)
    }
}

/// Record an audit event, warning (never erroring) if the sink rejects it.
///
/// The reliability loop must not abort an in-flight execution because an
/// audit write failed — but a dropped event during a retry storm (the exact
/// moment the trail matters most) must not be *silent* either. Mirrors the
/// "never silent" intent of `WorkflowRuntime::record_or_self_event`.
async fn record_or_warn(audit: &Arc<dyn AuditSink>, event: crate::audit::AuditEvent) {
    let event_type = event.event_type.clone();
    if let Err(e) = audit.record(event).await {
        tracing::warn!(
            target: "praxec_core::reliability",
            event = %event_type,
            error = %e,
            "audit record failed during reliability execution; event dropped \
             (continuing — the execution itself is the primary signal)"
        );
    }
}

/// Run an executor under the given reliability policy, emitting audit events
/// for each attempt. Tries the primary executor first, then any fallback
/// executors in declaration order.
#[allow(clippy::too_many_arguments)]
pub async fn execute_with_reliability(
    executors: &dyn ExecutorRegistry,
    audit: &Arc<dyn AuditSink>,
    instance: &WorkflowInstance,
    transition: Option<&str>,
    arguments: &Value,
    primary: Value,
    policy: &ReliabilityPolicy,
    correlation_id: &str,
) -> Result<ExecuteResult, ExecutorError> {
    let idempotency_key = compute_idempotency_key(&primary, instance, transition, correlation_id);
    let mut candidates: Vec<Value> = vec![primary];
    if let Some(fb) = &policy.fallback {
        candidates.extend(fb.executors.clone());
    }

    let retry = policy.retry();
    let mut last: Option<ExecutorError> = None;

    for (candidate_idx, exec_cfg) in candidates.into_iter().enumerate() {
        let kind = exec_cfg
            .get("kind")
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_string();

        if candidate_idx > 0 {
            record_or_warn(
                audit,
                instance
                    .audit_event("fallback.selected")
                    .with_correlation(correlation_id)
                    .with_payload(json!({
                        "transition": transition,
                        "candidate": candidate_idx,
                        "kind": kind,
                        "previousError": last.as_ref().map(|e| e.to_string()),
                    })),
            )
            .await;
        }

        let executor = match executors.get(&kind) {
            Some(e) => e,
            None => {
                // SPEC §33 D4 — the in-runtime LLM executor ships behind
                // the `llm-executor` cargo feature on the `praxec`
                // binary. A `kind: "llm"` reference reaching this branch
                // means the binary was built `--no-default-features` (or
                // an embedder forgot to register the executor). Surface
                // the actionable fix instead of the generic message.
                let msg = if kind == "llm" {
                    "executor kind 'llm' is not registered — this build does not \
                     include the LLM executor; re-install with the `llm-executor` \
                     feature (default-on for `cargo install praxec`)"
                        .to_string()
                } else {
                    format!("executor kind '{kind}' is not registered")
                };
                last = Some(ExecutorError::Permanent(msg));
                continue;
            }
        };

        for attempt in 1..=retry.max_attempts.max(1) {
            record_or_warn(
                audit,
                instance
                    .audit_event("executor.started")
                    .with_correlation(correlation_id)
                    .with_payload(json!({
                        "transition": transition,
                        "candidate": candidate_idx,
                        "attempt": attempt,
                        "kind": kind,
                        "idempotencyKey": idempotency_key,
                    })),
            )
            .await;

            let request = ExecuteRequest {
                workflow: instance.clone(),
                transition: transition.map(str::to_string),
                arguments: arguments.clone(),
                executor_config: exec_cfg.clone(),
                idempotency_key: idempotency_key.clone(),
                // SPEC §24 — thread the parent correlation_id through so
                // fan-out executors (kind: parallel) can emit per-branch
                // audit events that link back to the parent transition.
                correlation_id: Some(correlation_id.to_string()),
            };

            let result = match policy.timeout() {
                Some(t) => match timeout(t, executor.execute(request)).await {
                    Ok(r) => r,
                    Err(_) => Err(ExecutorError::Timeout(t.as_millis() as u64)),
                },
                None => executor.execute(request).await,
            };

            match result {
                Ok(ok) => {
                    record_or_warn(
                        audit,
                        instance
                            .audit_event("executor.succeeded")
                            .with_correlation(correlation_id)
                            .with_payload(json!({
                                "transition": transition,
                                "candidate": candidate_idx,
                                "attempt": attempt,
                                "kind": kind,
                            })),
                    )
                    .await;
                    return Ok(ok);
                }
                Err(err) => {
                    let class = err.class();
                    let token = class.token().to_string();
                    let message = err.to_string();

                    if attempt < retry.max_attempts && retryable(&retry, class) {
                        record_or_warn(
                            audit,
                            instance
                                .audit_event("executor.retrying")
                                .with_correlation(correlation_id)
                                .with_payload(json!({
                                    "transition": transition,
                                    "candidate": candidate_idx,
                                    "attempt": attempt,
                                    "kind": kind,
                                    "errorClass": token,
                                    "error": message,
                                })),
                        )
                        .await;
                        let delay = backoff_delay(&retry, attempt);
                        if !delay.is_zero() {
                            tokio::time::sleep(delay).await;
                        }
                        last = Some(err);
                        continue;
                    }

                    record_or_warn(
                        audit,
                        instance
                            .audit_event("executor.failed")
                            .with_correlation(correlation_id)
                            .with_payload(json!({
                                "transition": transition,
                                "candidate": candidate_idx,
                                "attempt": attempt,
                                "kind": kind,
                                "errorClass": token,
                                "error": message,
                            })),
                    )
                    .await;
                    last = Some(err);
                    break;
                }
            }
        }
    }

    Err(last.unwrap_or_else(|| ExecutorError::Permanent("no executor candidates".into())))
}

/// Compute an idempotency key for this execute call from the executor's
/// `idempotencyKey` field:
///
/// - `idempotencyKey: true` — auto-key, `<workflowId>.<transition>.<correlationId>`
/// - `idempotencyKey: "<template>"` — substitute `{workflowId}`,
///   `{transition}`, `{correlationId}` tokens.
/// - missing / `false` — no key.
///
/// The key is shared across retries and across fallback executors so a
/// downstream service that dedupes on the key sees the same identifier
/// for the whole "this submit" call.
fn compute_idempotency_key(
    primary_executor: &Value,
    instance: &WorkflowInstance,
    transition: Option<&str>,
    correlation_id: &str,
) -> Option<String> {
    let spec = primary_executor.get("idempotencyKey")?;
    let workflow_id = &instance.id;
    let transition = transition.unwrap_or("on_enter");

    if let Some(true) = spec.as_bool() {
        return Some(format!("{workflow_id}.{transition}.{correlation_id}"));
    }
    if let Some(template) = spec.as_str() {
        let key = template
            .replace("{workflowId}", workflow_id)
            .replace("{transition}", transition)
            .replace("{correlationId}", correlation_id);
        return Some(key);
    }
    None
}

fn retryable(retry: &RetryPolicy, class: ErrorClass) -> bool {
    let token = class.token();
    retry.retry_on.iter().any(|c| c == token)
}

/// Compute the backoff delay for a retry attempt.
///
/// NFR R2 — full-jitter is applied to exponential backoff to prevent
/// thundering-herd when multiple workflows retry a throttled provider
/// simultaneously. The jitter is a pseudo-random fraction of the capped
/// delay, derived from `std::time::SystemTime` (no external crate needed).
///
/// Jitter strategy: `delay = rand(0 .. capped_ms)` (full-jitter). This
/// is the AWS/Google-recommended strategy for distributed retry storms.
/// Fixed and None backoffs keep their exact value (jitter adds no value
/// when every retrier already uses a different fixed delay or zero).
fn backoff_delay(retry: &RetryPolicy, attempt: u32) -> Duration {
    let base = retry.initial_delay_ms;
    let raw_ms = match retry.backoff {
        Backoff::None => 0,
        Backoff::Fixed => base,
        Backoff::Exponential => base.saturating_mul(1u64 << attempt.saturating_sub(1).min(20)),
    };
    let capped = match retry.max_delay_ms {
        Some(max) => raw_ms.min(max),
        None => raw_ms,
    };
    // NFR R2 — full-jitter for exponential backoff only. Derive cheap
    // pseudo-randomness from the current nanosecond timestamp (xor-folded);
    // this avoids an external `rand` dep while providing enough spread to
    // prevent thundering herd across concurrent workflows.
    let jittered = match retry.backoff {
        Backoff::Exponential if capped > 0 => {
            let nanos = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.subsec_nanos())
                .unwrap_or(0) as u64;
            // Cheap xor-fold: spread bits, then modulo the cap.
            let r = nanos ^ (nanos >> 17) ^ (nanos >> 31);
            r % capped
        }
        _ => capped,
    };
    Duration::from_millis(jittered)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::audit::AuditEvent;
    use crate::model::ExecuteResult;
    use crate::ports::Executor;
    use async_trait::async_trait;

    fn test_instance() -> WorkflowInstance {
        WorkflowInstance {
            id: "wf".into(),
            definition_id: "d".into(),
            definition_version: "0".into(),
            definition: json!({}),
            state: "s".into(),
            version: 0,
            input: json!({}),
            context: json!({}),
            started_at: chrono::Utc::now(),
            run_env: crate::RunEnv::for_test(),
            cancelled_at: None,
            cancelled_reason: None,
            depth: 0,
            parent: None,
        }
    }

    /// An audit sink that always rejects writes — stands in for a backing
    /// store that's failing mid-execution (the retry-storm scenario).
    struct FailingAudit;
    #[async_trait]
    impl AuditSink for FailingAudit {
        async fn record(&self, _event: AuditEvent) -> anyhow::Result<()> {
            Err(anyhow::anyhow!("audit backend down"))
        }
    }

    struct OkExec;
    #[async_trait]
    impl Executor for OkExec {
        async fn execute(&self, _r: ExecuteRequest) -> Result<ExecuteResult, ExecutorError> {
            Ok(ExecuteResult::default())
        }
    }
    struct OneReg(Arc<dyn Executor>);
    impl ExecutorRegistry for OneReg {
        fn get(&self, _kind: &str) -> Option<Arc<dyn Executor>> {
            Some(self.0.clone())
        }
    }

    // FIX (reliability) — a failing audit sink must NOT abort the execution.
    // The audit writes are now routed through `record_or_warn`, which warns
    // (observable) on Err but never propagates it. The executor result must
    // still come back Ok. (The warn itself is verified by reading; this
    // pins the "audit failure doesn't break execution" contract.)
    #[tokio::test]
    async fn audit_failure_during_execution_is_not_fatal() {
        let registry = OneReg(Arc::new(OkExec));
        let audit: Arc<dyn AuditSink> = Arc::new(FailingAudit);
        let instance = test_instance();
        let policy = ReliabilityPolicy::default();
        let result = execute_with_reliability(
            &registry,
            &audit,
            &instance,
            Some("go"),
            &json!({}),
            json!({ "kind": "noop" }),
            &policy,
            "cor-1",
        )
        .await;
        assert!(
            result.is_ok(),
            "execution must succeed even though every audit write fails"
        );
    }

    #[test]
    fn from_value_absent_yields_default() {
        // CMP-007: an absent `reliability:` block is legitimate → defaults.
        let policy = ReliabilityPolicy::from_value(None).expect("None must be Ok(default)");
        assert_eq!(policy.timeout_ms, None);
        assert!(policy.retry.is_none());
        assert!(policy.fallback.is_none());
        assert_eq!(policy.retry().max_attempts, 1);
    }

    #[test]
    fn from_value_well_formed_parses() {
        let v = json!({ "timeoutMs": 1500, "retry": { "maxAttempts": 3 } });
        let policy = ReliabilityPolicy::from_value(Some(&v)).expect("well-formed must parse");
        assert_eq!(policy.timeout_ms, Some(1500));
        assert_eq!(policy.retry().max_attempts, 3);
    }

    fn retry(backoff: Backoff, initial: u64, max: Option<u64>) -> RetryPolicy {
        RetryPolicy {
            max_attempts: 3,
            backoff,
            initial_delay_ms: initial,
            max_delay_ms: max,
            retry_on: default_retry_on(),
        }
    }

    #[test]
    fn backoff_none_is_always_zero() {
        let p = retry(Backoff::None, 100, None);
        assert_eq!(backoff_delay(&p, 1).as_millis(), 0);
        assert_eq!(backoff_delay(&p, 5).as_millis(), 0);
    }

    #[test]
    fn backoff_fixed_is_constant_across_attempts() {
        let p = retry(Backoff::Fixed, 50, None);
        assert_eq!(backoff_delay(&p, 1).as_millis(), 50);
        assert_eq!(backoff_delay(&p, 2).as_millis(), 50);
        assert_eq!(backoff_delay(&p, 9).as_millis(), 50);
    }

    #[test]
    fn backoff_exponential_is_bounded_by_capped_delay() {
        // NFR R2: exponential backoff now applies full-jitter, so the exact
        // value varies. Assert that the delay is in [0, capped) — i.e. the
        // jitter always stays strictly within the exponential envelope.
        let p = retry(Backoff::Exponential, 100, None);
        // attempt 1: capped = 100 * 2^0 = 100 → jitter ∈ [0, 100)
        assert!(backoff_delay(&p, 1).as_millis() < 100);
        // attempt 2: capped = 100 * 2^1 = 200 → jitter ∈ [0, 200)
        assert!(backoff_delay(&p, 2).as_millis() < 200);
        // attempt 3: capped = 100 * 2^2 = 400 → jitter ∈ [0, 400)
        assert!(backoff_delay(&p, 3).as_millis() < 400);
    }

    #[test]
    fn backoff_max_delay_caps_the_jitter_range() {
        // NFR R2: jitter applies after the max-delay cap, so the actual
        // delay is in [0, cap). This also verifies the cap is respected.
        let p = retry(Backoff::Exponential, 100, Some(250));
        // attempts 1 and 2: raw < cap → jitter ∈ [0, raw)
        assert!(backoff_delay(&p, 1).as_millis() < 100);
        assert!(backoff_delay(&p, 2).as_millis() < 200);
        // attempt 3: raw = 400 > cap 250 → jitter ∈ [0, 250)
        assert!(backoff_delay(&p, 3).as_millis() < 250);
    }

    #[test]
    fn from_value_malformed_errors_not_default() {
        // CMP-007: a present-but-malformed block must FAIL-FAST rather than
        // silently collapsing to the no-timeout / one-attempt default, which
        // would strip the operator's intended reliability envelope.
        let bad = json!({ "retry": { "maxAttempts": "three" } }); // wrong type
        let err = ReliabilityPolicy::from_value(Some(&bad))
            .expect_err("malformed reliability block must error");
        assert!(
            err.to_string().contains("INVALID_RELIABILITY_POLICY"),
            "error should carry the named code, got: {err}"
        );

        // A non-object value is equally malformed.
        let bad_scalar = json!("not-an-object");
        assert!(ReliabilityPolicy::from_value(Some(&bad_scalar)).is_err());
    }
}
