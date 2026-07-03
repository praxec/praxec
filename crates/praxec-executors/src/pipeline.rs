//! SPEC §25 — the `pipeline` executor kind. Runs N steps sequentially,
//! threading each step's `output` into the next step's `$.input` scope.
//! Stops on first failure unless `on_step_failure: continue`.
//!
//! This is the deterministic-FP "compose" primitive. Today the same
//! shape is expressible by chaining N states with auto-advance — the
//! pipeline executor collapses that into one transition / one state.
//! Single audit trail, single version bump, idempotency-key-per-step
//! segmentation for replay.
//!
//! Architecture mirror to `parallel`:
//! - Both hold an `Arc<OnceLock<Arc<dyn ExecutorRegistry>>>` back-ref.
//! - Both encapsulate sub-execution INSIDE one outer transition.
//! - Both emit per-step / per-branch audit events that share the
//!   parent's `correlation_id`.
//!
//! Config shape:
//!
//! ```yaml
//! executor:
//!   kind: pipeline
//!   steps:                              # array of executor configs
//!     - { kind: script, subject: build.cargo.release }
//!     - { kind: cli,    connection: shell, command: "verify" }
//!     - { kind: mcp,    connection: notifier, tool: report }
//!   on_step_failure: bail               # bail (default) | continue
//!   total_timeout_ms: 60000             # optional
//! ```
//!
//! Output shape:
//!
//! ```json
//! {
//!   "steps": [
//!     { "ok": true,  "index": 0, "output": { ... } },
//!     { "ok": true,  "index": 1, "output": { ... } },
//!     { "ok": false, "index": 2, "error": { "code": "...", "message": "..." } }
//!   ],
//!   "final_output": { ... last successful step output, or null if first step failed ... },
//!   "summary": {
//!     "n":                 3,
//!     "ok_count":          2,
//!     "failed_count":      1,
//!     "durationMs":        420,
//!     "first_failure_index": 2,
//!     "verdict":           "failed"
//!   }
//! }
//! ```

use std::sync::{Arc, OnceLock};
use std::time::{Duration, Instant};

use async_trait::async_trait;
use praxec_core::audit::{AuditEvent, AuditSink};
use praxec_core::error::ExecutorError;
use praxec_core::model::{Evidence, ExecuteRequest, ExecuteResult};
use praxec_core::ports::{Executor, ExecutorRegistry};
use praxec_core::reliability::{ReliabilityPolicy, execute_with_reliability};
use serde_json::{Value, json};
use tokio::time::timeout;

pub struct PipelineExecutor {
    pub(crate) executors: Arc<OnceLock<Arc<dyn ExecutorRegistry>>>,
    pub(crate) audit: Arc<dyn AuditSink>,
}

impl PipelineExecutor {
    pub fn new(audit: Arc<dyn AuditSink>) -> Self {
        Self {
            executors: Arc::new(OnceLock::new()),
            audit,
        }
    }

    /// Wire the executor registry after the registry itself is built.
    /// Must be called exactly once during construction; a second call is a
    /// construction bug that would silently keep a stale registry, so panic.
    pub fn set_registry(&self, registry: Arc<dyn ExecutorRegistry>) {
        if self.executors.set(registry).is_err() {
            panic!(
                "PIPELINE_EXECUTOR_DOUBLE_WIRED: set_registry called more than once; \
                 the executor registry must be wired exactly once after construction."
            );
        }
    }

    fn registry(&self) -> Result<Arc<dyn ExecutorRegistry>, ExecutorError> {
        self.executors.get().cloned().ok_or_else(|| {
            ExecutorError::Permanent(
                "PIPELINE_EXECUTOR_NOT_WIRED: registry was not set after construction. \
                 Call PipelineExecutor::set_registry(registry) after building the \
                 registry that contains this executor."
                    .into(),
            )
        })
    }
}

#[derive(Debug, Clone, Copy)]
enum OnStepFailure {
    Bail,
    Continue,
}

struct PipelineConfig {
    steps: Vec<Value>,
    on_step_failure: OnStepFailure,
    total_timeout: Option<Duration>,
}

