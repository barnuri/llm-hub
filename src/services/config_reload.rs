use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use sha2::{Digest, Sha256};
use tokio::sync::Notify;

use crate::configs::HubConfig;
use crate::consts::{ENV_FILE, ENV_WATCH_INTERVAL};
use crate::dependencies::state::AppState;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ReloadKind {
    Hot,
    /// Bind/port or store settings changed — process must restart to apply.
    NeedsRestart,
}

/// Re-read `.env`, swap config/clients, drop the models cache.
/// Returns whether a process restart is required for listen/store changes.
pub fn reload(state: &AppState) -> Result<ReloadKind, String> {
    let previous = state.config();
    let vars = crate::load_env_vars();
    let next = HubConfig::from_map(&vars)?;
    if requires_restart(&previous, &next) {
        return Ok(ReloadKind::NeedsRestart);
    }
    state.swap_config(next)?;
    state.clear_model_cache();
    Ok(ReloadKind::Hot)
}

fn requires_restart(previous: &HubConfig, next: &HubConfig) -> bool {
    previous.bind != next.bind
        || previous.port != next.port
        || previous.persistent != next.persistent
        || previous.store_kind != next.store_kind
        || previous.store_path != next.store_path
}

/// Poll `.env` content hash; hot-reload on change, or exit so a supervisor
/// restarts us when listen/store settings change.
pub fn spawn_env_watcher(state: AppState, restart_notify: Arc<Notify>) {
    tokio::spawn(async move {
        let mut last_hash = env_file_hash(Path::new(ENV_FILE));
        let mut interval = tokio::time::interval(ENV_WATCH_INTERVAL);
        interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        loop {
            interval.tick().await;
            let hash = env_file_hash(Path::new(ENV_FILE));
            if hash == last_hash {
                continue;
            }
            // Editors often write via temp+rename; give the final file a moment
            // to settle before parsing.
            tokio::time::sleep(Duration::from_millis(150)).await;
            let settled = env_file_hash(Path::new(ENV_FILE));
            if settled != hash {
                continue;
            }
            last_hash = settled;
            match reload(&state) {
                Ok(ReloadKind::Hot) => {
                    tracing::info!("reloaded config from {ENV_FILE}");
                }
                Ok(ReloadKind::NeedsRestart) => {
                    tracing::info!(
                        "{ENV_FILE} changed bind/port/store settings — restarting to apply"
                    );
                    restart_notify.notify_one();
                    return;
                }
                Err(e) => {
                    tracing::warn!("ignored invalid {ENV_FILE} change: {e}");
                }
            }
        }
    });
}

fn env_file_hash(path: &Path) -> Option<[u8; 32]> {
    let bytes = std::fs::read(path).ok()?;
    let digest = Sha256::digest(bytes);
    let mut out = [0u8; 32];
    out.copy_from_slice(&digest);
    Some(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::configs::ProfileConfig;

    fn sample(bind: &str, port: u16) -> HubConfig {
        HubConfig {
            profiles: vec![ProfileConfig {
                name: "local".into(),
                display_name: None,
                base_url: "http://127.0.0.1:1/v1".into(),
                api_key: String::new(),
                extra_headers: vec![],
                timeout_ms: None,
                enabled: true,
                static_models: vec![],
                pricing: Default::default(),
                model_prices: Default::default(),
            }],
            master_key: None,
            default_fallbacks: vec![],
            max_replay_bytes: 1,
            config_readonly: false,
            bind: bind.into(),
            port,
            persistent: false,
            store_kind: "sqlite".into(),
            store_path: None,
            auto_update: false,
            stream_role_inject: true,
            pricing: Default::default(),
        }
    }

    #[test]
    fn profile_only_change_is_hot() {
        let a = sample("127.0.0.1", 8410);
        let mut b = sample("127.0.0.1", 8410);
        b.master_key = Some("x".into());
        assert!(!requires_restart(&a, &b));
    }

    #[test]
    fn port_change_needs_restart() {
        let a = sample("127.0.0.1", 8410);
        let b = sample("127.0.0.1", 8411);
        assert!(requires_restart(&a, &b));
    }

    #[test]
    fn store_path_change_needs_restart() {
        let a = sample("127.0.0.1", 8410);
        let mut b = sample("127.0.0.1", 8410);
        b.store_path = Some("other.db".into());
        assert!(requires_restart(&a, &b));
    }
}
