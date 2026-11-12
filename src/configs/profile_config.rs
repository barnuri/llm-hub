#[derive(Debug, Clone, PartialEq)]
pub struct ProfileConfig {
    pub name: String,
    /// Optional UI label; routing / model ids always use `name`.
    pub display_name: Option<String>,
    pub base_url: String,
    pub api_key: String,
    pub extra_headers: Vec<(String, String)>,
    pub timeout_ms: Option<u64>,
    pub enabled: bool,
    /// Static model list used only when the upstream exposes no /v1/models.
    pub static_models: Vec<String>,
    /// Default USD per 1M tokens for this profile (overridden by `model_prices`).
    pub pricing: crate::configs::TokenRates,
    /// Bare model id → rates for this profile.
    pub model_prices: std::collections::HashMap<String, crate::configs::TokenRates>,
    /// Local server llm-hub starts on demand; env-only, never set through the admin API.
    pub managed: Option<crate::configs::ManagedCommand>,
    /// Native GitHub Copilot auth; env-only, never set through the admin API.
    pub copilot: Option<crate::configs::CopilotAuth>,
    /// Keep this profile's idle prompt caches warm with pings. `LLM_HUB_<NAME>_KEEP_WARM`, on by
    /// default. The global `LLM_HUB_KEEP_WARM` switch still has the last word.
    pub keep_warm: bool,
    /// Longest wait for the first byte of a streamed Smart Auto attempt on this profile.
    /// `LLM_HUB_<NAME>_FIRST_BYTE_TIMEOUT_MS`, env-only. It beats the global limit and applies to
    /// a local server too; `0` turns the limit off for this profile. `None` uses the global rule.
    pub first_byte_timeout_ms: Option<u64>,
}

impl ProfileConfig {
    /// Label shown in the UI — `display_name` when set, otherwise `name`.
    pub fn label(&self) -> &str {
        self.display_name
            .as_deref()
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .unwrap_or(&self.name)
    }

    /// True when the hub starts the server itself or the base URL points at this machine.
    pub fn is_local(&self) -> bool {
        if self.managed.is_some() {
            return true;
        }
        let Ok(url) = reqwest::Url::parse(&self.base_url) else {
            return false;
        };
        let Some(host) = url.host_str() else {
            return false;
        };
        let host = host.trim_start_matches('[').trim_end_matches(']');
        host.eq_ignore_ascii_case("localhost")
            || host
                .parse::<std::net::IpAddr>()
                .is_ok_and(|ip| ip.is_loopback())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn profile(base_url: &str) -> ProfileConfig {
        ProfileConfig {
            name: "p".into(),
            display_name: None,
            base_url: base_url.into(),
            api_key: String::new(),
            extra_headers: Vec::new(),
            timeout_ms: None,
            enabled: true,
            static_models: Vec::new(),
            pricing: crate::configs::TokenRates::default(),
            model_prices: std::collections::HashMap::new(),
            managed: None,
            copilot: None,
            keep_warm: true,
            first_byte_timeout_ms: None,
        }
    }

    #[test]
    fn loopback_and_localhost_urls_are_local() {
        assert!(profile("http://127.0.0.1:11436/v1").is_local());
        assert!(profile("http://LOCALHOST:1234/v1").is_local());
        assert!(profile("http://[::1]:8080").is_local());
    }

    #[test]
    fn remote_or_unparseable_urls_are_not_local() {
        assert!(!profile("https://api.githubcopilot.com").is_local());
        assert!(!profile("http://10.0.0.5:8080").is_local());
        assert!(!profile("not a url").is_local());
    }
}
