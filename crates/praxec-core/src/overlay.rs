//! A reusable single-kind executor overlay.
//!
//! The gateway wires executors that need live, post-startup context (the
//! `kind: llm` executor needs a `RuntimeTransitionResolver` built from the
//! running `WorkflowRuntime`; the `kind: agent` executor needs a model
//! resolver) onto the base registry *after* it's built. Each does so by
//! capturing the current registry, wrapping it so one `kind` resolves to the
//! new executor and everything else delegates to the captured inner, and
//! swapping the wrapper back into the [`SwappableExecutorRegistry`].
//!
//! This type is that wrapper — one struct instead of a hand-rolled
//! `XAwareRegistry` per kind. Stacking is by composition: wrap an
//! already-wrapped registry to add another kind (the captured `inner` is the
//! pre-wrap registry, so there is no cycle).

use std::sync::Arc;

use crate::ports::{Executor, ExecutorRegistry};

/// Overlays a single executor `kind` onto an inner registry; all other kinds
/// delegate to `inner`.
pub struct SingleKindOverlay {
    inner: Arc<dyn ExecutorRegistry>,
    kind: &'static str,
    executor: Arc<dyn Executor>,
}

impl SingleKindOverlay {
    pub fn new(
        inner: Arc<dyn ExecutorRegistry>,
        kind: &'static str,
        executor: Arc<dyn Executor>,
    ) -> Self {
        Self {
            inner,
            kind,
            executor,
        }
    }
}

impl ExecutorRegistry for SingleKindOverlay {
    fn get(&self, kind: &str) -> Option<Arc<dyn Executor>> {
        if kind == self.kind {
            Some(self.executor.clone())
        } else {
            self.inner.get(kind)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::error::ExecutorError;
    use crate::model::{ExecuteRequest, ExecuteResult};
    use async_trait::async_trait;

    /// Identity is established by `Arc::ptr_eq` against the inserted handle —
    /// the executor never needs to run.
    struct Stub;

    #[async_trait]
    impl Executor for Stub {
        async fn execute(&self, _request: ExecuteRequest) -> Result<ExecuteResult, ExecutorError> {
            Err(ExecutorError::Permanent("unused".into()))
        }
    }

    /// Minimal base registry holding one kind.
    struct OneKind(&'static str, Arc<dyn Executor>);
    impl ExecutorRegistry for OneKind {
        fn get(&self, kind: &str) -> Option<Arc<dyn Executor>> {
            if kind == self.0 {
                Some(self.1.clone())
            } else {
                None
            }
        }
    }

    #[test]
    fn overlay_kind_resolves_to_overlay_executor() {
        let base_cli: Arc<dyn Executor> = Arc::new(Stub);
        let llm: Arc<dyn Executor> = Arc::new(Stub);
        let base: Arc<dyn ExecutorRegistry> = Arc::new(OneKind("cli", base_cli));
        let overlay = SingleKindOverlay::new(base, "llm", llm.clone());
        assert!(Arc::ptr_eq(&overlay.get("llm").unwrap(), &llm));
    }

    #[test]
    fn other_kinds_delegate_to_inner() {
        let base_cli: Arc<dyn Executor> = Arc::new(Stub);
        let llm: Arc<dyn Executor> = Arc::new(Stub);
        let base: Arc<dyn ExecutorRegistry> = Arc::new(OneKind("cli", base_cli.clone()));
        let overlay = SingleKindOverlay::new(base, "llm", llm);
        assert!(Arc::ptr_eq(&overlay.get("cli").unwrap(), &base_cli));
        assert!(overlay.get("nope").is_none());
    }

    #[test]
    fn overlays_stack_by_composition() {
        let base_cli: Arc<dyn Executor> = Arc::new(Stub);
        let llm_exec: Arc<dyn Executor> = Arc::new(Stub);
        let agent_exec: Arc<dyn Executor> = Arc::new(Stub);
        let base: Arc<dyn ExecutorRegistry> = Arc::new(OneKind("cli", base_cli.clone()));
        let llm: Arc<dyn ExecutorRegistry> =
            Arc::new(SingleKindOverlay::new(base, "llm", llm_exec.clone()));
        let agent = SingleKindOverlay::new(llm, "agent", agent_exec.clone());
        assert!(Arc::ptr_eq(&agent.get("agent").unwrap(), &agent_exec));
        assert!(Arc::ptr_eq(&agent.get("llm").unwrap(), &llm_exec));
        assert!(Arc::ptr_eq(&agent.get("cli").unwrap(), &base_cli));
    }
}
