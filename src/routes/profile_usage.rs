use axum::Json;
use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use serde::Serialize;

use crate::dependencies::state::AppState;
use crate::schemas::app_error::AppError;
use crate::services::profile_usage::{self, ProfileUsage, UsageKey};

#[derive(Serialize)]
struct UsageBody<'a> {
    profile: &'a str,
    /// `copilot` or `bifrost`.
    kind: &'a str,
    #[serde(flatten)]
    usage: &'a ProfileUsage,
}

#[derive(Serialize)]
struct UsageSourceRow {
    profile: String,
    label: String,
    kind: &'static str,
}

#[derive(Serialize)]
struct UsageSourcesBody {
    sources: Vec<UsageSourceRow>,
}

/// `GET /api/usage/sources`: the enabled profiles that have a quota source, in config order.
pub async fn sources(State(state): State<AppState>) -> Response {
    let config = state.config();
    let sources = config
        .profiles
        .iter()
        .filter(|profile| profile.enabled)
        .filter_map(|profile| {
            UsageKey::for_profile(&config, &profile.name).map(|key| UsageSourceRow {
                profile: profile.name.clone(),
                label: profile.label().to_string(),
                kind: key.kind(),
            })
        })
        .collect();
    Json(UsageSourcesBody { sources }).into_response()
}

/// `GET /api/profiles/{name}/usage`: the profile's quota, whatever its source.
pub async fn usage(State(state): State<AppState>, Path(name): Path<String>) -> Response {
    let config = state.config();
    let Some(key) = UsageKey::for_profile(&config, &name) else {
        return AppError::NotFound(format!("profile {name} has no usage source")).into_response();
    };
    respond(&state, &name, &key).await
}

/// Fetches `profile`'s quota through the cache and renders it.
pub(crate) async fn respond(state: &AppState, profile: &str, key: &UsageKey) -> Response {
    let Some(client) = state.client(profile) else {
        return AppError::Internal(format!("profile {profile} has no client")).into_response();
    };
    let Some(usage) = profile_usage::usage(state, &client, profile, key).await else {
        let mut response = AppError::UpstreamUnavailable(format!(
            "profile {profile}: the hub could not fetch the {} quota. The hub log has the reason",
            key.kind()
        ))
        .into_response();
        *response.status_mut() = StatusCode::SERVICE_UNAVAILABLE;
        return response;
    };
    Json(UsageBody {
        profile,
        kind: key.kind(),
        usage: &usage,
    })
    .into_response()
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;
    use std::sync::Arc;

    use serde_json::{Value, json};
    use wiremock::matchers::{header, method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    use super::*;
    use crate::configs::HubConfig;
    use crate::services::fake_gh_cli::FakeGhCli;
    use crate::services::gh_cli::GhOutcome;
    use crate::services::memory_credential_store::MemoryCredentialStore;

    fn state(vars: &[(&str, &str)]) -> AppState {
        let vars: HashMap<String, String> = vars
            .iter()
            .map(|(key, value)| ((*key).to_string(), (*value).to_string()))
            .collect();
        AppState::with_seams(
            HubConfig::from_map(&vars).unwrap(),
            None,
            Arc::new(MemoryCredentialStore::default()),
            Arc::new(FakeGhCli::new(GhOutcome::NotInstalled)),
        )
        .unwrap()
    }

    async fn json_of(response: Response) -> (StatusCode, Value) {
        let status = response.status();
        let bytes = axum::body::to_bytes(response.into_body(), usize::MAX)
            .await
            .unwrap();
        (status, serde_json::from_slice(&bytes).unwrap())
    }

    #[tokio::test]
    async fn bifrost_profile_returns_its_quota_by_name() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/api/governance/virtual-keys/quota"))
            .and(header("x-bf-vk", "sk-bf-secret"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "budgets": [{"max_limit": 100.0, "current_usage": 40.0, "reset_duration": "1M"}],
            })))
            .mount(&server)
            .await;
        let base_url = format!("{}/v1", server.uri());
        let state = state(&[
            ("LLM_HUB_PROFILES", "llmgw"),
            ("LLM_HUB_LLMGW_BASE_URL", &base_url),
            ("LLM_HUB_LLMGW_API_KEY", "sk-bf-secret"),
        ]);

        let (status, body) = json_of(usage(State(state), Path("llmgw".into())).await).await;

        assert_eq!(status, StatusCode::OK);
        assert_eq!(body["profile"], "llmgw");
        assert_eq!(body["kind"], "bifrost");
        assert_eq!(body["quotas"][0]["id"], "budget:1M");
        assert_eq!(body["quotas"][0]["unit"], "usd");
        assert_eq!(body["quotas"][0]["usedUsd"], json!(40.0));
    }

    #[tokio::test]
    async fn profile_without_a_source_is_404() {
        let state = state(&[
            ("LLM_HUB_PROFILES", "plain"),
            ("LLM_HUB_PLAIN_BASE_URL", "http://127.0.0.1:9/v1"),
            ("LLM_HUB_PLAIN_API_KEY", "sk-other"),
        ]);

        let (status, _) = json_of(usage(State(state), Path("plain".into())).await).await;

        assert_eq!(status, StatusCode::NOT_FOUND);
    }

    #[tokio::test]
    async fn sources_lists_only_profiles_with_a_quota_source() {
        let state = state(&[
            ("LLM_HUB_PROFILES", "llmgw,plain,off"),
            ("LLM_HUB_LLMGW_BASE_URL", "http://127.0.0.1:9/v1"),
            ("LLM_HUB_LLMGW_API_KEY", "sk-bf-secret"),
            ("LLM_HUB_PLAIN_BASE_URL", "http://127.0.0.1:9/v1"),
            ("LLM_HUB_OFF_BASE_URL", "http://127.0.0.1:9/v1"),
            ("LLM_HUB_OFF_API_KEY", "sk-bf-secret"),
            ("LLM_HUB_OFF_USAGE", "none"),
        ]);

        let (_, body) = json_of(sources(State(state)).await).await;

        assert_eq!(
            body["sources"],
            json!([{"profile": "llmgw", "label": "llmgw", "kind": "bifrost"}])
        );
    }
}
