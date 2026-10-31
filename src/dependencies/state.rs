use std::collections::{HashMap, HashSet};
use std::sync::Arc;

use arc_swap::ArcSwap;
use dashmap::DashMap;

use crate::configs::HubConfig;
use crate::consts::LOGS_DIR;
use crate::schemas::upstream_model::UpstreamModel;
use crate::services::copilot_sessions::CopilotSessionCache;
use crate::services::credential_store::CredentialStore;
use crate::services::gh_cli::GhCli;
use crate::services::keyring_credential_store::KeyringCredentialStore;
use crate::services::managed_process::ProcessManager;
use crate::services::process_gh_cli::ProcessGhCli;
use crate::services::stats::StatsRegistry;
use crate::services::store::Store;
use crate::services::upstream::build_clients;

pub struct CachedModels {
    pub payload: serde_json::Value,
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
    /// Copilot profile name -> advertised bare model id -> upstream entry.
    upstream_models: DashMap<String, Arc<HashMap<String, UpstreamModel>>>,
    /// Fired to trigger graceful shutdown (auto-update, env watcher, /api/restart).
    restart_notify: Arc<tokio::sync::Notify>,
    managed: Arc<ProcessManager>,
    copilot_sessions: CopilotSessionCache,
}

#[derive(Clone)]
pub struct AppState(Arc<Inner>);

impl AppState {
    pub fn new(config: HubConfig, store: Option<Store>) -> Result<AppState, String> {
        AppState::with_seams(
            config,
            store,
            Arc::new(KeyringCredentialStore),
            Arc::new(ProcessGhCli),
        )
    }

    /// [`Self::new`] with the credential store and gh runner passed in, so
    /// tests never touch the real keychain or gh.
    pub fn with_seams(
        config: HubConfig,
        store: Option<Store>,
        credentials: Arc<dyn CredentialStore>,
        gh: Arc<dyn GhCli>,
    ) -> Result<AppState, String> {
        let clients = build_clients(&config)?;
        let state = AppState(Arc::new(Inner {
            config: ArcSwap::from_pointee(config),
            clients: ArcSwap::from_pointee(clients),
            stats: StatsRegistry::default(),
            store,
            api_key_hashes: ArcSwap::from_pointee(HashSet::new()),
            model_cache: ArcSwap::from_pointee(None),
            upstream_health: DashMap::new(),
            upstream_models: DashMap::new(),
            restart_notify: Arc::new(tokio::sync::Notify::new()),
            managed: Arc::new(ProcessManager::new(LOGS_DIR)),
            copilot_sessions: CopilotSessionCache::new(credentials, gh),
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
        self.0.managed.reconcile(&config);
        self.0.copilot_sessions.reconcile(&config);
        self.0.upstream_models.retain(|name, _| {
            config
                .profiles
                .iter()
                .any(|p| p.name == *name && p.copilot.is_some())
        });
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
        let cached = CachedModels { payload };
        self.0.model_cache.store(Arc::new(Some(Arc::new(cached))));
    }

    pub fn clear_model_cache(&self) {
        self.0.model_cache.store(Arc::new(None));
    }

    pub fn set_upstream_models(&self, profile: &str, models: HashMap<String, UpstreamModel>) {
        self.0
            .upstream_models
            .insert(profile.to_string(), Arc::new(models));
    }

    /// `None` when the profile has no catalog or never listed `bare`.
    pub fn upstream_model(&self, profile: &str, bare: &str) -> Option<UpstreamModel> {
        self.0
            .upstream_models
            .get(profile)
            .and_then(|models| models.get(bare).cloned())
    }

    pub fn managed(&self) -> &Arc<ProcessManager> {
        &self.0.managed
    }

    pub fn copilot_sessions(&self) -> &CopilotSessionCache {
        &self.0.copilot_sessions
    }

    pub fn restart_notify(&self) -> Arc<tokio::sync::Notify> {
        self.0.restart_notify.clone()
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
