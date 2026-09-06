//! Opt-in auto-drive continuation: compare only the author's declared read slice.
//! This is structural evidence change, not a judgment of semantic progress.
use crate::mapping::read_in_scopes;
use crate::model::WorkflowInstance;
use anyhow::{Result, bail};
use serde_json::{Map, Value, json};

pub(super) const KEY: &str = "_agent_continuation_reads";

pub(crate) fn validate(config: &Value) -> Result<Vec<&str>> {
    let Some(config) = config.as_object() else {
        bail!("CONTINUATION_CONFIG_INVALID: continuation must be an object");
    };
    if config.len() != 1 || !config.contains_key("reads") {
        bail!("CONTINUATION_CONFIG_INVALID: continuation accepts only reads");
    }
    let Some(reads) = config["reads"].as_array().filter(|r| !r.is_empty()) else {
        bail!("CONTINUATION_CONFIG_INVALID: reads must be a nonempty array");
    };
    let mut paths = Vec::new();
    for path in reads {
        let Some(path) = path.as_str() else {
            bail!("CONTINUATION_CONFIG_INVALID: each read must be a path string");
        };
        let tail = path
            .strip_prefix("$.context.")
            .or_else(|| path.strip_prefix("$.workflow.input."));
        if tail.is_none_or(|p| p.is_empty() || p.starts_with('_')) || path.contains('*') {
            bail!(
                "CONTINUATION_CONFIG_INVALID: read '{path}' must select a concrete public context or workflow input path"
            );
        }
        if paths.contains(&path) {
            bail!("CONTINUATION_CONFIG_INVALID: duplicate read '{path}'");
        }
        paths.push(path);
    }
    Ok(paths)
}

pub(super) fn capture(definition: &Value, instance: &WorkflowInstance) -> Result<Option<Value>> {
    let Some(config) = definition
        .get("states")
        .and_then(|s| s.get(&instance.state))
        .and_then(|s| s.get("continuation"))
    else {
        return Ok(None);
    };
    let reads = validate(config)?;
    let mut snapshot = Map::new();
    for path in reads {
        let Some(value) = read_in_scopes(
            path,
            &json!({}),
            &instance.context,
            &instance.input,
            None,
            Some(&instance.run_env),
        ) else {
            bail!(
                "CONTINUATION_INPUT_UNRESOLVED: state '{}' read '{path}' is missing",
                instance.state
            );
        };
        snapshot.insert(path.to_string(), value);
    }
    Ok(Some(Value::Object(snapshot)))
}

pub(super) fn unchanged(instance: &WorkflowInstance, snapshot: &Value) -> Result<bool> {
    match instance.context.get(KEY) {
        None => Ok(false),
        Some(Value::Object(states)) => Ok(states.get(&instance.state) == Some(snapshot)),
        Some(_) => {
            bail!("CONTINUATION_METADATA_INVALID: engine continuation metadata must be an object")
        }
    }
}

