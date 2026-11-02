//! Upstream base URL and credentials for one profile: a static `API_KEY`
//! bearer, or a native GitHub Copilot session.

use reqwest::StatusCode;
use reqwest::header::{AUTHORIZATION, HeaderMap, HeaderName, HeaderValue};

use crate::configs::{CopilotAuth, ProfileConfig};
use crate::dependencies::state::AppState;
use crate::services::copilot_headers::{self, HEADER_X_INITIATOR, INITIATOR_AGENT};

/// Inserts the profile's upstream credentials into `headers` and returns the
/// base URL to send to, plus the Copilot bearer when the profile uses Copilot
/// auth. `path` and `body` pick the per-request Copilot headers; an empty body
/// gets the `/models` header set. A Copilot header named in the profile's
/// `HEADERS` is left for those to set.
///
/// # Errors
/// When the static `API_KEY` is not a valid header value, or when the Copilot
/// session cannot be acquired.
pub async fn resolve(
    state: &AppState,
    client: &reqwest::Client,
    profile: &ProfileConfig,
    path: &str,
    body: &[u8],
    headers: &mut HeaderMap,
) -> Result<(String, Option<String>), String> {
    let Some(auth) = &profile.copilot else {
        if !profile.api_key.is_empty() {
            headers.insert(AUTHORIZATION, bearer_value(profile, &profile.api_key)?);
        }
        return Ok((profile.base_url.clone(), None));
    };
    let (bearer, api_base) = state
        .copilot_sessions()
        .get(client, &profile.name, auth)
        .await?;
    headers.insert(AUTHORIZATION, bearer_value(profile, &bearer)?);
    insert_copilot_headers(profile, auth, path, body, headers);
    let base = api_base.unwrap_or_else(|| profile.base_url.clone());
    Ok((base, Some(bearer)))
}

/// Drops the profile's Copilot session when the upstream answered 401 to
/// `copilot_bearer`. Returns whether it did, so the caller can retry once.
pub async fn invalidate_if_unauthorized(
    state: &AppState,
    profile: &str,
    status: StatusCode,
    copilot_bearer: Option<&str>,
) -> bool {
    let Some(bearer) = copilot_bearer.filter(|_| status == StatusCode::UNAUTHORIZED) else {
        return false;
    };
    state.copilot_sessions().invalidate(profile, bearer).await;
    true
}

fn insert_copilot_headers(
    profile: &ProfileConfig,
    auth: &CopilotAuth,
    path: &str,
    body: &[u8],
    headers: &mut HeaderMap,
) {
    let path = crate::services::copilot_auto::bare_path(path);
    let copilot_headers = if body.is_empty() {
        copilot_headers::models_headers(auth.flow)
    } else {
        copilot_headers::request_headers(auth.flow, path, body)
    };
    let caller_is_agent = headers
        .get(HEADER_X_INITIATOR)
        .is_some_and(|value| value.as_bytes() == INITIATOR_AGENT.as_bytes());
    for (name, value) in copilot_headers {
        if is_profile_header(profile, &name) || (caller_is_agent && name == HEADER_X_INITIATOR) {
            continue;
        }
        headers.insert(name, value);
    }
}

fn is_profile_header(profile: &ProfileConfig, name: &HeaderName) -> bool {
    profile
        .extra_headers
        .iter()
        .any(|(configured, _)| configured.eq_ignore_ascii_case(name.as_str()))
}