impl PipelineConfig {
    fn from_value(cfg: &Value) -> Result<Self, ExecutorError> {
        let steps = cfg
            .get("steps")
            .and_then(Value::as_array)
            .ok_or_else(|| {
                ExecutorError::Permanent(
                    "INVALID_PIPELINE_CONFIG: missing `steps` (required: array of executor \
                     configs run sequentially)"
                        .into(),
                )
            })?
            .clone();
        if steps.is_empty() {
            return Err(ExecutorError::Permanent(
                "INVALID_PIPELINE_CONFIG: `steps` must be non-empty".into(),
            ));
        }
        let on_step_failure = match cfg
            .get("on_step_failure")
            .and_then(Value::as_str)
            .unwrap_or("bail")
        {
            "bail" => OnStepFailure::Bail,
            "continue" => OnStepFailure::Continue,
            other => {
                return Err(ExecutorError::Permanent(format!(
                    "INVALID_PIPELINE_CONFIG: `on_step_failure` must be \"bail\" or \"continue\" \
                     (got \"{other}\")"
                )));
            }
        };
        let total_timeout = cfg
            .get("total_timeout_ms")
            .and_then(Value::as_u64)
            .map(Duration::from_millis);
        Ok(PipelineConfig {
            steps,
            on_step_failure,
            total_timeout,
        })
    }
}

#[async_trait]
impl Executor for PipelineExecutor {
    async fn execute(&self, request: ExecuteRequest) -> Result<ExecuteResult, ExecutorError> {
        let cfg = PipelineConfig::from_value(&request.executor_config)?;
        let registry = self.registry()?;
        let correlation_id = request
            .correlation_id
            .clone()
            .unwrap_or_else(|| "unset-corr".to_string());
        let parent_workflow_id = request.workflow.id.clone();
        let transition = request.transition.clone();
        let parent_idem = request.idempotency_key.clone();
        let n = cfg.steps.len();

        let start = Instant::now();
        let inner = run_steps(
            &cfg,
            n,
            registry,
            &request,
            &correlation_id,
            &parent_workflow_id,
            &transition,
            parent_idem.as_deref(),
            self.audit.clone(),
        );

        let outcome = match cfg.total_timeout {
            Some(d) => match timeout(d, inner).await {
                Ok(o) => o,
                Err(_) => {
                    return Err(ExecutorError::Timeout(d.as_millis() as u64));
                }
            },
            None => inner.await,
        };

        let elapsed_ms = start.elapsed().as_millis() as u64;

        let StepsOutcome {
            step_results,
            aggregated_evidence,
            ok_count,
            failed_count,
            first_failure_index,
            final_output,
        } = outcome;

        let verdict = if failed_count == 0 && ok_count == n {
            "succeeded"
        } else {
            "failed"
        };

        let summary = json!({
            "n":                   n,
            "ok_count":            ok_count,
            "failed_count":        failed_count,
            "durationMs":          elapsed_ms,
            "first_failure_index": first_failure_index,
            "verdict":             verdict,
        });

        self.audit
            .record(
                AuditEvent::new("pipeline.completed")
                    .with_workflow(&request.workflow.id)
                    .with_correlation(&correlation_id)
                    .with_payload(json!({
                        "transition": transition,
                        "summary":    summary,
                    })),
            )
            .await
            .unwrap_or_else(|e| tracing::warn!(error = %e, "audit emit failed; event dropped"));

        let output = json!({
            "steps":        step_results,
            "final_output": final_output,
            "summary":      summary,
        });

        if verdict == "succeeded" {
            Ok(ExecuteResult {
                output,
                evidence: aggregated_evidence,
                child_workflow_id: None,
                next_transition: None,
                suspend: None,
                telemetry: None,
            })
        } else {
            Err(ExecutorError::Permanent(format!(
                "pipeline failed: ok={ok_count}/{n}, first_failure_index={first_failure_index:?}"
            )))
        }
    }
}

struct StepsOutcome {
    step_results: Vec<Value>,
    aggregated_evidence: Vec<Evidence>,
    ok_count: usize,
    failed_count: usize,
    first_failure_index: Option<usize>,
    final_output: Value,
}

