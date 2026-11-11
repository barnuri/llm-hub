use axum::Json;
use axum::extract::{Query, State};
use axum::response::{IntoResponse, Response};
use serde::{Deserialize, Serialize};

use crate::consts::{AUTO_DECISIONS_DEFAULT_LIMIT, AUTO_DECISIONS_MAX_LIMIT};
use crate::dependencies::state::AppState;
use crate::schemas::app_error::AppError;
use crate::schemas::quota_history::QuotaHistory;
use crate::schemas::quota_snapshot_row::QuotaSnapshotRow;
use crate::services::copilot_auto_decisions::TieredDecision;
use crate::services::profile_usage::UsageKey;
use crate::services::quota_snapshots::QuotaSnapshots;
use crate::services::stats::SERIES_DAY_MS;
use crate::services::store::{Store, now_ms};

const DEFAULT_QUOTA_RANGE: &str = "7d";

#[derive(Debug, Deserialize, Default)]
pub struct UsageQuery {
    pub profile: Option<String>,
}

/// Query of `GET /api/copilot/usage/history`.
#[derive(Debug, Deserialize, Default)]
pub struct QuotaHistoryQuery {
    /// `7d` (default) or `30d`.
    pub range: Option<String>,
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
    let Some(profile) = selected.filter(|p| p.copilot.is_some()) else {
        let message = query.profile.map_or_else(
            || "no enabled profile uses Copilot auth".to_string(),
            |name| format!("no Copilot profile named {name}"),
        );
        return AppError::NotFound(message).into_response();
    };
    let Some(key) = UsageKey::for_profile(&config, &profile.name) else {
        return AppError::Internal(format!("profile {} has no usage source", profile.name))
            .into_response();
    };
    crate::routes::profile_usage::respond(&state, &profile.name, &key).await
}

/// `GET /api/copilot/usage/history?range=7d|30d`: every stored quota reading in the range, and the daily
/// burn computed from them. Needs the sqlite store.
pub async fn usage_history(
    State(state): State<AppState>,
    Query(query): Query<QuotaHistoryQuery>,
) -> Result<Json<QuotaHistory>, AppError> {
    let range = query
        .range
        .unwrap_or_else(|| DEFAULT_QUOTA_RANGE.to_owned());
    let days = match range.as_str() {
        "7d" => 7,
        "30d" => 30,
        other => {
            return Err(AppError::BadRequest(format!(
                "range must be 7d or 30d, got {other}"
            )));
        }
    };
    let since_ms = now_ms().saturating_sub(days * SERIES_DAY_MS);
    let rows: Vec<QuotaSnapshotRow> = tokio::task::spawn_blocking(move || {
        let store = state
            .store()
            .filter(|store| matches!(store, Store::Sqlite(_)))
            .ok_or_else(|| {
                AppError::BadRequest(
                    "quota history needs persistence on the sqlite store (LLM_HUB_PERSISTENT=true, LLM_HUB_STORE=sqlite)"
                        .into(),
                )
            })?;
        QuotaSnapshots::history(store, since_ms).map_err(AppError::Internal)
    })
    .await
    .map_err(|e| AppError::Internal(format!("quota history task failed: {e}")))??;
    let daily = QuotaSnapshots::daily_deltas(&rows);
    Ok(Json(QuotaHistory { range, rows, daily }))
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;
    use std::sync::Arc;

    use axum::http::StatusCode;
    use serde_json::{Value, json};
    use wiremock::matchers::{method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    use super::*;
    use crate::configs::HubConfig;
    use crate::services::fake_gh_cli::FakeGhCli;
    use crate::services::gh_cli::GhOutcome;
    use crate::services::memory_credential_store::MemoryCredentialStore;

    fn state(vars: &[(&str, &str)]) -> AppState {
        state_with(vars, None, GhOutcome::Token("gho_from_gh".to_string()))
    }

    fn state_with(vars: &[(&str, &str)], store: Option<Store>, gh: GhOutcome) -> AppState {
        let vars: HashMap<String, String> = vars
            .iter()
            .map(|(key, value)| ((*key).to_string(), (*value).to_string()))
            .collect();
        AppState::with_seams(
            HubConfig::from_map(&vars).unwrap(),
            store,
            Arc::new(MemoryCredentialStore::default()),
            Arc::new(FakeGhCli::new(gh)),
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

    const HOUR_MS: u64 = 3_600_000;

    fn state_with_store(store: Store) -> AppState {
        state_with(
            &[
                ("LLM_HUB_PROFILES", "copilot"),
                ("LLM_HUB_COPILOT_BASE_URL", "http://127.0.0.1:9"),
            ],
            Some(store),
            GhOutcome::NotInstalled,
        )
    }

    fn reading(ts_ms: u64, used: f64) -> QuotaSnapshotRow {
        QuotaSnapshotRow {
            ts_ms,
            profile: "copilot".into(),
            quota: "premium_interactions".into(),
            used,
            entitlement: 300.0,
            used_usd: None,
            limit_usd: None,
        }
    }

    fn query(range: &str) -> Query<QuotaHistoryQuery> {
        Query(QuotaHistoryQuery {
            range: Some(range.into()),
        })
    }

    #[tokio::test]
    async fn history_bounds_rows_by_range_and_sums_the_burn() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("quota.db");
        let store = Store::open("sqlite", Some(path.to_str().unwrap())).unwrap();
        let now = now_ms();
        QuotaSnapshots::save(
            &store,
            &[
                reading(now - 8 * SERIES_DAY_MS, 1.0),
                reading(now - 2 * HOUR_MS, 10.0),
                reading(now - HOUR_MS, 13.0),
            ],
        )
        .unwrap();
        let state = state_with_store(store);

        let Json(week) = usage_history(State(state.clone()), query("7d"))
            .await
            .expect("7d is a valid range");
        assert_eq!(week.range, "7d");
        assert_eq!(week.rows.len(), 2);
        let week_burn: f64 = week.daily.iter().map(|day| day.used_delta).sum();
        assert!((week_burn - 3.0).abs() < 1e-9);

        let Json(month) = usage_history(State(state), query("30d"))
            .await
            .expect("30d is a valid range");
        assert_eq!(month.rows.len(), 3);
        let month_burn: f64 = month.daily.iter().map(|day| day.used_delta).sum();
        assert!((month_burn - 12.0).abs() < 1e-9);
    }

    #[tokio::test]
    async fn unknown_range_and_json_store_are_bad_requests() {
        let dir = tempfile::tempdir().unwrap();
        let sqlite =
            Store::open("sqlite", Some(dir.path().join("q.db").to_str().unwrap())).unwrap();
        let state = state_with_store(sqlite);
        let unknown = usage_history(State(state), query("2y")).await;
        assert!(matches!(unknown, Err(AppError::BadRequest(_))));

        let json = Store::open(
            "json",
            Some(dir.path().join("stats.json").to_str().unwrap()),
        )
        .unwrap();
        let state = state_with_store(json);
        let refused = usage_history(State(state), query("7d")).await;
        assert!(matches!(refused, Err(AppError::BadRequest(_))));
    }
}
