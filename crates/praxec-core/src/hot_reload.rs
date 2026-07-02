use std::sync::{Arc, RwLock};

use async_trait::async_trait;
use serde_json::Value;

use crate::discovery::{DiscoveryIndex, DiscoveryItem, DiscoveryKind, SearchHit, SearchRequest};
use crate::ports::{DefinitionStore, Executor, ExecutorRegistry};

pub struct SwappableDefinitionStore {
    inner: RwLock<Arc<dyn DefinitionStore>>,
}

impl SwappableDefinitionStore {
    pub fn new(initial: Arc<dyn DefinitionStore>) -> Self {
        Self {
            inner: RwLock::new(initial),
        }
    }

    pub fn swap(&self, new: Arc<dyn DefinitionStore>) {
        *self
            .inner
            .write()
            .expect("LOCK_POISONED: swappable definition store") = new;
    }
}

#[async_trait]
impl DefinitionStore for SwappableDefinitionStore {
    async fn load(&self, definition_id: &str) -> anyhow::Result<Value> {
        let store = self
            .inner
            .read()
            .expect("LOCK_POISONED: swappable definition store")
            .clone();
        store.load(definition_id).await
    }
}

pub struct SwappableExecutorRegistry {
    inner: RwLock<Arc<dyn ExecutorRegistry>>,
}

impl SwappableExecutorRegistry {
    pub fn new(initial: Arc<dyn ExecutorRegistry>) -> Self {
        Self {
            inner: RwLock::new(initial),
        }
    }

    pub fn swap(&self, new: Arc<dyn ExecutorRegistry>) {
        *self
            .inner
            .write()
            .expect("LOCK_POISONED: swappable executor registry") = new;
    }

    /// SPEC §33 D4 — snapshot the currently-held registry. The binary
    /// uses this to overlay the `kind: llm` executor onto the
    /// runtime-built base registry: capture the original, wrap it,
    /// swap the wrapper in. Holding the returned Arc separately means
    /// the overlay's delegation target is the original (not the
    /// swappable), so there's no get→swap→get cycle.
    pub fn current(&self) -> Arc<dyn ExecutorRegistry> {
        self.inner
            .read()
            .expect("LOCK_POISONED: swappable executor registry")
            .clone()
    }
}

impl ExecutorRegistry for SwappableExecutorRegistry {
    fn get(&self, kind: &str) -> Option<Arc<dyn Executor>> {
        let registry = self
            .inner
            .read()
            .expect("LOCK_POISONED: swappable executor registry")
            .clone();
        registry.get(kind)
    }
}

pub struct SwappableDiscoveryIndex {
    inner: RwLock<Arc<dyn DiscoveryIndex>>,
}

impl SwappableDiscoveryIndex {
    pub fn new(initial: Arc<dyn DiscoveryIndex>) -> Self {
        Self {
            inner: RwLock::new(initial),
        }
    }

    pub fn swap(&self, new: Arc<dyn DiscoveryIndex>) {
        *self
            .inner
            .write()
            .expect("LOCK_POISONED: swappable discovery index") = new;
    }
}

#[async_trait]
impl DiscoveryIndex for SwappableDiscoveryIndex {
    async fn search(&self, request: SearchRequest) -> anyhow::Result<Vec<SearchHit>> {
        let index = self
            .inner
            .read()
            .expect("LOCK_POISONED: swappable discovery index")
            .clone();
        index.search(request).await
    }

    async fn describe(&self, id: &str) -> anyhow::Result<Option<DiscoveryItem>> {
        let index = self
            .inner
            .read()
            .expect("LOCK_POISONED: swappable discovery index")
            .clone();
        index.describe(id).await
    }

    async fn list(&self, kind: Option<DiscoveryKind>) -> anyhow::Result<Vec<DiscoveryItem>> {
        let index = self
            .inner
            .read()
            .expect("LOCK_POISONED: swappable discovery index")
            .clone();
        index.list(kind).await
    }

    async fn home(&self) -> anyhow::Result<Value> {
        let index = self
            .inner
            .read()
            .expect("LOCK_POISONED: swappable discovery index")
            .clone();
        index.home().await
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::store::ConfigDefinitionStore;
    use serde_json::json;
    use std::collections::HashMap;

    #[tokio::test]
    async fn swap_definition_store() {
        let store_a = Arc::new(ConfigDefinitionStore::new(HashMap::from([(
            "wf_a".into(),
            json!({"initialState": "s", "states": {"s": {}}}),
        )])));
        let swappable = Arc::new(SwappableDefinitionStore::new(store_a));

        assert!(swappable.load("wf_a").await.is_ok());
        assert!(swappable.load("wf_b").await.is_err());

        let store_b = Arc::new(ConfigDefinitionStore::new(HashMap::from([(
            "wf_b".into(),
            json!({"initialState": "s", "states": {"s": {}}}),
        )])));
        swappable.swap(store_b);

        assert!(swappable.load("wf_a").await.is_err());
        assert!(swappable.load("wf_b").await.is_ok());
    }
}
