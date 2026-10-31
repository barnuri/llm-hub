use axum::Json;
use axum::extract::{Query, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use serde::{Deserialize, Serialize};

use crate::consts::{AUTO_DECISIONS_DEFAULT_LIMIT, AUTO_DECISIONS_MAX_LIMIT};
use crate::dependencies::state::AppState;
use crate::schemas::app_error::AppError;
use crate::services::copilot_auto_decisions::TieredDecision;
use crate::services::copilot_usage::{self, CopilotUsage};

#[derive(Debug, Deserialize, Default)]
pub struct UsageQuery {
    pub profile: Option<String>,
}

#[derive(Serialize)]
struct UsageBody<'a> {
    profile: &'a str,
    #[serde(flatten)]
    usage: &'a CopilotUsage,
}

#[derive(Debug, Deserialize, Default)]
pub struct DecisionsQuery {
    pub session: Option<String>,
    pub limit: Option<usize>,
}

#[derive(Serialize)]
struct DecisionsBody {
    decisions: Vec<TieredDecision>,
}

/// `GET /api/copilot/auto/decisions?session=<id>&limit=N`: recent Copilot Auto
/// decisions, newest first, for one Claude Code session or for all of them.
/// `limit` defaults to 50 and is clamped to 1..=200.
pub async fn auto_decisions(
    State(state): State<AppState>,
    Query(query): Query<DecisionsQuery>,
) -> Response {
    let limit = query
        .limit
        .unwrap_or(AUTO_DECISIONS_DEFAULT_LIMIT)
        .clamp(1, AUTO_DECISIONS_MAX_LIMIT);
    let session = query.session.as_deref().filter(|id| !id.is_empty());
    let decisions = state.copilot_auto().decisions().recent(session, limit);
    Json(DecisionsBody { decisions }).into_response()
}

/// `GET /api/copilot/usage?profile=<name>`: the profile's Copilot quota, or
/// the first enabled Copilot profile's when `profile` is omitted.
pub async fn usage(State(state): State<AppState>, Query(query): Query<UsageQuery>) -> Response {
    let config = state.config();
    let selected = match query.profile.as_deref() {
        Some(name) => config.profile(name),
        None => config
            .profiles
            .iter()
            .find(|p| p.enabled && p.copilot.is_some()),
    };
    let Some((profile, auth)) = selected.and_then(|p| p.copilot.as_ref().map(|auth| (p, auth)))
    else {
        let message = query.profile.map_or_else(
            || "no enabled profile uses Copilot auth".to_string(),
            |name| format!("no Copilot profile named {name}"),
        );
        return AppError::NotFound(message).into_response();
    };
    let Some(client) = state.client(&profile.name) else {
        return AppError::Internal(format!("profile {} has no client", profile.name))
            .into_response();
    };
    let Some(usage) = copilot_usage::usage(&state, &client, &profile.name, auth).await else {
        let mut response = AppError::UpstreamUnavailable(format!(
            "profile {}: the hub could not fetch the Copilot quota. The hub log has the reason",
            profile.name
        ))
        .into_response();
        *response.status_mut() = StatusCode::SERVICE_UNAVAILABLE;
        return response;
    };
    Json(UsageBody {
        profile: &profile.name,
        usage: &usage,
    })
    .into_response()
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;
    use std::sync::Arc;

    use serde_json::{Value, json};
    use wiremock::matchers::{method, path};
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
            Arc::new(FakeGhCli::new(GhOutcome::Token("gho_from_gh".to_string()))),
        )
        .unwrap()
    }

    async fn call(state: AppState, profile: Option<&str>) -> (StatusCode, Value) {
        let query = UsageQuery {
            profile: profile.map(str::to_string),
        };
        let response = usage(State(state), Query(query)).await;
        let status = response.status();
        let bytes = axum::body::to_bytes(response.into_body(), usize::MAX)
            .await
            .unwrap();
        (status, serde_json::from_slice(&bytes).unwrap())
    }

    #[tokio::test]
    async fn non_copilot_and_unknown_profiles_are_404() {
        let state = state(&[
            ("LLM_HUB_PROFILES", "plain"),
            ("LLM_HUB_PLAIN_BASE_URL", "http://127.0.0.1:9"),
        ]);

        let (named, _) = call(state.clone(), Some("plain")).await;
        let (unknown, _) = call(state.clone(), Some("missing")).await;
        let (default, _) = call(state, None).await;

        assert_eq!(named, StatusCode::NOT_FOUND);
        assert_eq!(unknown, StatusCode::NOT_FOUND);
        assert_eq!(default, StatusCode::NOT_FOUND);
    }

    #[tokio::test]
    async fn default_profile_returns_the_quota_with_its_name() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/copilot_internal/user"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "chat_enabled": true,
                "quota_reset_date_utc": "2026-11-01T00:00:00Z",
                "quota_snapshots": {"premium_interactions": {
                    "credits_used": 12626, "entitlement": 33000, "remaining": 20489,
                    "percent_remaining": 62.0, "token_based_billing": true,
                }},
            })))
            .mount(&server)
            .await;
        let api_url = server.uri();
        let state = state(&[
            ("LLM_HUB_PROFILES", "plain,copilot"),
            ("LLM_HUB_PLAIN_BASE_URL", "http://127.0.0.1:9"),
            ("LLM_HUB_COPILOT_BASE_URL", "http://127.0.0.1:9"),
            ("LLM_HUB_COPILOT_AUTH", "github-copilot"),
            ("LLM_HUB_COPILOT_GITHUB_API_URL", &api_url),
        ]);

        let (status, body) = call(state, None).await;

        assert_eq!(status, StatusCode::OK);
        assert_eq!(body["profile"], "copilot");
        assert_eq!(body["source"], "copilot_internal/user");
        assert_eq!(body["quotas"][0]["unit"], "aic");
        assert_eq!(body["quotas"][0]["usedUsd"], json!(126.26));
    }

    #[tokio::test]
    async fn failed_first_fetch_is_503() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/copilot_internal/user"))
            .respond_with(ResponseTemplate::new(500))
            .mount(&server)
            .await;
        let api_url = server.uri();
        let state = state(&[
            ("LLM_HUB_PROFILES", "copilot"),
            ("LLM_HUB_COPILOT_BASE_URL", "http://127.0.0.1:9"),
            ("LLM_HUB_COPILOT_AUTH", "github-copilot"),
            ("LLM_HUB_COPILOT_GITHUB_API_URL", &api_url),
        ]);

        let (status, body) = call(state, Some("copilot")).await;

        assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE);
        assert_eq!(body["error"]["type"], "upstream_error");
    }
}