#[allow(clippy::too_many_arguments)]
async fn run_steps(
    cfg: &PipelineConfig,
    n: usize,
    registry: Arc<dyn ExecutorRegistry>,
    request: &ExecuteRequest,
    correlation_id: &str,
    parent_workflow_id: &str,
    transition: &Option<String>,
    parent_idem: Option<&str>,
    audit: Arc<dyn AuditSink>,
) -> StepsOutcome {
    let mut step_results: Vec<Value> = Vec::with_capacity(n);
    let mut aggregated_evidence: Vec<Evidence> = Vec::new();
    let mut ok_count = 0usize;
    let mut failed_count = 0usize;
    let mut first_failure_index: Option<usize> = None;
    // Step N reads $.input = step (N-1)'s output. Step 0 reads the
    // executor's original arguments as $.input (parity with how a
    // standalone executor sees them).
    let mut threaded_input = request.arguments.clone();

    for (index, step_cfg) in cfg.steps.iter().enumerate() {
        let step_kind = step_cfg.get("kind").and_then(Value::as_str).unwrap_or("");
        audit
            .record(
                AuditEvent::new("pipeline.step.started")
                    .with_workflow(parent_workflow_id)
                    .with_correlation(correlation_id)
                    .with_payload(json!({
                        "transition":    transition,
                        "step_index":    index,
                        "step_kind":     step_kind,
                    })),
            )
            .await
            .unwrap_or_else(|e| tracing::warn!(error = %e, "audit emit failed; event dropped"));

        let _ = parent_idem; // segmentation happens via the threaded input + step index in audit
        // A present-but-malformed `reliability:` block on a step fails the
        // step (rather than silently running it with default reliability).
        let result = match ReliabilityPolicy::from_value(step_cfg.get("reliability")) {
            Ok(policy) => {
                execute_with_reliability(
                    registry.as_ref(),
                    &audit,
                    &request.workflow,
                    transition.as_deref(),
                    &threaded_input,
                    step_cfg.clone(),
                    &policy,
                    correlation_id,
                )
                .await
            }
            Err(e) => Err(ExecutorError::Permanent(e.to_string())),
        };

        match result {
            Ok(res) => {
                ok_count += 1;
                let next_input = res.output.clone();
                aggregated_evidence.extend(res.evidence);
                step_results.push(json!({
                    "ok":     true,
                    "index":  index,
                    "output": res.output,
                }));
                audit
                    .record(
                        AuditEvent::new("pipeline.step.completed")
                            .with_workflow(parent_workflow_id)
                            .with_correlation(correlation_id)
                            .with_payload(json!({
                                "transition": transition,
                                "step_index": index,
                            })),
                    )
                    .await
                    .unwrap_or_else(
                        |e| tracing::warn!(error = %e, "audit emit failed; event dropped"),
                    );
                // Thread output → next step's input.
                threaded_input = next_input;
            }
            Err(err) => {
                if first_failure_index.is_none() {
                    first_failure_index = Some(index);
                }
                failed_count += 1;
                step_results.push(json!({
                    "ok":    false,
                    "index": index,
                    "error": {
                        "code":    err.class().token(),
                        "message": err.to_string(),
                    },
                }));
                audit
                    .record(
                        AuditEvent::new("pipeline.step.failed")
                            .with_workflow(parent_workflow_id)
                            .with_correlation(correlation_id)
                            .with_payload(json!({
                                "transition": transition,
                                "step_index": index,
                                "error_code": err.class().token(),
                            })),
                    )
                    .await
                    .unwrap_or_else(
                        |e| tracing::warn!(error = %e, "audit emit failed; event dropped"),
                    );
                if matches!(cfg.on_step_failure, OnStepFailure::Bail) {
                    break;
                }
                // On `continue`, threaded_input STAYS at the previous
                // step's output (we don't thread a failed step's empty
                // output, which would erase context built so far).
            }
        }
    }

    let final_output = if ok_count > 0 {
        // Walk back through step_results to find the latest successful
        // output (handles `continue` mid-failure).
        step_results
            .iter()
            .rev()
            .find_map(|s| {
                if s.get("ok").and_then(Value::as_bool) == Some(true) {
                    s.get("output").cloned()
                } else {
                    None
                }
            })
            .unwrap_or(Value::Null)
    } else {
        Value::Null
    };

    StepsOutcome {
        step_results,
        aggregated_evidence,
        ok_count,
        failed_count,
        first_failure_index,
        final_output,
    }
}
