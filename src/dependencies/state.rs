use std::collections::{HashMap, HashSet};
use std::sync::Arc;

use arc_swap::ArcSwap;
use dashmap::{DashMap, DashSet};

use crate::configs::HubConfig;
use crate::consts::LOGS_DIR;
use crate::schemas::model_descriptor::ModelDescriptor;
use crate::schemas::model_descriptor_snapshot::ModelDescriptorSnapshot;
use crate::schemas::upstream_model::UpstreamModel;
use crate::services::copilot_auto::CopilotAutoRouter;
use crate::services::copilot_body_fields::{BodyField, UnsupportedBodyFields};
use crate::services::copilot_descriptions::CopilotDescriptions;
use crate::services::copilot_prices::CopilotPrices;
use crate::services::copilot_sessions::CopilotSessionCache;
use crate::services::copilot_usage::CopilotUsageCache;
use crate::services::credential_store::CredentialStore;
use crate::services::gh_cli::GhCli;
use crate::services::keep_warm::KeepWarm;
use crate::services::keyring_credential_store::KeyringCredentialStore;
use crate::services::managed_process::ProcessManager;
use crate::services::model_limits::{ModelLimit, ModelLimits};
use crate::services::pricing::PricingBook;
use crate::services::process_gh_cli::ProcessGhCli;
use crate::services::selection_log::SelectionLog;
use crate::services::smart_auto_affinity::SmartAutoAffinity;
use crate::services::smart_auto_decisions::SmartAutoDecisionLog;
use crate::services::smart_auto_rules::SmartAutoRulesStore;
use crate::services::stats::StatsRegistry;
use crate::services::store::{Store, now_ms, store_dir};
use crate::services::upstream::build_clients;

pub struct CachedModels {
    pub payload: serde_json::Value,
}

