use std::collections::HashMap;
use std::sync::Arc;

use praxec_core::ports::{Executor, ExecutorRegistry};

#[derive(Default, Clone)]
pub struct HashMapExecutorRegistry {
    executors: HashMap<String, Arc<dyn Executor>>,
}

impl HashMapExecutorRegistry {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn with(mut self, kind: impl Into<String>, executor: Arc<dyn Executor>) -> Self {
        self.executors.insert(kind.into(), executor);
        self
    }

    pub fn insert(&mut self, kind: impl Into<String>, executor: Arc<dyn Executor>) {
        self.executors.insert(kind.into(), executor);
    }
}

impl ExecutorRegistry for HashMapExecutorRegistry {
    fn get(&self, kind: &str) -> Option<Arc<dyn Executor>> {
        self.executors.get(kind).cloned()
    }
}
