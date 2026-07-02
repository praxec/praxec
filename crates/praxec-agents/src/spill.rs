use async_trait::async_trait;
use std::collections::hash_map::DefaultHasher;
use std::collections::HashMap;
use std::hash::{Hash, Hasher};
use std::sync::Mutex;

#[async_trait]
pub trait SpillStore: Send + Sync {
    /// Store `payload` and return a content-addressed slot id.
    async fn put(&self, payload: String) -> String;
    /// Read a byte-range window `[start, end)` from `slot`, char-boundary
    /// clamped. Returns `Err` when the slot is unknown.
    async fn get(&self, slot: &str, start: usize, end: usize) -> Result<String, String>;
}

/// In-memory, session-scoped, content-addressed spill store.
pub struct InMemorySpillStore {
    slots: Mutex<HashMap<String, String>>,
}

impl InMemorySpillStore {
    pub fn new() -> Self {
        Self {
            slots: Mutex::new(HashMap::new()),
        }
    }
}

impl Default for InMemorySpillStore {
    fn default() -> Self {
        Self::new()
    }
}

/// Short alias for the in-memory impl, used by tests.
pub type MemSpillStore = InMemorySpillStore;

#[async_trait]
impl SpillStore for InMemorySpillStore {
    async fn put(&self, payload: String) -> String {
        let mut hasher = DefaultHasher::new();
        payload.hash(&mut hasher);
        let slot = format!("{:x}", hasher.finish());
        self.slots.lock().unwrap().insert(slot.clone(), payload);
        slot
    }

    async fn get(&self, slot: &str, start: usize, end: usize) -> Result<String, String> {
        let guard = self.slots.lock().unwrap();
        let payload = guard
            .get(slot)
            .ok_or_else(|| format!("unknown slot: {slot}"))?;
        let len = payload.len();
        if start >= len {
            return Ok(String::new());
        }
        let mut s = start;
        while s > 0 && !payload.is_char_boundary(s) {
            s -= 1;
        }
        let mut e = end.min(len);
        while e > s && !payload.is_char_boundary(e) {
            e -= 1;
        }
        Ok(payload[s..e].to_string())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn get_unknown_slot_returns_error() {
        let store = InMemorySpillStore::new();
        let result = store.get("unknown", 0, 100).await;
        assert!(result.is_err());
    }

    #[tokio::test]
    async fn round_trip_exact_window() {
        let store = InMemorySpillStore::new();
        let payload = "hello world";
        let slot = store.put(payload.to_string()).await;
        let result = store.get(&slot, 0, 5).await;
        assert_eq!(result.unwrap(), "hello");
    }

    #[tokio::test]
    async fn range_past_end_clamps() {
        let store = InMemorySpillStore::new();
        let payload = "hello";
        let slot = store.put(payload.to_string()).await;
        // end beyond length → returns up to len, char-boundary safe
        let result = store.get(&slot, 0, 100).await;
        assert_eq!(result.unwrap(), "hello");
    }

    // Each agent run owns a distinct store (a local in `run()`), so concurrent
    // agents/workflows in one process never share spill state: a slot minted in
    // one run's store is not resolvable in another's, even though both stores
    // address content identically. This pins that per-run isolation.
    #[tokio::test]
    async fn a_slot_from_one_store_is_unknown_in_another() {
        let run_a = InMemorySpillStore::new();
        let run_b = InMemorySpillStore::new();
        let slot = run_a.put("payload-from-run-A".to_string()).await;
        let leaked = run_b.get(&slot, 0, 100).await;
        assert!(
            leaked.is_err(),
            "run B must not see run A's slot — stores are isolated namespaces"
        );
    }

    // Within ONE store (one run's turns), the Mutex<HashMap> must stay coherent
    // under concurrent access: every payload stored from a different task is
    // retrievable intact, with no lost writes or interleaving corruption. Proves
    // SpillStore: Send + Sync is honored, not just declared.
    #[tokio::test]
    async fn concurrent_puts_into_one_store_all_round_trip() {
        use std::sync::Arc;
        let store = Arc::new(InMemorySpillStore::new());
        let mut tasks = Vec::new();
        for i in 0..64 {
            let s = store.clone();
            tasks.push(tokio::spawn(async move {
                // Each payload is distinct (value + length) → distinct slot.
                let payload = format!("payload-{i}-{}", "x".repeat(i));
                let slot = s.put(payload.clone()).await;
                (slot, payload)
            }));
        }
        let mut stored = Vec::new();
        for t in tasks {
            stored.push(t.await.unwrap());
        }
        let mut intact = 0;
        for (slot, payload) in stored {
            let got = store.get(&slot, 0, payload.len()).await.unwrap();
            if got == payload {
                intact += 1;
            }
        }
        assert_eq!(intact, 64, "every concurrently-stored payload must survive");
    }
}