fn bearer_value(profile: &ProfileConfig, token: &str) -> Result<HeaderValue, String> {
    let mut value = HeaderValue::from_str(&format!("Bearer {token}")).map_err(|_| {
        format!(
            "profile {}: the upstream credential is not a valid header value",
            profile.name
        )
    })?;
    value.set_sensitive(true);
    Ok(value)
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;
    use std::sync::Arc;

    use axum::http::Method;
    use bytes::Bytes;
    use serde_json::{Value, json};
    use wiremock::matchers::{header, method, path};
    use wiremock::{Mock, MockServer, Request, ResponseTemplate};

    use super::*;
    use crate::configs::{CopilotFlow, HubConfig};
    use crate::routes::models::refresh_models;
    use crate::routes::proxy::attempt_loop;
    use crate::services::credential_store::CredentialStore;
    use crate::services::fake_gh_cli::FakeGhCli;
    use crate::services::gh_cli::GhOutcome;
    use crate::services::memory_credential_store::MemoryCredentialStore;
    use crate::services::transforms::RouteKind;

    const DEAD_BASE_URL: &str = "http://127.0.0.1:9";
    const GH_TOKEN: &str = "gho_from_gh";
    const LOGIN_TOKEN: &str = "gho_from_login";
    const FIRST_COPILOT_TOKEN: &str = "tid=first";
    const SECOND_COPILOT_TOKEN: &str = "tid=second";
    const CHAT_PATH: &str = "/v1/chat/completions";
    const UPSTREAM_CHAT_PATH: &str = "/chat/completions";
    const EXCHANGE_PATH: &str = "/copilot_internal/v2/token";
    const USER_PATH: &str = "/copilot_internal/user";
    const HUB_KEY: &str = "hub-key";

    fn config(vars: &[(&str, &str)]) -> HubConfig {
        let vars: HashMap<String, String> = vars
            .iter()
            .map(|(key, value)| ((*key).to_string(), (*value).to_string()))
            .collect();
        HubConfig::from_map(&vars).unwrap()
    }

    fn copilot_config(auth: CopilotFlow, github: &MockServer) -> HubConfig {
        config(&[
            ("LLM_HUB_PROFILES", "copilot"),
            ("LLM_HUB_COPILOT_BASE_URL", DEAD_BASE_URL),
            ("LLM_HUB_COPILOT_AUTH", auth.auth_value()),
            ("LLM_HUB_COPILOT_GITHUB_API_URL", &github.uri()),
            (
                "LLM_HUB_COPILOT_HEADERS",
                r#"{"User-Agent":"from-profile"}"#,
            ),
        ])
    }

    fn state_with(config: HubConfig, gh: Arc<FakeGhCli>) -> AppState {
        let store = MemoryCredentialStore::default();
        for flow in [CopilotFlow::Opencode, CopilotFlow::Vscode] {
            store
                .set(&flow.credential_account("github.com"), LOGIN_TOKEN)
                .unwrap();
        }
        AppState::with_seams(config, None, Arc::new(store), gh).unwrap()
    }

    fn gh_with_token() -> Arc<FakeGhCli> {
        Arc::new(FakeGhCli::new(GhOutcome::Token(GH_TOKEN.to_string())))
    }

    fn chat_body(model: &str) -> Bytes {
        Bytes::from(
            json!({"model": model, "messages": [{"role": "user", "content": "hi"}]}).to_string(),
        )
    }

    fn client_headers() -> axum::http::HeaderMap {
        let mut headers = axum::http::HeaderMap::new();
        headers.insert(
            "authorization",
            format!("Bearer {HUB_KEY}").parse().unwrap(),
        );
        headers.insert("x-api-key", HUB_KEY.parse().unwrap());
        headers.insert("content-type", "application/json".parse().unwrap());
        headers
    }

    async fn chat(state: &AppState, model: &str) -> u16 {
        attempt_loop(
            state,
            Method::POST,
            CHAT_PATH,
            &client_headers(),
            chat_body(model),
            RouteKind::OpenAi,
        )
        .await
        .unwrap()
        .status()
        .as_u16()
    }

    fn ok_completion() -> ResponseTemplate {
        ResponseTemplate::new(200).set_body_json(json!({"id": "chatcmpl-1", "choices": []}))
    }

    fn lacks(name: &'static str) -> impl Fn(&Request) -> bool {
        move |request: &Request| !request.headers.contains_key(name)
    }

    fn token_response(token: &str, copilot: &MockServer) -> ResponseTemplate {
        ResponseTemplate::new(200).set_body_json(json!({
            "token": token,
            "expires_at": 4_000_000_000_u64,
            "refresh_in": 1500,
            "endpoints": {"api": copilot.uri()},
        }))
    }

    async fn mount_user(github: &MockServer, copilot: &MockServer) {
        Mock::given(method("GET"))
            .and(path(USER_PATH))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "chat_enabled": true,
                "endpoints": {"api": copilot.uri()},
            })))
            .mount(github)
            .await;
    }

    fn header_values(request: &Request, name: &str) -> Vec<String> {
        request
            .headers
            .get_all(name)
            .iter()
            .map(|value| value.to_str().unwrap().to_string())
            .collect()
    }

    #[tokio::test]
    async fn vscode_flow_sends_the_exchanged_token_and_exchanges_once() {
        let github = MockServer::start().await;
        let copilot = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path(EXCHANGE_PATH))
            .and(header(
                "authorization",
                format!("token {LOGIN_TOKEN}").as_str(),
            ))
            .respond_with(token_response(FIRST_COPILOT_TOKEN, &copilot))
            .expect(1)
            .mount(&github)
            .await;
        Mock::given(method("POST"))
            .and(path(UPSTREAM_CHAT_PATH))
            .and(header(
                "authorization",
                format!("Bearer {FIRST_COPILOT_TOKEN}").as_str(),
            ))
            .and(header("copilot-integration-id", "vscode-chat"))
            .and(header("x-initiator", "user"))
            .respond_with(ok_completion())
            .expect(2)
            .mount(&copilot)
            .await;
        let state = state_with(
            copilot_config(CopilotFlow::Vscode, &github),
            gh_with_token(),
        );

        assert_eq!(chat(&state, "copilot/gpt-4.1").await, 200);
        assert_eq!(chat(&state, "copilot/gpt-4.1").await, 200);
    }

    #[tokio::test]
    async fn vscode_flow_re_exchanges_and_retries_after_a_401() {
        let github = MockServer::start().await;
        let copilot = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path(EXCHANGE_PATH))
            .respond_with(token_response(FIRST_COPILOT_TOKEN, &copilot))
            .up_to_n_times(1)
            .expect(1)
            .mount(&github)
            .await;
        Mock::given(method("GET"))
            .and(path(EXCHANGE_PATH))
            .respond_with(token_response(SECOND_COPILOT_TOKEN, &copilot))
            .expect(1)
            .mount(&github)
            .await;
        Mock::given(method("POST"))
            .and(header(
                "authorization",
                format!("Bearer {FIRST_COPILOT_TOKEN}").as_str(),
            ))
            .respond_with(ResponseTemplate::new(401))
            .expect(1)
            .mount(&copilot)
            .await;
        Mock::given(method("POST"))
            .and(header(
                "authorization",
                format!("Bearer {SECOND_COPILOT_TOKEN}").as_str(),
            ))
            .respond_with(ok_completion())
            .expect(1)
            .mount(&copilot)
            .await;
        let state = state_with(
            copilot_config(CopilotFlow::Vscode, &github),
            gh_with_token(),
        );

        assert_eq!(chat(&state, "copilot/gpt-4.1").await, 200);
    }

    #[tokio::test]
    async fn opencode_flow_sends_the_gh_token_with_minimal_headers_to_the_user_base() {
        let github = MockServer::start().await;
        let copilot = MockServer::start().await;
        mount_user(&github, &copilot).await;
        Mock::given(method("POST"))
            .and(path(UPSTREAM_CHAT_PATH))
            .and(header(
                "authorization",
                format!("Bearer {GH_TOKEN}").as_str(),
            ))
            .and(header("openai-intent", "conversation-edits"))
            .and(lacks("copilot-integration-id"))
            .and(lacks("x-github-api-version"))
            .and(lacks("x-api-key"))
            .respond_with(ok_completion())
            .expect(1)
            .mount(&copilot)
            .await;
        let state = state_with(
            copilot_config(CopilotFlow::Opencode, &github),
            gh_with_token(),
        );

        assert_eq!(chat(&state, "copilot/gpt-4.1").await, 200);

        let received = copilot.received_requests().await.unwrap();
        assert_eq!(header_values(&received[0], "user-agent"), ["from-profile"]);
        assert_eq!(
            header_values(&received[0], "authorization"),
            [format!("Bearer {GH_TOKEN}")]
        );
    }

    #[tokio::test]
    async fn opencode_flow_asks_gh_again_and_retries_after_a_401() {
        let github = MockServer::start().await;
        let copilot = MockServer::start().await;
        mount_user(&github, &copilot).await;
        Mock::given(method("POST"))
            .respond_with(ResponseTemplate::new(401))
            .up_to_n_times(1)
            .expect(1)
            .mount(&copilot)
            .await;
        Mock::given(method("POST"))
            .and(header(
                "authorization",
                format!("Bearer {GH_TOKEN}").as_str(),
            ))
            .respond_with(ok_completion())
            .expect(1)
            .mount(&copilot)
            .await;
        let gh = gh_with_token();
        let state = state_with(
            copilot_config(CopilotFlow::Opencode, &github),
            Arc::clone(&gh),
        );

        assert_eq!(chat(&state, "copilot/gpt-4.1").await, 200);
        assert_eq!(gh.calls(), 2);
    }

    #[tokio::test]
    async fn caller_agent_initiator_is_kept() {
        let github = MockServer::start().await;
        let copilot = MockServer::start().await;
        mount_user(&github, &copilot).await;
        Mock::given(method("POST"))
            .and(header("x-initiator", "agent"))
            .respond_with(ok_completion())
            .expect(1)
            .mount(&copilot)
            .await;
        let state = state_with(
            copilot_config(CopilotFlow::Opencode, &github),
            gh_with_token(),
        );
        let mut headers = client_headers();
        headers.insert("x-initiator", "agent".parse().unwrap());

        let response = attempt_loop(
            &state,
            Method::POST,
            CHAT_PATH,
            &headers,
            chat_body("copilot/gpt-4.1"),
            RouteKind::OpenAi,
        )
        .await
        .unwrap();

        assert_eq!(response.status().as_u16(), 200);
        let received = copilot.received_requests().await.unwrap();
        assert_eq!(header_values(&received[0], "x-initiator"), ["agent"]);
    }

    #[tokio::test]
    async fn static_profile_sends_its_own_key_and_never_the_hub_key() {
        let upstream = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path(UPSTREAM_CHAT_PATH))
            .and(header("authorization", "Bearer sk-static"))
            .and(header("x-org", "me"))
            .and(lacks("x-api-key"))
            .and(lacks("x-initiator"))
            .respond_with(ok_completion())
            .expect(1)
            .mount(&upstream)
            .await;
        let state = state_with(
            config(&[
                ("LLM_HUB_PROFILES", "static"),
                ("LLM_HUB_STATIC_BASE_URL", &upstream.uri()),
                ("LLM_HUB_STATIC_API_KEY", "sk-static"),
                ("LLM_HUB_STATIC_HEADERS", r#"{"x-org":"me"}"#),
            ]),
            gh_with_token(),
        );

        assert_eq!(chat(&state, "static/gpt-4.1").await, 200);

        let received = upstream.received_requests().await.unwrap();
        assert_eq!(
            header_values(&received[0], "authorization"),
            ["Bearer sk-static"]
        );
    }

    #[tokio::test]
    async fn static_profile_without_key_sends_no_authorization() {
        let upstream = MockServer::start().await;
        Mock::given(method("POST"))
            .and(lacks("authorization"))
            .respond_with(ok_completion())
            .expect(1)
            .mount(&upstream)
            .await;
        let state = state_with(
            config(&[
                ("LLM_HUB_PROFILES", "local"),
                ("LLM_HUB_LOCAL_BASE_URL", &upstream.uri()),
            ]),
            gh_with_token(),
        );

        assert_eq!(chat(&state, "local/qwen").await, 200);
    }

    #[tokio::test]
    async fn refresh_models_lists_copilot_models_and_drops_disabled_ones() {
        let github = MockServer::start().await;
        let copilot = MockServer::start().await;
        mount_user(&github, &copilot).await;
        Mock::given(method("GET"))
            .and(path("/models"))
            .and(header("authorization", format!("Bearer {GH_TOKEN}").as_str()))
            .and(lacks("copilot-integration-id"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({"data": [
                {"id": "gpt-4.1", "model_picker_enabled": true, "policy": {"state": "enabled"}},
                {"id": "gpt-hidden", "model_picker_enabled": false},
                {"id": "gpt-blocked", "model_picker_enabled": true, "policy": {"state": "disabled"}},
                {"id": "gemini-2.5-pro"},
            ]})))
            .expect(1)
            .mount(&copilot)
            .await;
        let state = state_with(
            copilot_config(CopilotFlow::Opencode, &github),
            gh_with_token(),
        );

        let payload = refresh_models(&state).await;

        // Smart Auto `hub/*` entries are appended after the profile lists; this test covers the Copilot list.
        let ids: Vec<&str> = payload["data"]
            .as_array()
            .unwrap()
            .iter()
            .filter_map(|model| model["id"].as_str())
            .filter(|id| id.starts_with("copilot/"))
            .collect();
        assert_eq!(
            ids,
            [
                "copilot/gpt-4.1",
                "copilot/gemini-2.5-pro",
                "copilot/auto-efficiency",
                "copilot/auto",
                "copilot/auto-intelligence",
                "copilot/auto-fast"
            ]
        );
        assert_eq!(state.upstream_health().get("copilot"), Some(&true));
    }

    #[tokio::test]
    async fn refresh_models_keeps_disabled_entries_for_static_profiles() {
        let upstream = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/models"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({"data": [
                {"id": "gpt-hidden", "model_picker_enabled": false},
            ]})))
            .mount(&upstream)
            .await;
        let state = state_with(
            config(&[
                ("LLM_HUB_PROFILES", "proxy"),
                ("LLM_HUB_PROXY_BASE_URL", &upstream.uri()),
            ]),
            gh_with_token(),
        );

        let payload = refresh_models(&state).await;

        assert_eq!(payload["data"][0]["id"], Value::from("proxy/gpt-hidden"));
    }
}