/// Restore the engine's pre-hop ledger after arbitrary output merges. Only a
/// successfully committed auto-drive hop updates its state's previous inputs.
pub(super) fn commit(
    context: &mut Value,
    previous: Option<Value>,
    state: &str,
    snapshot: Option<Value>,
) -> Result<()> {
    if previous.is_none() && snapshot.is_none() {
        if let Some(obj) = context.as_object_mut() {
            obj.remove(KEY);
        }
        return Ok(());
    }
    let mut ledger = match previous {
        None => Map::new(),
        Some(Value::Object(states)) => states,
        Some(_) => {
            bail!("CONTINUATION_METADATA_INVALID: engine continuation metadata must be an object")
        }
    };
    if let Some(snapshot) = snapshot {
        ledger.insert(state.to_string(), snapshot);
    }
    let Some(obj) = context.as_object_mut() else {
        bail!("CONTINUATION_METADATA_INVALID: context must be an object");
    };
    obj.insert(KEY.to_string(), Value::Object(ledger));
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::audit::MemoryAuditSink;
    use crate::error::ExecutorError;
    use crate::guards::DefaultGuardEvaluator;
    use crate::model::{ExecuteRequest, ExecuteResult, Principal};
    use crate::ports::{Executor, ExecutorRegistry, WorkflowStore};
    use crate::runtime::{ChainOutcome, WorkflowRuntime};
    use crate::store::{ConfigDefinitionStore, InMemoryWorkflowStore};
    use async_trait::async_trait;
    use std::sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    };

    struct Worker {
        calls: AtomicUsize,
        fail_first: bool,
    }
    #[async_trait]
    impl Executor for Worker {
        async fn execute(&self, _: ExecuteRequest) -> Result<ExecuteResult, ExecutorError> {
            let call = self.calls.fetch_add(1, Ordering::SeqCst);
            if self.fail_first && call == 0 {
                return Err(ExecutorError::Permanent("provider unavailable".into()));
            }
            Ok(ExecuteResult {
                output: json!({}),
                ..Default::default()
            })
        }
    }
    struct Registry(Arc<Worker>);
    impl ExecutorRegistry for Registry {
        fn get(&self, kind: &str) -> Option<Arc<dyn Executor>> {
            (kind == "agent").then(|| self.0.clone() as Arc<dyn Executor>)
        }
    }
    fn runtime(store: Arc<InMemoryWorkflowStore>, worker: Arc<Worker>) -> WorkflowRuntime {
        WorkflowRuntime::new(
            Arc::new(ConfigDefinitionStore::from_config(&json!({}))),
            store,
            Arc::new(Registry(worker)),
            Arc::new(DefaultGuardEvaluator::new()),
            Arc::new(MemoryAuditSink::new()),
        )
        .with_auto_drive_agents(true, "coding", vec![], 10)
    }
    fn definition() -> Value {
        json!({"states":{"s":{"goal":"Improve the artifact", "continuation":{"reads":["$.context.digest"]}, "transitions":{"work":{"actor":"agent", "target":"s"}}}}})
    }
    async fn drive(
        rt: &WorkflowRuntime,
        def: &Value,
        instance: WorkflowInstance,
    ) -> Result<ChainOutcome> {
        rt.run_deterministic_chain(
            def,
            instance,
            &Principal::anonymous(),
            "continuation-test",
            1,
            20,
        )
        .await
    }

    #[tokio::test]
    async fn restart_and_counter_changes_cannot_repeat_unchanged_model_work() {
        let store = Arc::new(InMemoryWorkflowStore::new());
        let worker = Arc::new(Worker {
            calls: AtomicUsize::new(0),
            fail_first: false,
        });
        let def = definition();
        let mut initial = WorkflowInstance::for_test_with_context(json!({"digest":null}));
        initial.definition = def.clone();
        let initial = store.create(initial).await.unwrap();
        assert!(matches!(
            drive(&runtime(store.clone(), worker.clone()), &def, initial)
                .await
                .unwrap(),
            ChainOutcome::Completed(_)
        ));
        assert_eq!(worker.calls.load(Ordering::SeqCst), 1);
        let mut saved = store.load("wf").await.unwrap();
        assert_eq!(saved.context[KEY]["s"]["$.context.digest"], Value::Null);
        // Recreate runtime and deserialize persisted snapshot: no in-memory gate.
        saved = serde_json::from_value(serde_json::to_value(saved).unwrap()).unwrap();
        saved.context["iteration"] = json!(999);
        saved.context["_chain_hops_total"] = json!(5);
        assert!(
            matches!(drive(&runtime(store.clone(), worker.clone()), &def, saved.clone()).await.unwrap(), ChainOutcome::Quarantined { reason, .. } if reason.contains("continuation_unchanged"))
        );
        assert_eq!(worker.calls.load(Ordering::SeqCst), 1);
        saved.context["digest"] = json!("changed artifact");
        assert!(matches!(
            drive(&runtime(store.clone(), worker.clone()), &def, saved)
                .await
                .unwrap(),
            ChainOutcome::Completed(_)
        ));
        assert_eq!(worker.calls.load(Ordering::SeqCst), 2);
    }

    #[tokio::test]
    async fn missing_read_refuses_before_model_and_failed_call_does_not_consume_slice() {
        let store = Arc::new(InMemoryWorkflowStore::new());
        let worker = Arc::new(Worker {
            calls: AtomicUsize::new(0),
            fail_first: true,
        });
        let def = definition();
        let mut initial = WorkflowInstance::for_test_with_context(json!({}));
        initial.definition = def.clone();
        let mut initial = store.create(initial).await.unwrap();
        let rt = runtime(store.clone(), worker.clone());
        let err = drive(&rt, &def, initial.clone()).await.err().unwrap();
        assert!(err.to_string().contains("CONTINUATION_INPUT_UNRESOLVED"));
        assert_eq!(worker.calls.load(Ordering::SeqCst), 0);
        initial.context["digest"] = json!("d1");
        assert!(matches!(
            drive(&rt, &def, initial.clone()).await.unwrap(),
            ChainOutcome::Failed { .. }
        ));
        assert!(store.load("wf").await.unwrap().context.get(KEY).is_none());
        assert!(matches!(
            drive(&rt, &def, initial).await.unwrap(),
            ChainOutcome::Completed(_)
        ));
        assert_eq!(worker.calls.load(Ordering::SeqCst), 2);
    }

    #[test]
    fn invalid_policies_and_forged_output_do_not_reset_the_gate() {
        for config in [
            json!({"reads":[]}),
            json!({"reads":["$.context"]}),
            json!({"reads":["$.context._chain_hops_total"]}),
            json!({"reads":["$.context.x","$.context.x"]}),
            json!({"reads":["$.context.x"],"retry":true}),
            json!({"reads":["$.output.x"]}),
        ] {
            assert!(validate(&config).is_err(), "{config}");
        }
        let mut context = json!({"_agent_continuation_reads":{"s":"forged"}});
        let previous = json!({"s":{"$.context.digest":"previous"}});
        commit(&mut context, Some(previous.clone()), "other", None).unwrap();
        assert_eq!(context[KEY], previous);
        let mut instance =
            WorkflowInstance::for_test_with_context(json!({"_agent_continuation_reads":null}));
        assert!(unchanged(&instance, &json!({})).is_err());
        instance.context = json!({"digest":"d"});
        assert!(
            capture(&json!({"states":{"s":{}}}), &instance)
                .unwrap()
                .is_none()
        );
    }
}
