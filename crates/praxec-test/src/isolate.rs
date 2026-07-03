//! Seed a WorkflowInstance at a given state+context and submit one transition.

use std::sync::Arc;

#[cfg(test)]
use praxec_core::audit::AuditSink;
use praxec_core::model::{Principal, SubmitTransition, WorkflowInstance};
use praxec_core::ports::{GuidanceAcknowledgmentStore, WorkflowStore};
use praxec_core::runtime::WorkflowRuntime;
use praxec_core::store::InMemoryWorkflowStore;
use serde_json::Value;

/// Pre-seeded guidance acknowledgments: the store plus `(guidance_id, hash)`
/// pairs to stamp before the transition runs.
type PreAcks = Option<(Arc<dyn GuidanceAcknowledgmentStore>, Vec<(String, String)>)>;

#[derive(Debug, PartialEq)]
pub enum SubmitResult {
    Fired { to_state: String, version: u64 },
    Rejected { code: String },
    Errored { code: String },
}

/// Seed `definition_id` at `state` with `context`, submit `transition`.
/// `snapshot` is the resolved definition (carried by the instance).
/// `arguments` is passed directly as the `SubmitTransition::arguments` field.
#[allow(clippy::too_many_arguments)]
pub async fn submit_isolated(
    runtime: &WorkflowRuntime,
    store: &Arc<InMemoryWorkflowStore>,
    snapshot: &Value,
    definition_id: &str,
    state: &str,
    context: Value,
    input: Value,
    transition: &str,
    principal: Principal,
    arguments: Value,
) -> anyhow::Result<SubmitResult> {
    submit_isolated_inner(
        runtime,
        store,
        snapshot,
        definition_id,
        state,
        context,
        input,
        transition,
        principal,
        arguments,
        None,
    )
    .await
}

/// Like [`submit_isolated`] but also pre-records guidance acknowledgments so
/// `guidance_acknowledged` guards pass. Each `(subject, body_hash)` pair is
/// recorded against the generated instance ID before the runtime sees it.
#[allow(clippy::too_many_arguments)]
pub async fn submit_isolated_with_acks(
    runtime: &WorkflowRuntime,
    store: &Arc<InMemoryWorkflowStore>,
    snapshot: &Value,
    definition_id: &str,
    state: &str,
    context: Value,
    input: Value,
    transition: &str,
    principal: Principal,
    arguments: Value,
    ack_store: Arc<dyn GuidanceAcknowledgmentStore>,
    ack_subjects: Vec<(String, String)>,
) -> anyhow::Result<SubmitResult> {
    submit_isolated_inner(
        runtime,
        store,
        snapshot,
        definition_id,
        state,
        context,
        input,
        transition,
        principal,
        arguments,
        Some((ack_store, ack_subjects)),
    )
    .await
}

#[allow(clippy::too_many_arguments)]
async fn submit_isolated_inner(
    runtime: &WorkflowRuntime,
    store: &Arc<InMemoryWorkflowStore>,
    snapshot: &Value,
    definition_id: &str,
    state: &str,
    context: Value,
    input: Value,
    transition: &str,
    principal: Principal,
    arguments: Value,
    pre_acks: PreAcks,
) -> anyhow::Result<SubmitResult> {
    let id = format!("wf_iso_{}", uuid::Uuid::new_v4().simple());
    let version = snapshot
        .get("version")
        .and_then(Value::as_str)
        .unwrap_or("0")
        .to_string();
    let instance = WorkflowInstance {
        id: id.clone(),
        definition_id: definition_id.to_string(),
        definition_version: version,
        definition: snapshot.clone(),
        state: state.to_string(),
        version: 0,
        input,
        context,
        started_at: chrono::Utc::now(),
        trace_id: None,
        run_id: None,
        depth: 0,
        cancelled_at: None,
        cancelled_reason: None,
        parent: None,
    };
    store.create(instance).await?;

    // Pre-record any guidance acknowledgments AFTER we know the instance ID.
    if let Some((ack_store, subjects)) = pre_acks {
        for (subject, hash) in subjects {
            ack_store.record(&id, &subject, &hash).await?;
        }
    }

    let resp = runtime
        .submit(SubmitTransition {
            workflow_id: id,
            expected_version: 0,
            transition: transition.to_string(),
            arguments,
            principal,
            ..Default::default()
        })
        .await?;
    Ok(classify(&resp))
}

fn classify(resp: &Value) -> SubmitResult {
    if let Some(code) = resp.pointer("/error/code").and_then(Value::as_str) {
        return if code == "GUARD_REJECTED" || code == "ACTOR_MISMATCH" {
            SubmitResult::Rejected {
                code: code.to_string(),
            }
        } else {
            SubmitResult::Errored {
                code: code.to_string(),
            }
        };
    }
    SubmitResult::Fired {
        to_state: resp
            .pointer("/workflow/state")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_string(),
        version: resp
            .pointer("/workflow/version")
            .and_then(Value::as_u64)
            .unwrap_or(0),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use praxec_core::WorkflowRuntime;
    use praxec_core::audit::MemoryAuditSink;
    use praxec_core::guards::DefaultGuardEvaluator;
    use praxec_core::ports::{DefinitionStore, ExecutorRegistry};
    use praxec_core::store::ConfigDefinitionStore;
    use serde_json::json;

    async fn build() -> (WorkflowRuntime, Arc<InMemoryWorkflowStore>, Value) {
        let cfg = json!({ "version": "1.0.0", "workflows": { "wf": {
            "version": "1.0.0", "initialState": "draft",
            "states": {
                "draft": { "transitions": { "submit": {
                    "target": "review", "actor": "agent",
                    "guards": [ { "kind": "expr", "expr": "$.context.ready == true" } ],
                    "executor": { "kind": "noop" } } } },
                "review": { "terminal": true, "transitions": {} }
            }
        }}});
        let resolved = praxec_core::config::resolve(cfg).expect("resolve");
        let definitions: Arc<dyn DefinitionStore> =
            Arc::new(ConfigDefinitionStore::from_config(&resolved));
        let snapshot = definitions.load("wf").await.expect("load");
        let store = Arc::new(InMemoryWorkflowStore::new());
        let executors: Arc<dyn ExecutorRegistry> = Arc::new(crate::mock::MockRegistry);
        let guards = Arc::new(DefaultGuardEvaluator::new());
        let audit = Arc::new(MemoryAuditSink::new());
        let runtime = WorkflowRuntime::new(
            definitions.clone(),
            store.clone(),
            executors,
            guards,
            audit as Arc<dyn AuditSink>,
        );
        (runtime, store, snapshot)
    }

    #[tokio::test]
    async fn fires_on_satisfying_context() {
        let (rt, store, snap) = build().await;
        let r = submit_isolated(
            &rt,
            &store,
            &snap,
            "wf",
            "draft",
            json!({"ready": true}),
            json!({}),
            "submit",
            Principal::anonymous(),
            json!({}),
        )
        .await
        .unwrap();
        assert_eq!(
            r,
            SubmitResult::Fired {
                to_state: "review".into(),
                version: 1
            }
        );
    }

    #[tokio::test]
    async fn rejects_on_violating_context() {
        let (rt, store, snap) = build().await;
        let r = submit_isolated(
            &rt,
            &store,
            &snap,
            "wf",
            "draft",
            json!({"ready": false}),
            json!({}),
            "submit",
            Principal::anonymous(),
            json!({}),
        )
        .await
        .unwrap();
        assert_eq!(
            r,
            SubmitResult::Rejected {
                code: "GUARD_REJECTED".into()
            }
        );
    }
}
