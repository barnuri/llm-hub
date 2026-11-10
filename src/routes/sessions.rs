use axum::Json;
use axum::extract::{Path, Query, State};
use serde::Deserialize;

use crate::dependencies::state::AppState;
use crate::routes::admin::since_for_range;
use crate::schemas::app_error::AppError;
use crate::schemas::session_detail::SessionDetail;
use crate::schemas::session_list::SessionList;
use crate::services::pricing::PricingBook;
use crate::services::session_insights;
use crate::services::store::{Store, now_ms};

const DEFAULT_SESSIONS_RANGE: &str = "7d";

#[derive(Debug, Deserialize, Default)]
pub struct SessionsQuery {
    /// `today`, `1d`, `7d` (default), `30d`, or `all`.
    #[serde(default)]
    pub range: Option<String>,
}

pub async fn list(
    State(state): State<AppState>,
    Query(query): Query<SessionsQuery>,
) -> Result<Json<SessionList>, AppError> {
    let range = query
        .range
        .unwrap_or_else(|| DEFAULT_SESSIONS_RANGE.to_owned());
    let since_ms = since_for_range(&range, now_ms())?;
    let list = read_sessions(state, move |store, book| {
        session_insights::list_sessions(store, &range, since_ms, book)
    })
    .await?;
    Ok(Json(list))
}

pub async fn detail(
    State(state): State<AppState>,
    Path(worker): Path<String>,
    Query(query): Query<SessionsQuery>,
) -> Result<Json<SessionDetail>, AppError> {
    let range = query
        .range
        .unwrap_or_else(|| DEFAULT_SESSIONS_RANGE.to_owned());
    let since_ms = since_for_range(&range, now_ms())?;
    let wanted = worker.clone();
    let detail = read_sessions(state, move |store, book| {
        session_insights::session_detail(store, &wanted, since_ms, book)
    })
    .await?;
    detail.map(Json).ok_or_else(|| {
        AppError::NotFound(format!(
            "no requests for session '{worker}' in range '{range}'"
        ))
    })
}

/// Runs a session read on the blocking pool, because it scans the store under its connection lock.
async fn read_sessions<T, F>(state: AppState, read: F) -> Result<T, AppError>
where
    T: Send + 'static,
    F: FnOnce(&Store, &PricingBook) -> Result<T, String> + Send + 'static,
{
    tokio::task::spawn_blocking(move || {
        let store = sqlite_store(&state)?;
        let book = state.pricing_book();
        read(store, &book).map_err(AppError::Internal)
    })
    .await
    .map_err(|e| AppError::Internal(format!("session read task failed: {e}")))?
}

fn sqlite_store(state: &AppState) -> Result<&Store, AppError> {
    match state.store() {
        Some(store) if matches!(store, Store::Sqlite(_)) => Ok(store),
        _ => Err(AppError::BadRequest(
            "session insights need persistence on the sqlite store (LLM_HUB_PERSISTENT=true, LLM_HUB_STORE=sqlite)".into(),
        )),
    }
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

    fn state_with_store(store: Store) -> AppState {
        let vars: HashMap<String, String> = [
            ("LLM_HUB_PROFILES", "copilot"),
            ("LLM_HUB_COPILOT_BASE_URL", "http://127.0.0.1:9"),
        ]
        .into_iter()
        .map(|(key, value)| (key.to_string(), value.to_string()))
        .collect();
        AppState::with_seams(
            HubConfig::from_map(&vars).unwrap(),
            Some(store),
            Arc::new(MemoryCredentialStore::default()),
            Arc::new(FakeGhCli::new(GhOutcome::NotInstalled)),
        )
        .unwrap()
    }

    fn temp_sqlite_store(dir: &tempfile::TempDir) -> Store {
        let path = dir.path().join("sessions.db");
        Store::open("sqlite", Some(path.to_str().unwrap())).unwrap()
    }

    #[tokio::test]
    async fn unknown_range_bad_request() {
        let dir = tempfile::tempdir().unwrap();
        let state = state_with_store(temp_sqlite_store(&dir));
        let query = || SessionsQuery {
            range: Some("2y".into()),
        };
        let listed = list(State(state.clone()), Query(query())).await;
        let detailed = detail(State(state), Path("demo-worker".into()), Query(query())).await;
        assert!(matches!(listed, Err(AppError::BadRequest(_))));
        assert!(matches!(detailed, Err(AppError::BadRequest(_))));
    }

    #[tokio::test]
    async fn json_store_session_routes_bad_request() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("stats.json");
        let store = Store::open("json", Some(path.to_str().unwrap())).unwrap();
        let state = state_with_store(store);
        let listed = list(State(state.clone()), Query(SessionsQuery::default())).await;
        let detailed = detail(
            State(state),
            Path("demo-worker".into()),
            Query(SessionsQuery::default()),
        )
        .await;
        assert!(matches!(listed, Err(AppError::BadRequest(_))));
        assert!(matches!(detailed, Err(AppError::BadRequest(_))));
    }

    #[tokio::test]
    async fn unknown_worker_not_found() {
        let dir = tempfile::tempdir().unwrap();
        let state = state_with_store(temp_sqlite_store(&dir));
        let detailed = detail(
            State(state),
            Path("missing-worker".into()),
            Query(SessionsQuery::default()),
        )
        .await;
        assert!(matches!(detailed, Err(AppError::NotFound(_))));
    }
}
