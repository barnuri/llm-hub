use std::time::Duration;

use serde::Deserialize;
use serde_json::Value;

use super::hub_endpoint::HubEndpoint;
use crate::consts::VERSION;
use crate::schemas::launch::hub_model::HubModel;
use crate::schemas::launch::profile_filter::ProfileFilter;
use crate::schemas::selection_event::SelectionEvent;

/// The body of `GET /api/smart-auto/history`.
#[derive(Deserialize)]
struct HistoryPage {
    events: Vec<SelectionEvent>,
}

/// The few hub calls a launcher makes before it starts the harness.
#[derive(Debug, Clone)]
pub struct HubClient {
    endpoint: HubEndpoint,
    http: reqwest::Client,
    profiles: ProfileFilter,
}

impl HubClient {
    const HEALTH_TIMEOUT: Duration = Duration::from_secs(2);
    const FETCH_TIMEOUT: Duration = Duration::from_secs(3);
    const HISTORY_TIMEOUT: Duration = Duration::from_secs(10);

    /// A client for `endpoint`.
    ///
    /// # Errors
    /// Returns a message when the HTTP client cannot be built.
    pub fn new(endpoint: HubEndpoint) -> Result<Self, String> {
        let http = reqwest::Client::builder()
            .user_agent(format!("llm-hub/{VERSION}"))
            .build()
            .map_err(|e| e.to_string())?;
        Ok(Self {
            endpoint,
            http,
            profiles: ProfileFilter::default(),
        })
    }

    /// Limits every model and settings fetch to `profiles`.
    #[must_use]
    pub fn with_profiles(mut self, profiles: ProfileFilter) -> Self {
        self.profiles = profiles;
        self
    }

    /// The profiles the launched harness is limited to.
    #[must_use]
    pub fn profiles(&self) -> &ProfileFilter {
        &self.profiles
    }

    /// The hub the client calls.
    #[must_use]
    pub fn endpoint(&self) -> &HubEndpoint {
        &self.endpoint
    }

    /// Checks that the hub answers `GET /healthz`.
    ///
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

    /// Smart Auto selection events of `workflow` after `since_seq`, oldest first, at most `limit`.
    ///
    /// # Errors
    /// Returns a message when the request fails, the hub answers an error status, or the body is not history.
    pub async fn smart_auto_history(
        &self,
        workflow: &str,
        since_seq: u64,
        limit: usize,
    ) -> Result<Vec<SelectionEvent>, String> {
        let url = format!("{}/api/smart-auto/history", self.endpoint.url());
        let response = self
            .http
            .get(url)
            .bearer_auth(self.endpoint.api_key())
            .query(&[
                ("workflow", workflow.to_string()),
                ("since_seq", since_seq.to_string()),
                ("limit", limit.to_string()),
            ])
            .timeout(Self::HISTORY_TIMEOUT)
            .send()
            .await
            .and_then(reqwest::Response::error_for_status)
            .map_err(|e| format!("smart-auto history request failed: {e}"))?;
        let body = response
            .text()
            .await
            .map_err(|e| format!("smart-auto history body could not be read: {e}"))?;
        serde_json::from_str::<HistoryPage>(&body)
            .map(|page| page.events)
            .map_err(|e| format!("smart-auto history body is not valid: {e}"))
    }

    async fn fetch(&self, path: &str) -> Option<String> {
        let mut request = self
            .http
            .get(format!("{}{path}", self.endpoint.url()))
            .bearer_auth(self.endpoint.api_key());
        if !self.profiles.is_empty() {
            request = request.query(&[(ProfileFilter::QUERY_PARAM, self.profiles.to_list())]);
        }
        let response = request
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
    use wiremock::matchers::{header, method, path, query_param};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    use super::*;
    use crate::schemas::selection_stage::SelectionStage;
    use crate::schemas::selection_state::SelectionState;

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
        let settings = r#"{"modelPicker":{"replaceBuiltInOptions":true,"options":[{"model":"copilot/auto"},{"model":"copilot/opus[1m]"}]}}"#;
        Mock::given(method("GET"))
            .and(path("/api/claude-code/settings"))
            .and(header("authorization", "Bearer key-1"))
            .respond_with(ResponseTemplate::new(200).set_body_string(settings))
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

        assert_eq!(client.claude_settings().await.as_deref(), Some(settings));
        assert_eq!(
            client.models().await,
            [HubModel::new("copilot/auto", Some(128_000))]
        );
    }

    #[tokio::test]
    async fn profiles_are_sent_on_settings_and_models() {
        let server = MockServer::start().await;
        Mock::given(path("/api/claude-code/settings"))
            .and(query_param("profiles", "copilot,ivrit"))
            .respond_with(ResponseTemplate::new(200).set_body_string("{}"))
            .mount(&server)
            .await;
        Mock::given(path("/v1/models"))
            .and(query_param("profiles", "copilot,ivrit"))
            .respond_with(
                ResponseTemplate::new(200).set_body_json(json!({"data": [{"id": "ivrit/m"}]})),
            )
            .mount(&server)
            .await;
        let client =
            client_for(&server.uri()).with_profiles(ProfileFilter::from_list("copilot,ivrit"));

        assert_eq!(client.claude_settings().await.as_deref(), Some("{}"));
        assert_eq!(client.models().await, [HubModel::new("ivrit/m", None)]);
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
    async fn history_sends_the_workflow_cursor_and_bearer() {
        let server = MockServer::start().await;
        let mut event = SelectionEvent::new(
            SelectionStage::Solver,
            SelectionState::Selected,
            "copilot/m",
        );
        event.seq = 4;
        Mock::given(method("GET"))
            .and(path("/api/smart-auto/history"))
            .and(query_param("workflow", "wf-1"))
            .and(query_param("since_seq", "3"))
            .and(query_param("limit", "50"))
            .and(header("authorization", "Bearer key-1"))
            .respond_with(
                ResponseTemplate::new(200)
                    .set_body_json(json!({"events": [serde_json::to_value(&event).unwrap()]})),
            )
            .mount(&server)
            .await;

        let events = client_for(&server.uri())
            .smart_auto_history("wf-1", 3, 50)
            .await
            .unwrap();

        assert_eq!(events, [event]);
    }

    #[tokio::test]
    async fn history_error_status_is_an_error() {
        let server = MockServer::start().await;
        Mock::given(path("/api/smart-auto/history"))
            .respond_with(ResponseTemplate::new(401))
            .mount(&server)
            .await;

        assert!(
            client_for(&server.uri())
                .smart_auto_history("wf-1", 0, 50)
                .await
                .is_err()
        );
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
