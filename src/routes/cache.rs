use axum::Json;
use axum::extract::{Query, State};
use serde::Deserialize;
use utoipa::IntoParams;

use crate::dependencies::state::AppState;
use crate::schemas::app_error::AppError;
use crate::schemas::error_body::ErrorBody;
use crate::services::cache_stats::CacheStats;
use crate::services::store::{Store, now_ms};

#[derive(Debug, Deserialize, Default, IntoParams)]
#[into_params(parameter_in = Query)]
pub struct CacheQuery {
    #[serde(default)]
    pub range: Option<String>,
}

#[utoipa::path(get, path = "/api/cache", operation_id = "cache_analysis", tag = "stats", summary = "Prompt-cache analysis", params(CacheQuery), responses((status = 200, description = "Prompt-cache hit and miss analysis", body = Object), (status = 400, description = "Invalid range.", body = ErrorBody), (status = 500, description = "Internal error.", body = ErrorBody), (status = 401, description = "Missing or invalid hub key.", body = ErrorBody)))]
pub async fn list(
    State(state): State<AppState>,
    Query(query): Query<CacheQuery>,
) -> Result<Json<CacheStats>, AppError> {
    let range = query.range.unwrap_or_else(|| "1h".to_owned());
    let now = now_ms();
    let state_now = state.clone();
    let result = tokio::task::spawn_blocking(move || {
        let store = match state_now.store() {
            Some(store) if matches!(store, Store::Sqlite(_)) => store,
            _ => return Err(AppError::BadRequest(
                "prompt-cache analysis needs persistence on the sqlite store (LLM_HUB_PERSISTENT=true, LLM_HUB_STORE=sqlite)".into(),
            )),
        };
        CacheStats::read(store, &state_now.pricing_book(), now, &range)
            .map_err(AppError::BadRequest)
    })
    .await
    .map_err(|error| AppError::Internal(format!("cache read task failed: {error}")))??;
    Ok(Json(result))
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;
    use std::sync::Arc;

    use super::*;
    use crate::configs::HubConfig;
    use crate::services::fake_gh_cli::FakeGhCli;
    use crate::services::gh_cli::GhOutcome;
    use crate::services::memory_credential_store::MemoryCredentialStore;
    use crate::services::stats::RequestOutcome;

    fn state_with_store(store: Store) -> AppState {
        let vars: HashMap<String, String> = [
            ("LLM_HUB_PROFILES", "demo"),
            ("LLM_HUB_DEMO_BASE_URL", "http://127.0.0.1:9"),
        ]
        .into_iter()
        .map(|(key, value)| (key.to_owned(), value.to_owned()))
        .collect();
        AppState::with_seams(
            HubConfig::from_map(&vars).unwrap(),
            Some(store),
            Arc::new(MemoryCredentialStore::default()),
            Arc::new(FakeGhCli::new(GhOutcome::NotInstalled)),
        )
        .unwrap()
    }

    #[tokio::test]
    async fn range_one_hour_and_one_day_return_cache_shape() {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::open(
            "sqlite",
            Some(dir.path().join("cache-route.db").to_str().unwrap()),
        )
        .unwrap();
        store
            .record(&RequestOutcome {
                profile: "demo".to_owned(),
                model_key: "demo/model".to_owned(),
                status: 200,
                ts_ms: now_ms(),
                latency_ms: 1,
                ttft_ms: None,
                tokens_in: 100_000,
                tokens_out: 1,
                cache_read_tokens: 10_000,
                cache_write_tokens: 0,
                error: None,
                client: Some("claude".to_owned()),
                session_id: None,
                worker: Some("worker-a".to_owned()),
                routed_by: None,
                attempt: None,
                role: None,
            })
            .unwrap();
        let state = state_with_store(store);

        let hour = list(
            State(state.clone()),
            Query(CacheQuery {
                range: Some("1h".to_owned()),
            }),
        )
        .await
        .unwrap();
        assert_eq!(hour.range, "1h");
        assert_eq!(hour.requests, 1);
        assert_eq!(hour.big_misses, 1);

        let day = list(
            State(state),
            Query(CacheQuery {
                range: Some("1d".to_owned()),
            }),
        )
        .await
        .unwrap();
        assert_eq!(day.range, "1d");
        assert_eq!(day.requests, 1);
    }

    #[tokio::test]
    async fn invalid_range_is_bad_request() {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::open(
            "sqlite",
            Some(dir.path().join("cache-invalid.db").to_str().unwrap()),
        )
        .unwrap();
        let state = state_with_store(store);
        let result = list(
            State(state),
            Query(CacheQuery {
                range: Some("7d".to_owned()),
            }),
        )
        .await;
        assert!(matches!(result, Err(AppError::BadRequest(_))));
    }
}
