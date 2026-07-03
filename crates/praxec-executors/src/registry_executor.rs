//! SPEC §17.2 + §8.4 — `registry` executor. Writes a candidate definition
//! through `DefinitionStoreWritable`. Feature-flagged: with
//! `praxec.authoring.write_enabled: false` (default), the executor fails
//! fast with `WRITE_DISABLED` and performs no I/O.

use std::sync::Arc;

use async_trait::async_trait;
use praxec_core::error::ExecutorError;
use praxec_core::model::{ExecuteRequest, ExecuteResult};
use praxec_core::ports::{DefinitionStoreWritable, Executor};
use serde_json::{Value, json};

/// Construct with `Some(writable)` when the flag is on, `None` when off.
/// The executor's behavior is governed by the variant: when `None`, every
/// invocation fails fast with `WRITE_DISABLED`.
pub struct RegistryExecutor {
    writable: Option<Arc<dyn DefinitionStoreWritable>>,
    /// Operator's top-level connection names — the only connections an
    /// authored definition may reference (provenance gate, README trust model).
    allowed_connections: Vec<String>,
}

impl RegistryExecutor {
    pub fn new(writable: Option<Arc<dyn DefinitionStoreWritable>>) -> Self {
        Self {
            writable,
            allowed_connections: Vec::new(),
        }
    }

    pub fn enabled(writable: Arc<dyn DefinitionStoreWritable>) -> Self {
        Self {
            writable: Some(writable),
            allowed_connections: Vec::new(),
        }
    }

    pub fn disabled() -> Self {
        Self {
            writable: None,
            allowed_connections: Vec::new(),
        }
    }

    /// Declare the operator's trusted connection names; an authored definition
    /// may reference these (but never introduce a raw command of its own).
    pub fn with_allowed_connections(mut self, names: Vec<String>) -> Self {
        self.allowed_connections = names;
        self
    }
}

#[async_trait]
impl Executor for RegistryExecutor {
    async fn execute(&self, request: ExecuteRequest) -> Result<ExecuteResult, ExecutorError> {
        let args = &request.arguments;
        let ctx = &request.workflow.context;
        // Prefer explicit arguments; otherwise fall back to the validated
        // candidate on the blackboard. In the reference authoring workflow
        // (§17.1) the `publish` transition fires with `arguments: {}` and
        // publishes exactly the candidate that was structurally checked +
        // dry-run — never a fresh re-submission (which would let a caller
        // validate one definition and publish a different one). Mirrors the
        // same context fallback the `dry_run` executor uses.
        let definition_id = args
            .get("definition_id")
            .and_then(Value::as_str)
            .or_else(|| ctx.get("candidate_definition_id").and_then(Value::as_str))
            .ok_or_else(|| {
                ExecutorError::Permanent(
                    "registry: missing `definition_id` (and no `candidate_definition_id` \
                     in workflow context)"
                        .into(),
                )
            })?
            .to_string();
        let definition = args
            .get("definition")
            .cloned()
            .or_else(|| ctx.get("candidate_definition").cloned())
            .ok_or_else(|| {
                ExecutorError::Permanent(
                    "registry: missing `definition` (and no `candidate_definition` in \
                     workflow context)"
                        .into(),
                )
            })?;
        let definition_id = definition_id.as_str();

        // Provenance gate (hard backstop): an authored/published definition may
        // only execute via hash-pinned `kind: script` + operator-declared
        // connections — never a fresh raw command. Rejected BEFORE any write.
        if let Some(reason) = crate::untrusted_execution::untrusted_execution_reason(
            &definition,
            &self.allowed_connections,
        ) {
            return Err(ExecutorError::Permanent(format!(
                "UNTRUSTED_EXECUTION_IN_PUBLISHED_DEFINITION: '{definition_id}' {reason}. \
                 Authored definitions may only run hash-pinned `kind: script` and reference \
                 operator-declared connections."
            )));
        }

        let Some(writable) = self.writable.as_ref() else {
            return Ok(ExecuteResult {
                output: json!({
                    "error": "WRITE_DISABLED",
                    "message": "registry executor invoked while \
                                praxec.authoring.write_enabled is false",
                }),
                evidence: vec![],
                child_workflow_id: None,
                next_transition: None,
                suspend: None,
                telemetry: None,
            });
        };

        // Optimistic-concurrency basis for an edit-publish. In precedence:
        //   1. an explicit `expected_prior_hash` argument,
        //   2. the `base_definition_hash` the edit flow stamped on the blackboard,
        //   3. the hash of the full `base_definition` snapshot the edit was read
        //      from (so authors pass what they read, not a hash they compute).
        // Absent entirely for a create (unconditional write).
        let expected_prior_hash = args
            .get("expected_prior_hash")
            .and_then(Value::as_str)
            .map(str::to_string)
            .or_else(|| {
                ctx.get("base_definition_hash")
                    .and_then(Value::as_str)
                    .map(str::to_string)
            })
            .or_else(|| {
                args.get("base_definition")
                    .or_else(|| ctx.get("base_definition"))
                    .map(praxec_core::config::compute_definition_hash)
            });

        match writable
            .register(definition_id, definition, expected_prior_hash.as_deref())
            .await
        {
            Ok(()) => Ok(ExecuteResult {
                output: json!({
                    "definitionId": definition_id,
                    "outcome":      "published",
                }),
                evidence: vec![],
                child_workflow_id: None,
                next_transition: None,
                suspend: None,
                telemetry: None,
            }),
            Err(e) => Err(ExecutorError::Permanent(format!(
                "registry: register('{definition_id}') failed: {e}"
            ))),
        }
    }
}
