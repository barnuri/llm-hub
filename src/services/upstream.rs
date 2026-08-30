use std::collections::HashMap;

use crate::configs::HubConfig;
use crate::consts::{CONNECT_TIMEOUT, POOL_IDLE_TIMEOUT, POOL_MAX_IDLE_PER_HOST};

/// One pooled client per profile. Pool sizing is the main latency lever:
/// default reqwest keeps only 2 idle connections per host, which forces a
/// fresh TCP+TLS handshake on nearly every concurrent request.
/// Deliberately NO whole-request `Client::timeout` — it would sever
/// long-lived SSE streams; per-attempt deadlines wrap `execute` instead.
pub fn build_clients(config: &HubConfig) -> Result<HashMap<String, reqwest::Client>, String> {
    let mut clients = HashMap::with_capacity(config.profiles.len());
    for profile in &config.profiles {
        let client = reqwest::Client::builder()
            .pool_max_idle_per_host(POOL_MAX_IDLE_PER_HOST)
            .pool_idle_timeout(POOL_IDLE_TIMEOUT)
            .connect_timeout(CONNECT_TIMEOUT)
            .build()
            .map_err(|e| {
                format!(
                    "failed to build http client for profile {}: {e}",
                    profile.name
                )
            })?;
        clients.insert(profile.name.clone(), client);
    }
    Ok(clients)
}
