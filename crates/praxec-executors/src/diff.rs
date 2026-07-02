//! SPEC §8.4 — `diff` executor. A deterministic review tool for the edit
//! authoring flow: it renders the unified line diff between the definition
//! currently on record (`base_definition`) and the proposed `candidate_definition`
//! so a human or the model can see exactly what an edit changes before it's
//! published. Pure + side-effect-free; the diff itself is computed by
//! `core::config::definition_diff`, never by a prompt.

use async_trait::async_trait;
use praxec_core::config::definition_diff;
use praxec_core::error::ExecutorError;
use praxec_core::model::{ExecuteRequest, ExecuteResult};
use praxec_core::ports::Executor;
use serde_json::{json, Value};

pub struct DiffExecutor;

/// Resolve a definition argument by its explicit name, then the edit flow's
/// blackboard slot, mirroring the context fallback `dry_run` / `registry` use.
fn resolve<'a>(args: &'a Value, ctx: &'a Value, arg: &str, slot: &str) -> Option<&'a Value> {
    args.get(arg).or_else(|| ctx.get(slot))
}

#[async_trait]
impl Executor for DiffExecutor {
    async fn execute(&self, request: ExecuteRequest) -> Result<ExecuteResult, ExecutorError> {
        let args = &request.arguments;
        let ctx = &request.workflow.context;

        let base = resolve(args, ctx, "base_definition", "base_definition").ok_or_else(|| {
            ExecutorError::Permanent(
                "diff: missing `base_definition` (and none on the blackboard) — the \
                 definition currently on record to diff against"
                    .into(),
            )
        })?;
        let candidate = resolve(args, ctx, "candidate_definition", "candidate_definition")
            .ok_or_else(|| {
                ExecutorError::Permanent(
                    "diff: missing `candidate_definition` (and none on the blackboard) — the \
                     proposed edit"
                        .into(),
                )
            })?;

        let diff = definition_diff(base, candidate);
        let changed = diff != "(no changes)";
        Ok(ExecuteResult {
            output: json!({ "diff": diff, "changed": changed }),
            evidence: vec![],
            child_workflow_id: None,
            next_transition: None,
            suspend: None,
            telemetry: None,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::Utc;
    use praxec_core::model::WorkflowInstance;

    fn req(args: Value, context: Value) -> ExecuteRequest {
        ExecuteRequest {
            workflow: WorkflowInstance {
                id: "wf".into(),
                definition_id: "stub".into(),
                definition_version: "0".into(),
                definition: Value::Null,
                state: "s".into(),
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
            },
            transition: None,
            arguments: args,
            executor_config: Value::Null,
            idempotency_key: None,
            correlation_id: None,
        }
    }

    #[tokio::test]
    async fn diffs_explicit_arguments() {
        let out = DiffExecutor
            .execute(req(
                json!({
                    "base_definition": { "initialState": "draft" },
                    "candidate_definition": { "initialState": "ready" },
                }),
                json!({}),
            ))
            .await
            .unwrap();
        assert_eq!(out.output["changed"], json!(true));
        let diff = out.output["diff"].as_str().unwrap();
        assert!(diff.contains("- initialState: draft"));
        assert!(diff.contains("+ initialState: ready"));
    }

    #[tokio::test]
    async fn falls_back_to_blackboard_and_reports_no_change() {
        let same = json!({ "initialState": "s" });
        let out = DiffExecutor
            .execute(req(
                json!({}),
                json!({ "base_definition": same, "candidate_definition": same }),
            ))
            .await
            .unwrap();
        assert_eq!(out.output["changed"], json!(false));
        assert_eq!(out.output["diff"].as_str(), Some("(no changes)"));
    }

    #[tokio::test]
    async fn missing_inputs_fail_fast() {
        let err = DiffExecutor
            .execute(req(json!({ "base_definition": {} }), json!({})))
            .await
            .unwrap_err();
        assert!(format!("{err:?}").contains("candidate_definition"));
    }
}
