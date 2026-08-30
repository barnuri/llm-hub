use std::collections::{HashMap, HashSet};
use std::sync::Arc;
use std::time::Instant;

use arc_swap::ArcSwap;
use dashmap::DashMap;

use crate::configs::HubConfig;
use crate::services::stats::StatsRegistry;
use crate::services::store::Store;
use crate::services::upstream::build_clients;

pub struct CachedModels {
    pub payload: serde_json::Value,
    pub fetched_at: Instant,
}

struct Inner {
    config: ArcSwap<HubConfig>,
    clients: ArcSwap<HashMap<String, reqwest::Client>>,
    stats: StatsRegistry,
    store: Option<Store>,
    /// SHA-256 hex hashes of enabled hub api keys, refreshed on CRUD.
    api_key_hashes: ArcSwap<HashSet<String>>,
    model_cache: ArcSwap<Option<Arc<CachedModels>>>,
    /// profile name -> reachable, updated on every models fan-out.
    upstream_health: DashMap<String, bool>,
}

#[derive(Clone)]
pub struct AppState(Arc<Inner>);

impl AppState {
    pub fn new(config: HubConfig, store: Option<Store>) -> Result<AppState, String> {
        let clients = build_clients(&config)?;
        let state = AppState(Arc::new(Inner {
            config: ArcSwap::from_pointee(config),
            clients: ArcSwap::from_pointee(clients),
            stats: StatsRegistry::default(),
            store,
            api_key_hashes: ArcSwap::from_pointee(HashSet::new()),
            model_cache: ArcSwap::from_pointee(None),
            upstream_health: DashMap::new(),
        }));
        state.refresh_api_key_hashes();
        Ok(state)
    }

    pub fn config(&self) -> Arc<HubConfig> {
        self.0.config.load_full()
    }

    /// Swaps in a new config and rebuilds the client pool atomically —
    /// in-flight requests keep the clients they already cloned.
    pub fn swap_config(&self, config: HubConfig) -> Result<(), String> {
        let clients = build_clients(&config)?;
        self.0.clients.store(Arc::new(clients));
        self.0.config.store(Arc::new(config));
        Ok(())
    }

    pub fn client(&self, profile: &str) -> Option<reqwest::Client> {
        self.0.clients.load().get(profile).cloned()
    }

    pub fn stats(&self) -> &StatsRegistry {
        &self.0.stats
    }

    pub fn store(&self) -> Option<&Store> {
        self.0.store.as_ref()
    }

    pub fn api_key_hashes(&self) -> Arc<HashSet<String>> {
        self.0.api_key_hashes.load_full()
    }

    pub fn refresh_api_key_hashes(&self) {
        let Some(store) = self.store() else {
            return;
        };
        match store.list_api_keys() {
            Ok(keys) => {
                let hashes = keys
                    .into_iter()
                    .filter(|k| k.enabled)
                    .map(|k| k.key_hash)
                    .collect();
                self.0.api_key_hashes.store(Arc::new(hashes));
            }
            Err(e) => tracing::warn!("failed to refresh api key cache: {e}"),
        }
    }

    pub fn model_cache(&self) -> Option<Arc<CachedModels>> {
        self.0.model_cache.load().as_ref().clone()
    }

    pub fn set_model_cache(&self, payload: serde_json::Value) {
        let cached = CachedModels {
            payload,
            fetched_at: Instant::now(),
        };
        self.0.model_cache.store(Arc::new(Some(Arc::new(cached))));
    }

    pub fn set_upstream_health(&self, profile: &str, healthy: bool) {
        self.0.upstream_health.insert(profile.to_string(), healthy);
    }

    pub fn upstream_health(&self) -> HashMap<String, bool> {
        self.0
            .upstream_health
            .iter()
            .map(|e| (e.key().clone(), *e.value()))
            .collect()
    }
}