struct Inner {
    config: ArcSwap<HubConfig>,
    clients: ArcSwap<HashMap<String, reqwest::Client>>,
    stats: StatsRegistry,
    store: Option<Arc<Store>>,
    /// SHA-256 hex hashes of enabled hub api keys, refreshed on CRUD.
    api_key_hashes: ArcSwap<HashSet<String>>,
    model_cache: ArcSwap<Option<Arc<CachedModels>>>,
    /// Rebuilt with the model list on every refresh, never per request.
    model_descriptors: ArcSwap<ModelDescriptorSnapshot>,
    model_usage: ArcSwap<HashMap<String, u64>>,
    /// profile name -> reachable, updated on every models fan-out.
    upstream_health: DashMap<String, bool>,
    /// Copilot profile name -> advertised bare model id -> upstream entry.
    upstream_models: DashMap<String, Arc<HashMap<String, UpstreamModel>>>,
    /// Copilot profile name -> its usable raw `/models` entries, for Auto routing.
    copilot_catalogs: DashMap<String, Arc<Vec<serde_json::Value>>>,
    /// Set once the first models fan-out since startup has finished.
    first_refresh: tokio::sync::OnceCell<()>,
    /// Fired to trigger graceful shutdown (auto-update, env watcher, /api/restart).
    restart_notify: Arc<tokio::sync::Notify>,
    managed: Arc<ProcessManager>,
    copilot_sessions: CopilotSessionCache,
    copilot_auto: CopilotAutoRouter,
    copilot_usage: CopilotUsageCache,
    copilot_prices: CopilotPrices,
    copilot_descriptions: CopilotDescriptions,
    /// Real prompt limits per Copilot model, shared with `copilot_auto`.
    model_limits: Arc<ModelLimits>,
    /// Request fields each Copilot profile rejected on `/v1/messages`.
    unsupported_body_fields: UnsupportedBodyFields,
    /// `<profile>/<upstream model>` keys that refused image input, learned from upstream rejections.
    vision_unsupported: DashSet<String>,
    /// `<profile>/<upstream model>` -> the prompt limit an upstream 400 named in its message.
    rejected_prompt_limits: DashMap<String, u64>,
    smart_auto_affinity: SmartAutoAffinity,
    /// The bounded log of Smart Auto routes.
    smart_auto_decisions: SmartAutoDecisionLog,
    /// The selection history of Smart Auto and Copilot Auto dispatches.
    selection_log: SelectionLog,
    /// The user-editable Smart Auto include and exclude rules.
    smart_auto_rules: SmartAutoRulesStore,
    /// Idle Copilot Claude streams kept warm with pings.
    keep_warm: Arc<KeepWarm>,
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
        let copilot_profiles = config
            .profiles
            .iter()
            .filter(|p| p.copilot.is_some())
            .map(|p| p.name.as_str());
        let store = store.map(Arc::new);
        let model_limits = Arc::new(ModelLimits::load(
            store.as_deref(),
            copilot_profiles,
            now_ms(),
        ));
        let unsupported_body_fields = UnsupportedBodyFields::load(store.as_deref());
        let selection_log = SelectionLog::open(store.clone());
        let smart_auto_rules = SmartAutoRulesStore::open(
            &store_dir(config.store_path.as_deref()),
            config.smart_auto_rules_env.as_deref(),
        )?;
        let state = AppState(Arc::new(Inner {
            config: ArcSwap::from_pointee(config),
            clients: ArcSwap::from_pointee(clients),
            stats: StatsRegistry::default(),
            store,
            api_key_hashes: ArcSwap::from_pointee(HashSet::new()),
            model_cache: ArcSwap::from_pointee(None),
            model_descriptors: ArcSwap::from_pointee(ModelDescriptorSnapshot::default()),
            model_usage: ArcSwap::from_pointee(HashMap::new()),
            upstream_health: DashMap::new(),
            upstream_models: DashMap::new(),
            copilot_catalogs: DashMap::new(),
            first_refresh: tokio::sync::OnceCell::new(),
            restart_notify: Arc::new(tokio::sync::Notify::new()),
            managed: Arc::new(ProcessManager::new(LOGS_DIR)),
            copilot_sessions: CopilotSessionCache::new(Arc::clone(&credentials), Arc::clone(&gh)),
            copilot_auto: CopilotAutoRouter::with_limits(Arc::clone(&model_limits)),
            copilot_usage: CopilotUsageCache::new(credentials, gh),
            copilot_prices: CopilotPrices::default(),
            copilot_descriptions: CopilotDescriptions::default(),
            model_limits,
            unsupported_body_fields,
            vision_unsupported: DashSet::new(),
            rejected_prompt_limits: DashMap::new(),
            smart_auto_affinity: SmartAutoAffinity::new(),
            smart_auto_decisions: SmartAutoDecisionLog::default(),
            selection_log,
            smart_auto_rules,
            keep_warm: Arc::new(KeepWarm::default()),
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
        self.0.copilot_usage.reconcile(&config);
        let is_copilot = |name: &String| {
            config
                .profiles
                .iter()
                .any(|p| p.name == *name && p.copilot.is_some())
        };
        self.0.upstream_models.retain(|name, _| is_copilot(name));
        self.0.copilot_catalogs.retain(|name, _| is_copilot(name));
        self.0.config.store(Arc::new(config));
        Ok(())
    }

    pub fn client(&self, profile: &str) -> Option<reqwest::Client> {
        self.0.clients.load().get(profile).cloned()
    }

    pub(crate) fn model_usage(&self) -> Arc<HashMap<String, u64>> {
        self.0.model_usage.load_full()
    }

    pub(crate) fn set_model_usage(&self, usage: HashMap<String, u64>) {
        self.0.model_usage.store(Arc::new(usage));
    }

    pub fn stats(&self) -> &StatsRegistry {
        &self.0.stats
    }

    pub fn store(&self) -> Option<&Store> {
        self.0.store.as_deref()
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

    /// The descriptors from the last model refresh. Empty before the first one.
    pub fn model_descriptors(&self) -> Arc<ModelDescriptorSnapshot> {
        self.0.model_descriptors.load_full()
    }

    /// Swaps in the descriptors a model refresh built. Readers keep the snapshot they loaded.
    pub fn set_model_descriptors(&self, descriptors: Vec<ModelDescriptor>) {
        self.0
            .model_descriptors
            .store(Arc::new(ModelDescriptorSnapshot::new(descriptors)));
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

    /// False until a catalog for `profile` was fetched or restored from the store.
    pub fn has_upstream_models(&self, profile: &str) -> bool {
        self.0.upstream_models.contains_key(profile)
    }

    /// Swaps in a freshly fetched Copilot catalog and saves it to the store.
    /// Unchanged entries skip both, so a steady refresh never writes to disk.
    pub fn update_copilot_catalog(
        &self,
        profile: &str,
        entries: Vec<serde_json::Value>,
        models: HashMap<String, UpstreamModel>,
    ) {
        let unchanged = self.has_upstream_models(profile)
            && self
                .0
                .copilot_catalogs
                .get(profile)
                .is_some_and(|current| **current == entries);
        if unchanged {
            return;
        }
        if let Some(store) = self.store()
            && let Err(e) = store.save_model_catalog(profile, &entries)
        {
            tracing::warn!(profile, "cannot save the model catalog: {e}");
        }
        self.set_upstream_models(profile, models);
        self.set_copilot_catalog(profile, entries);
    }

    pub fn set_copilot_catalog(&self, profile: &str, entries: Vec<serde_json::Value>) {
        self.0
            .copilot_catalogs
            .insert(profile.to_string(), Arc::new(entries));
    }

    /// Empty when the profile's models were never fetched.
    pub fn copilot_catalog(&self, profile: &str) -> Arc<Vec<serde_json::Value>> {
        self.0
            .copilot_catalogs
            .get(profile)
            .map(|entries| Arc::clone(&entries))
            .unwrap_or_default()
    }

    pub fn first_refresh(&self) -> &tokio::sync::OnceCell<()> {
        &self.0.first_refresh
    }

    pub fn copilot_auto(&self) -> &CopilotAutoRouter {
        &self.0.copilot_auto
    }

    pub fn model_limits(&self) -> &ModelLimits {
        &self.0.model_limits
    }

    /// The Smart Auto model pins, shared by every request.
    pub fn smart_auto_affinity(&self) -> &SmartAutoAffinity {
        &self.0.smart_auto_affinity
    }

    /// The bounded log of Smart Auto routes.
    pub fn smart_auto_decisions(&self) -> &SmartAutoDecisionLog {
        &self.0.smart_auto_decisions
    }

    /// The Smart Auto include and exclude rules, read at startup and on each save.
    pub fn smart_auto_rules(&self) -> &SmartAutoRulesStore {
        &self.0.smart_auto_rules
    }

    pub fn keep_warm(&self) -> &Arc<KeepWarm> {
        &self.0.keep_warm
    }

    /// The selection history that Smart Auto and Copilot Auto dispatches are recorded in.
    pub fn selection_log(&self) -> &SelectionLog {
        &self.0.selection_log
    }

    /// Learns `model`'s prompt limit from a Copilot rejection body. A changed
    /// limit is logged and saved to the store off the calling task.
    pub fn learn_prompt_limit(&self, profile: &str, model: &str, error_body: &[u8]) {
        let Some(limit) = ModelLimits::limit_from_error(error_body) else {
            return;
        };
        let Some((entry, previous)) = self.0.model_limits.learn(profile, model, limit, now_ms())
        else {
            return;
        };
        let previous = previous.map_or_else(|| "unknown".to_string(), |p| p.to_string());
        tracing::info!(
            "profile {profile}: Copilot model {model} allows {limit} prompt tokens (was {previous})"
        );
        if self.store().is_some() {
            let state = self.clone();
            let (profile, model) = (profile.to_string(), model.to_string());
            tokio::task::spawn_blocking(move || state.save_prompt_limit(&profile, &model, &entry));
        }
    }

    /// Whether `model` on `profile` refused image input before. Held in memory until restart.
    pub fn vision_unsupported(&self, profile: &str, model: &str) -> bool {
        self.0
            .vision_unsupported
            .contains(&format!("{profile}/{model}"))
    }

    /// Records that `model` on `profile` refused image input, so later image requests skip it.
    pub fn learn_vision_unsupported(&self, profile: &str, model: &str) {
        if self
            .0
            .vision_unsupported
            .insert(format!("{profile}/{model}"))
        {
            tracing::info!(
                "profile {profile}: model {model} refuses image input; image requests skip it"
            );
        }
    }

    /// Records the prompt limit a 400 body names for `model` on `profile`. Held in memory until restart.
    pub fn learn_rejected_prompt_limit(&self, profile: &str, model: &str, body: &[u8]) {
        let Some(limit) = ModelLimits::limit_from_rejection(body) else {
            return;
        };
        let previous = self
            .0
            .rejected_prompt_limits
            .insert(format!("{profile}/{model}"), limit);
        if previous != Some(limit) {
            tracing::info!(
                "profile {profile}: model {model} rejects prompts over {limit} tokens; larger requests skip it"
            );
        }
    }

    /// The prompt limit a rejection taught for `model` on `profile`, if any.
    pub fn rejected_prompt_limit(&self, profile: &str, model: &str) -> Option<u64> {
        self.0
            .rejected_prompt_limits
            .get(&format!("{profile}/{model}"))
            .map(|limit| *limit)
    }

    /// The request fields `profile`'s Copilot upstream rejects.
    pub fn unsupported_body_fields(&self, profile: &str) -> Arc<Vec<BodyField>> {
        self.0.unsupported_body_fields.for_profile(profile)
    }

    /// Records the fields a Copilot rejection named. Each new field is logged
    /// once and saved to the store off the calling task.
    pub fn learn_unsupported_body_fields(&self, profile: &str, fields: &[BodyField]) {
        let new = self.0.unsupported_body_fields.learn(profile, fields);
        if new.is_empty() {
            return;
        }
        let stored: Vec<String> = new.iter().map(BodyField::stored).collect();
        for field in &stored {
            tracing::info!(
                "profile {profile}: Copilot rejects the request field {field}; the hub strips it from now on"
            );
        }
        if self.store().is_some() {
            let state = self.clone();
            let profile = profile.to_string();
            tokio::task::spawn_blocking(move || {
                let learned_at = now_ms();
                for field in &stored {
                    if let Some(store) = state.store()
                        && let Err(e) =
                            store.save_unsupported_body_field(&profile, field, learned_at)
                    {
                        tracing::warn!(
                            profile,
                            field,
                            "cannot save the unsupported body field: {e}"
                        );
                    }
                }
            });
        }
    }

    fn save_prompt_limit(&self, profile: &str, model: &str, entry: &ModelLimit) {
        if let Some(store) = self.store()
            && let Err(e) = store.save_model_limit(profile, model, entry)
        {
            tracing::warn!(profile, model, "cannot save the prompt limit: {e}");
        }
    }

    pub fn managed(&self) -> &Arc<ProcessManager> {
        &self.0.managed
    }

    pub fn copilot_sessions(&self) -> &CopilotSessionCache {
        &self.0.copilot_sessions
    }

    pub fn copilot_usage(&self) -> &CopilotUsageCache {
        &self.0.copilot_usage
    }

    pub fn copilot_prices(&self) -> &CopilotPrices {
        &self.0.copilot_prices
    }

    pub fn copilot_descriptions(&self) -> &CopilotDescriptions {
        &self.0.copilot_descriptions
    }

    /// Rates from the current config and the last fetched Copilot price table.
    pub fn pricing_book(&self) -> PricingBook {
        PricingBook::from_hub(&self.config()).with_copilot_prices(self.copilot_prices().snapshot())
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

#[cfg(test)]
mod tests {
    use super::*;

    fn descriptor(qualified_id: &str) -> ModelDescriptor {
        ModelDescriptor::for_test(qualified_id)
    }

    fn snapshot() -> ModelDescriptorSnapshot {
        ModelDescriptorSnapshot::new(vec![
            descriptor("copilot/claude-opus-5-5"),
            descriptor("llmgw/bedrock/claude-opus-5"),
        ])
    }

    #[test]
    fn get_finds_a_qualified_id_with_or_without_the_1m_alias() {
        let snapshot = snapshot();
        assert_eq!(snapshot.len(), 2);
        let plain = snapshot.get("copilot/claude-opus-5-5").unwrap();
        assert_eq!(plain.profile, "copilot");
        assert_eq!(snapshot.get("copilot/claude-opus-5-5[1m]"), Some(plain));
        assert_eq!(snapshot.get("copilot/claude-opus-5.5"), None);
        assert_eq!(snapshot.get("copilot/unknown"), None);
    }

    #[test]
    fn for_model_joins_profile_and_bare_id() {
        let snapshot = snapshot();
        assert_eq!(
            snapshot
                .for_model("llmgw", "bedrock/claude-opus-5[1M]")
                .map(|d| d.qualified_id.as_str()),
            Some("llmgw/bedrock/claude-opus-5")
        );
        assert_eq!(snapshot.for_model("copilot", "bedrock/claude-opus-5"), None);
    }

    #[test]
    fn later_descriptor_with_the_same_id_replaces_the_earlier_one() {
        let mut replacement = descriptor("copilot/gpt-9");
        replacement.chat = false;
        let snapshot = ModelDescriptorSnapshot::new(vec![descriptor("copilot/gpt-9"), replacement]);
        assert_eq!(snapshot.iter().count(), 1);
        assert!(!snapshot.get("copilot/gpt-9").unwrap().chat);
    }
}
