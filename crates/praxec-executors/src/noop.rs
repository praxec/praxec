use async_trait::async_trait;
use praxec_core::error::ExecutorError;
use praxec_core::model::{ExecuteRequest, ExecuteResult};
use praxec_core::ports::Executor;
use serde_json::json;

/// An executor that always succeeds with `{}`. **First-class semantics**
/// — used as:
/// - the default executor for `proxy.expose` entries that don't specify
///   one (deliberate: a proxy that only re-shapes a capability without
///   firing side effects should not require an executor stanza), and
/// - the load-bearing executor for transitions that exist to advance
///   state without performing work (gate checks, human approvals,
///   guidance-only transitions).
///
/// This is committed in STABILITY Tier 1. Treating `noop` as "missing
/// implementation" is a contributor mistake; see docs/reference/stability.md for the
/// distinction between first-class no-op semantics and unfinished work.
pub struct NoopExecutor;

#[async_trait]
impl Executor for NoopExecutor {
    async fn execute(&self, _request: ExecuteRequest) -> Result<ExecuteResult, ExecutorError> {
        Ok(ExecuteResult {
            output: json!({}),
            evidence: vec![],
            child_workflow_id: None,
            next_transition: None,
            suspend: None,
            telemetry: None,
        })
    }
}
