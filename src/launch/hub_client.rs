use std::time::Duration;

use serde_json::Value;

use super::hub_endpoint::HubEndpoint;
use super::hub_model::HubModel;
use crate::consts::VERSION;

/// The few hub calls a launcher makes before it starts the harness.
#[derive(Debug, Clone)]
pub struct HubClient {
    endpoint: HubEndpoint,
    http: reqwest::Client,
}

impl HubClient {
    const HEALTH_TIMEOUT: Duration = Duration::from_secs(2);
    const FETCH_TIMEOUT: Duration = Duration::from_secs(3);

    /// # Errors
    /// Returns a message when the HTTP client cannot be built.
    pub fn new(endpoint: HubEndpoint) -> Result<Self, String> {
        let http = reqwest::Client::builder()
            .user_agent(format!("llm-hub/{VERSION}"))
            .build()
            .map_err(|e| e.to_string())?;
        Ok(Self { endpoint, http })
    }

    #[must_use]
    pub fn endpoint(&self) -> &HubEndpoint {
        &self.endpoint
    }

    /// # Errors
    /// Returns a message when `GET /healthz` fails or takes more than 2 s.
    pub async fn check_reachable(&self) -> Result<(), String> {
        let url = self.endpoint.url();
        self.http
            .get(format!("{url}/healthz"))
            .timeout(Self::HEALTH_TIMEOUT)
            .send()
            .await
            .and_then(reqwest::Response::error_for_status)
            .map(|_| ())
            .map_err(|_| format!("llm-hub is not reachable at {url}"))
    }

    /// The `claude --settings` JSON, or `None` when the hub gives no body.
    pub async fn claude_settings(&self) -> Option<String> {
        self.fetch("/api/claude-code/settings")
            .await
            .filter(|body| !body.trim().is_empty())
    }

    /// The hub catalog. A failed fetch gives an empty catalog, so the
    /// harness starts with its own defaults.
    pub async fn models(&self) -> Vec<HubModel> {
        self.fetch("/v1/models")
            .await
            .and_then(|body| serde_json::from_str::<Value>(&body).ok())
            .map(|payload| HubModel::list_from(&payload))
            .unwrap_or_default()
    }

    async fn fetch(&self, path: &str) -> Option<String> {
        let response = self
            .http
            .get(format!("{}{path}", self.endpoint.url()))
            .bearer_auth(self.endpoint.api_key())
            .timeout(Self::FETCH_TIMEOUT)
            .send()
            .await
            .and_then(reqwest::Response::error_for_status)
            .ok()?;
        response.text().await.ok()
    }
}

#[cfg(test)]
mod tests {
    use serde_json::json;
    use wiremock::matchers::{header, method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    use super::*;

    fn client_for(url: &str) -> HubClient {
        let endpoint = HubEndpoint::from_lookup(|name| match name {
            "LLM_HUB_URL" => Some(url.to_string()),
            "LLM_HUB_API_KEY" => Some("key-1".to_string()),
            _ => None,
        });
        HubClient::new(endpoint).unwrap()
    }

    #[tokio::test]
    async fn unreachable_hub_names_the_url() {
        let client = client_for("http://127.0.0.1:9");

        let error = client.check_reachable().await.unwrap_err();

        assert_eq!(error, "llm-hub is not reachable at http://127.0.0.1:9");
    }

    #[tokio::test]
    async fn failing_health_check_is_unreachable() {
        let server = MockServer::start().await;
        Mock::given(path("/healthz"))
            .respond_with(ResponseTemplate::new(503))
            .mount(&server)
            .await;

        assert!(client_for(&server.uri()).check_reachable().await.is_err());
    }

    #[tokio::test]
    async fn healthy_hub_is_reachable() {
        let server = MockServer::start().await;
        Mock::given(path("/healthz"))
            .respond_with(ResponseTemplate::new(200))
            .mount(&server)
            .await;

        assert_eq!(client_for(&server.uri()).check_reachable().await, Ok(()));
    }

    #[tokio::test]
    async fn settings_and_models_send_the_bearer() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/api/claude-code/settings"))
            .and(header("authorization", "Bearer key-1"))
            .respond_with(ResponseTemplate::new(200).set_body_string("{\"modelPicker\":{}}"))
            .mount(&server)
            .await;
        Mock::given(method("GET"))
            .and(path("/v1/models"))
            .and(header("authorization", "Bearer key-1"))
            .respond_with(ResponseTemplate::new(200).set_body_json(
                json!({"data": [{"id": "copilot/auto", "context_window": 128_000}]}),
            ))
            .mount(&server)
            .await;
        let client = client_for(&server.uri());

        assert_eq!(
            client.claude_settings().await.as_deref(),
            Some("{\"modelPicker\":{}}")
        );
        assert_eq!(
            client.models().await,
            [HubModel::new("copilot/auto", Some(128_000))]
        );
    }

    #[tokio::test]
    async fn failed_fetches_give_no_settings_and_no_models() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .respond_with(ResponseTemplate::new(401))
            .mount(&server)
            .await;
        let client = client_for(&server.uri());

        assert_eq!(client.claude_settings().await, None);
        assert!(client.models().await.is_empty());
    }

    #[tokio::test]
    async fn empty_settings_body_is_no_settings() {
        let server = MockServer::start().await;
        Mock::given(path("/api/claude-code/settings"))
            .respond_with(ResponseTemplate::new(200))
            .mount(&server)
            .await;

        assert_eq!(client_for(&server.uri()).claude_settings().await, None);
    }
}
