use std::collections::HashMap;
use std::sync::Mutex;

use open_sandbox_contracts::error::ProxyError;
use open_sandbox_contracts::types::{AgentId, SandboxId};

use crate::routing_store::RoutingStore;

#[derive(Clone, Debug)]
pub struct CachedRoute {
    pub agent_id: AgentId,
    pub sandbox_id: SandboxId,
}

pub struct RoutingCache<S: RoutingStore> {
    store: S,
    cache: Mutex<HashMap<String, CachedRoute>>,
}

impl<S: RoutingStore> RoutingCache<S> {
    pub fn new(store: S) -> Self {
        Self {
            store,
            cache: Mutex::new(HashMap::new()),
        }
    }

    pub fn lookup(&self, subdomain: &str) -> Option<CachedRoute> {
        self.cache.lock().unwrap().get(subdomain).cloned()
    }

    /// Fast cache hit, with a single DB round-trip fallback on
    /// miss. Used on the data-plane hot path so a freshly-created
    /// sandbox is routable immediately, instead of waiting for the
    /// next periodic refresh tick. The cache is updated in-place
    /// so subsequent lookups hit the fast path. The store call
    /// uses `lookup_by_subdomain`, which means a 12-hex-char
    /// prefix is enough — callers don't need the full SandboxId.
    pub async fn lookup_or_fetch(
        &self,
        subdomain: &str,
    ) -> Result<Option<CachedRoute>, ProxyError> {
        if let Some(hit) = self.lookup(subdomain) {
            return Ok(Some(hit));
        }
        match self.store.lookup_by_subdomain(subdomain).await? {
            Some(entry) => {
                let route = CachedRoute {
                    agent_id: entry.agent_id.clone(),
                    sandbox_id: entry.sandbox_id.clone(),
                };
                self.cache.lock().unwrap().insert(
                    entry.sandbox_id.subdomain(),
                    CachedRoute {
                        agent_id: entry.agent_id,
                        sandbox_id: entry.sandbox_id,
                    },
                );
                Ok(Some(route))
            }
            None => Ok(None),
        }
    }

    pub async fn refresh(&self) -> Result<(), ProxyError> {
        let entries = self.store.load_all().await?;
        let mut cache = self.cache.lock().unwrap();
        cache.clear();
        for entry in entries {
            cache.insert(
                entry.sandbox_id.subdomain(),
                CachedRoute {
                    agent_id: entry.agent_id,
                    sandbox_id: entry.sandbox_id,
                },
            );
        }
        Ok(())
    }

    pub fn insert(&self, sandbox_id: SandboxId, agent_id: AgentId) {
        self.cache.lock().unwrap().insert(
            sandbox_id.subdomain(),
            CachedRoute {
                agent_id,
                sandbox_id,
            },
        );
    }

    pub fn remove_by_subdomain(&self, subdomain: &str) {
        self.cache.lock().unwrap().remove(subdomain);
    }

    pub fn len(&self) -> usize {
        self.cache.lock().unwrap().len()
    }

    pub fn is_empty(&self) -> bool {
        self.cache.lock().unwrap().is_empty()
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
        let route = cache.lookup(&sandbox_id.subdomain()).unwrap();
        assert_eq!(route.agent_id, agent_id);
        assert_eq!(route.sandbox_id, sandbox_id);
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
        let route = cache.lookup(&sandbox_id.subdomain()).unwrap();
        assert_eq!(route.agent_id, agent_id);
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
        assert_eq!(
            cache.lookup(&sandbox_id.subdomain()).unwrap().agent_id,
            old_agent
        );

        store.clear();
        store.add_entry(sandbox_id.clone(), new_agent.clone());
        cache.refresh().await.unwrap();
        assert_eq!(
            cache.lookup(&sandbox_id.subdomain()).unwrap().agent_id,
            new_agent
        );
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
