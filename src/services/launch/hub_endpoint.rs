/// Where a launched harness reaches the hub, and the bearer it sends.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HubEndpoint {
    url: String,
    api_key: String,
}

impl HubEndpoint {
    const URL_VAR: &str = "LLM_HUB_URL";
    const API_KEY_VAR: &str = "LLM_HUB_API_KEY";
    /// The installed service listens here.
    const DEFAULT_URL: &str = "http://localhost:8888";
    /// Harnesses need a non-empty key even when hub auth is off.
    const PLACEHOLDER_API_KEY: &str = "llm-hub";

    /// Reads `LLM_HUB_URL` and `LLM_HUB_API_KEY`. An empty value counts as unset.
    pub fn from_lookup(lookup: impl Fn(&str) -> Option<String>) -> Self {
        let read = |name: &str| lookup(name).filter(|value| !value.is_empty());
        let url = read(Self::URL_VAR).unwrap_or_else(|| Self::DEFAULT_URL.to_string());
        Self {
            url: url.trim_end_matches('/').to_string(),
            api_key: read(Self::API_KEY_VAR)
                .unwrap_or_else(|| Self::PLACEHOLDER_API_KEY.to_string()),
        }
    }

    /// The hub root, without a trailing slash.
    #[must_use]
    pub fn url(&self) -> &str {
        &self.url
    }

    /// The `OpenAI`-compatible base, `<hub>/v1`.
    #[must_use]
    pub fn v1_url(&self) -> String {
        format!("{}/v1", self.url)
    }

    /// The bearer harnesses send to the hub.
    #[must_use]
    pub fn api_key(&self) -> &str {
        &self.api_key
    }

    /// The variable that carries the key into harnesses that read it by name.
    #[must_use]
    pub fn api_key_var() -> &'static str {
        Self::API_KEY_VAR
    }

    /// The variable that carries the hub URL into harnesses that read it by name.
    #[must_use]
    pub fn url_var() -> &'static str {
        Self::URL_VAR
    }
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;

    use super::*;

    fn endpoint(vars: &[(&str, &str)]) -> HubEndpoint {
        let vars: HashMap<String, String> = vars
            .iter()
            .map(|(key, value)| ((*key).to_string(), (*value).to_string()))
            .collect();
        HubEndpoint::from_lookup(|name| vars.get(name).cloned())
    }

    #[test]
    fn unset_vars_use_the_service_port_and_placeholder_key() {
        let endpoint = endpoint(&[]);

        assert_eq!(endpoint.url(), "http://localhost:8888");
        assert_eq!(endpoint.v1_url(), "http://localhost:8888/v1");
        assert_eq!(endpoint.api_key(), "llm-hub");
    }

    #[test]
    fn vars_override_the_defaults() {
        let endpoint = endpoint(&[
            ("LLM_HUB_URL", "http://hub.local:8410/"),
            ("LLM_HUB_API_KEY", "secret"),
        ]);

        assert_eq!(endpoint.url(), "http://hub.local:8410");
        assert_eq!(endpoint.v1_url(), "http://hub.local:8410/v1");
        assert_eq!(endpoint.api_key(), "secret");
    }

    #[test]
    fn empty_vars_count_as_unset() {
        let endpoint = endpoint(&[("LLM_HUB_URL", ""), ("LLM_HUB_API_KEY", "")]);

        assert_eq!(endpoint.url(), "http://localhost:8888");
        assert_eq!(endpoint.api_key(), "llm-hub");
    }
}
