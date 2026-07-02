//! SPEC §33 D3 — concrete [`TransitionResolver`] backed by the live
//! [`WorkflowRuntime`]. Wraps `runtime_links::links` +
//! `WorkflowRuntime::filter_links_by_guards` so the in-runtime `kind: llm`
//! executor sees exactly the same per-state, guard-filtered link list
//! the HATEOAS response would surface to a caller.
//!
//! Construction:
//!
//! ```ignore
//! use std::sync::Arc;
//! use praxec_core::runtime_transition_resolver::RuntimeTransitionResolver;
//!
//! let resolver = RuntimeTransitionResolver::new(runtime.clone());
//! let arc: Arc<dyn praxec_core::ports::TransitionResolver> = Arc::new(resolver);
//! ```
//!
//! Cheap to clone — `WorkflowRuntime` is `Clone` (all internal handles
//! are `Arc`), so the resolver is just a thin wrapper.

use async_trait::async_trait;
use serde_json::Value;

use crate::model::{Principal, WorkflowInstance};
use crate::ports::TransitionResolver;
use crate::runtime::runtime_links::{link_filter_byguards, links};
use crate::runtime::WorkflowRuntime;

/// SPEC §33 D3 — runtime-backed transition resolver for the in-runtime
/// LLM executor. Returns the same guard-filtered link list that
/// `runtime_response::response().links` would emit, so the executor's
/// per-turn tool list mirrors what the HATEOAS layer would surface.
#[derive(Clone)]
pub struct RuntimeTransitionResolver {
    runtime: WorkflowRuntime,
}

impl RuntimeTransitionResolver {
    /// Build a resolver over the given runtime. The resolver holds a
    /// clone (all inner state is `Arc`-shared), so multiple resolvers
    /// can coexist cheaply.
    pub fn new(runtime: WorkflowRuntime) -> Self {
        Self { runtime }
    }
}

#[async_trait]
impl TransitionResolver for RuntimeTransitionResolver {
    async fn available_transitions(
        &self,
        instance: &WorkflowInstance,
        principal: &Principal,
    ) -> anyhow::Result<Vec<Value>> {
        let definition = &instance.definition;
        let mut out = links(definition, instance);
        // SPEC §33 D2 / D3 — when the workflow or current state declares
        // `linkFilter: byGuards`, apply the same per-link guard pass that
        // `runtime_response::response()` would. The executor sees a
        // narrowed tool list that matches what the caller would have
        // been offered — no transition the model can't actually take.
        if link_filter_byguards(definition, &instance.state) {
            out = self
                .runtime
                .filter_links_by_guards(out, definition, instance, principal)
                .await;
        }
        Ok(out)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::audit::{AuditSink, MemoryAuditSink};
    use crate::guards::DefaultGuardEvaluator;
    use crate::model::StartWorkflow;
    use crate::store::{ConfigDefinitionStore, InMemoryWorkflowStore};
    use serde_json::json;
    use std::sync::Arc;

    struct EmptyRegistry;
    impl crate::ports::ExecutorRegistry for EmptyRegistry {
        fn get(&self, _kind: &str) -> Option<Arc<dyn crate::ports::Executor>> {
            None
        }
    }

    /// Smoke test: the resolver returns the same `rel`s the HATEOAS
    /// response would surface for the workflow's current state. Two
    /// transitions declared, both expected.
    #[tokio::test]
    async fn resolver_returns_state_transitions() {
        let cfg = json!({
            "version": "1.0.0",
            "workflows": {
                "p": {
                    "version": "1.0.0",
                    "initialState": "a",
                    "states": {
                        "a": {
                            "transitions": {
                                "alpha": { "target": "b", "actor": "agent" },
                                "beta":  { "target": "c", "actor": "agent" }
                            }
                        },
                        "b": { "terminal": true },
                        "c": { "terminal": true }
                    }
                }
            }
        });
        let definitions = Arc::new(ConfigDefinitionStore::from_config(&cfg));
        let store = Arc::new(InMemoryWorkflowStore::new());
        let executors = Arc::new(EmptyRegistry);
        let guards = Arc::new(DefaultGuardEvaluator::new());
        let audit = Arc::new(MemoryAuditSink::new());
        let runtime = WorkflowRuntime::new(
            definitions,
            store,
            executors,
            guards,
            audit as Arc<dyn AuditSink>,
        );
        let start_resp = runtime
            .start(StartWorkflow {
                definition_id: "p".into(),
                input: json!({}),
                principal: Principal::anonymous(),
                trace_id: None,
                run_id: None,
                depth: 0,
                parent: None,
            })
            .await
            .unwrap();
        let wf_id = start_resp["workflow"]["id"].as_str().unwrap();
        let instance = runtime.load_instance(wf_id).await.unwrap();
        let resolver = RuntimeTransitionResolver::new(runtime.clone());
        let avail = resolver
            .available_transitions(&instance, &Principal::anonymous())
            .await
            .unwrap();
        let mut rels: Vec<&str> = avail
            .iter()
            .filter_map(|l| l.get("rel").and_then(Value::as_str))
            .collect();
        rels.sort();
        assert_eq!(rels, vec!["alpha", "beta"]);
    }
}
