use std::collections::HashMap;
use std::sync::Mutex;

use open_sandbox_contracts::error::ProxyError;
use open_sandbox_contracts::types::{AgentId, SandboxId};

use crate::routing_store::RoutingStore;

pub struct RoutingCache<S: RoutingStore> {
    store: S,
    cache: Mutex<HashMap<String, AgentId>>,
}

impl<S: RoutingStore> RoutingCache<S> {
    pub fn new(store: S) -> Self {
        Self {
            store,
            cache: Mutex::new(HashMap::new()),
        }
    }

    pub fn lookup(&self, subdomain: &str) -> Option<AgentId> {
        self.cache.lock().unwrap().get(subdomain).cloned()
    }

    pub async fn refresh(&self) -> Result<(), ProxyError> {
        let entries = self.store.load_all().await?;
        let mut cache = self.cache.lock().unwrap();
        cache.clear();
        for entry in entries {
            cache.insert(entry.sandbox_id.subdomain(), entry.agent_id);
        }
        Ok(())
    }

    pub fn insert(&self, sandbox_id: SandboxId, agent_id: AgentId) {
        self.cache
            .lock()
            .unwrap()
            .insert(sandbox_id.subdomain(), agent_id);
    }

    pub fn remove_by_subdomain(&self, subdomain: &str) {
        self.cache.lock().unwrap().remove(subdomain);
    }

    pub fn len(&self) -> usize {
        self.cache.lock().unwrap().len()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testutil::*;

    #[tokio::test]
    async fn lookup_returns_none_for_unknown_sandbox() {
        let store = InMemoryRoutingStore::new();
        let cache = RoutingCache::new(store);
        assert!(cache.lookup(&SandboxId::new().subdomain()).is_none());
    }

    #[tokio::test]
    async fn insert_and_lookup() {
        let store = InMemoryRoutingStore::new();
        let cache = RoutingCache::new(store);
        let sandbox_id = SandboxId::new();
        let agent_id = AgentId::new();

        cache.insert(sandbox_id.clone(), agent_id.clone());
        assert_eq!(cache.lookup(&sandbox_id.subdomain()), Some(agent_id));
    }

    #[tokio::test]
    async fn remove_evicts_entry() {
        let store = InMemoryRoutingStore::new();
        let cache = RoutingCache::new(store);
        let sandbox_id = SandboxId::new();

        cache.insert(sandbox_id.clone(), AgentId::new());
        cache.remove_by_subdomain(&sandbox_id.subdomain());
        assert!(cache.lookup(&sandbox_id.subdomain()).is_none());
    }

    #[tokio::test]
    async fn refresh_loads_from_store() {
        let store = InMemoryRoutingStore::new();
        let sandbox_id = SandboxId::new();
        let agent_id = AgentId::new();
        store.add_entry(sandbox_id.clone(), agent_id.clone());

        let cache = RoutingCache::new(store);
        assert!(cache.lookup(&sandbox_id.subdomain()).is_none());

        cache.refresh().await.unwrap();
        assert_eq!(cache.lookup(&sandbox_id.subdomain()), Some(agent_id));
    }

    #[tokio::test]
    async fn refresh_replaces_stale_entries() {
        let store = InMemoryRoutingStore::new();
        let sandbox_id = SandboxId::new();
        let old_agent = AgentId::new();
        let new_agent = AgentId::new();

        store.add_entry(sandbox_id.clone(), old_agent.clone());
        let cache = RoutingCache::new(store.clone());
        cache.refresh().await.unwrap();
        assert_eq!(cache.lookup(&sandbox_id.subdomain()), Some(old_agent));

        store.clear();
        store.add_entry(sandbox_id.clone(), new_agent.clone());
        cache.refresh().await.unwrap();
        assert_eq!(cache.lookup(&sandbox_id.subdomain()), Some(new_agent));
    }

    #[tokio::test]
    async fn len_reflects_cache_size() {
        let store = InMemoryRoutingStore::new();
        let cache = RoutingCache::new(store);
        assert_eq!(cache.len(), 0);

        cache.insert(SandboxId::new(), AgentId::new());
        assert_eq!(cache.len(), 1);

        cache.insert(SandboxId::new(), AgentId::new());
        assert_eq!(cache.len(), 2);
    }
}
