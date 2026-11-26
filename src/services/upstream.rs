use std::collections::HashMap;

use crate::configs::HubConfig;
use crate::consts::{CONNECT_TIMEOUT, POOL_IDLE_TIMEOUT, POOL_MAX_IDLE_PER_HOST};

/// One pooled client handle per profile. Pool sizing is the main latency lever:
/// default reqwest keeps only 2 idle connections per host, which forces a
/// fresh TCP+TLS handshake on nearly every concurrent request.
/// Deliberately NO whole-request `Client::timeout` — it would sever
/// long-lived SSE streams; per-attempt deadlines wrap `execute` instead.
pub fn build_clients(config: &HubConfig) -> Result<HashMap<String, reqwest::Client>, String> {
    build_clients_with(config, reqwest::Client::builder)
}

/// [`build_clients`] with each profile's builder made by `new_builder`.
fn build_clients_with(
    config: &HubConfig,
    new_builder: fn() -> reqwest::ClientBuilder,
) -> Result<HashMap<String, reqwest::Client>, String> {
    let mut clients = HashMap::with_capacity(config.profiles.len());
    let Some(first) = config.profiles.first() else {
        return Ok(clients);
    };
    // Every profile's client has the same settings, and building one loads the system root
    // certificates. A `Client` is a cheap handle, so one build serves all profiles.
    let shared = new_builder()
        .pool_max_idle_per_host(POOL_MAX_IDLE_PER_HOST)
        .pool_idle_timeout(POOL_IDLE_TIMEOUT)
        .connect_timeout(CONNECT_TIMEOUT)
        .build()
        .map_err(|e| {
            format!(
                "failed to build http client for profile {}: {e}",
                first.name
            )
        })?;
    for profile in &config.profiles {
        clients.insert(profile.name.clone(), shared.clone());
    }
    Ok(clients)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn config() -> HubConfig {
        let vars: HashMap<String, String> = [
            ("LLM_HUB_PROFILES", "alpha,beta"),
            ("LLM_HUB_ALPHA_BASE_URL", "http://127.0.0.1:9"),
            ("LLM_HUB_BETA_BASE_URL", "http://127.0.0.1:9"),
        ]
        .into_iter()
        .map(|(key, value)| (key.to_owned(), value.to_owned()))
        .collect();
        HubConfig::from_map(&vars).unwrap()
    }

    fn unbuildable() -> reqwest::ClientBuilder {
        reqwest::Client::builder()
            .min_tls_version(reqwest::tls::Version::TLS_1_3)
            .max_tls_version(reqwest::tls::Version::TLS_1_2)
    }

    #[test]
    fn every_profile_gets_its_own_client() {
        let clients = build_clients(&config()).unwrap();

        let mut names: Vec<&str> = clients.keys().map(String::as_str).collect();
        names.sort_unstable();
        assert_eq!(names, ["alpha", "beta"]);
    }

    #[test]
    fn a_client_that_cannot_be_built_names_its_profile() {
        let error = build_clients_with(&config(), unbuildable).unwrap_err();

        assert!(
            error.starts_with("failed to build http client for profile alpha: "),
            "{error}"
        );
    }
}
