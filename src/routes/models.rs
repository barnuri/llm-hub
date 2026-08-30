use axum::Json;
use axum::extract::State;
use futures_util::future::join_all;
use serde_json::{Value, json};

use crate::configs::ProfileConfig;
use crate::consts::{MODELS_CACHE_TTL, MODELS_FANOUT_TIMEOUT};
use crate::dependencies::state::AppState;

pub async fn list_models(State(state): State<AppState>) -> Json<Value> {
    if let Some(cached) = state.model_cache()
        && cached.fetched_at.elapsed() < MODELS_CACHE_TTL
    {
        return Json(cached.payload.clone());
    }
    Json(refresh_models(&state).await)
}

/// Fans out to every enabled upstream, rewrites ids to `<profile>/<id>`,
/// merges. A dead upstream contributes nothing (or its static list) and a
/// warning — never a 502.
pub async fn refresh_models(state: &AppState) -> Value {
    let config = state.config();
    let enabled: Vec<&ProfileConfig> = config.profiles.iter().filter(|p| p.enabled).collect();

    let fetches = enabled
        .iter()
        .map(|profile| fetch_profile_models(state, profile));
    let per_profile = join_all(fetches).await;

    let mut merged = Vec::new();
    for (profile, result) in enabled.iter().zip(per_profile) {
        match result {
            Ok(models) => {
                state.set_upstream_health(&profile.name, true);
                merged.extend(models);
            }
            Err(reason) => {
                state.set_upstream_health(&profile.name, false);
                tracing::warn!(profile = %profile.name, "models fetch failed: {reason}");
                merged.extend(static_models(profile));
            }
        }
    }

    let payload = json!({ "object": "list", "data": merged });
    state.set_model_cache(payload.clone());
    payload
}

async fn fetch_profile_models(
    state: &AppState,
    profile: &ProfileConfig,
) -> Result<Vec<Value>, String> {
    let client = state.client(&profile.name).ok_or("no client for profile")?;
    let mut request = client.get(format!("{}/models", profile.base_url));
    if !profile.api_key.is_empty() {
        request = request.bearer_auth(&profile.api_key);
    }
    for (name, value) in &profile.extra_headers {
        request = request.header(name, value);
    }

    let response = tokio::time::timeout(MODELS_FANOUT_TIMEOUT, request.send())
        .await
        .map_err(|_| "timeout".to_string())?
        .map_err(|e| e.to_string())?;

    if !response.status().is_success() {
        return Err(format!("status {}", response.status()));
    }

    let body: Value = response.json().await.map_err(|e| e.to_string())?;
    let items = body
        .get("data")
        .and_then(Value::as_array)
        .ok_or("no data array in /models response")?;

    Ok(items
        .iter()
        .map(|item| rewrite_model(profile, item))
        .collect())
}

fn rewrite_model(profile: &ProfileConfig, item: &Value) -> Value {
    let mut model = item.clone();
    let raw_id = item.get("id").and_then(Value::as_str).unwrap_or_default();
    if let Some(obj) = model.as_object_mut() {
        obj.insert("id".into(), json!(format!("{}/{}", profile.name, raw_id)));
        obj.insert("owned_by".into(), json!(profile.name));
    }
    model
}

fn static_models(profile: &ProfileConfig) -> Vec<Value> {
    profile
        .static_models
        .iter()
        .map(|id| {
            json!({
                "id": format!("{}/{}", profile.name, id),
                "object": "model",
                "owned_by": profile.name,
            })
        })
        .collect()
}

/// Spawned from main: keeps the cache warm so client startups never fan out.
pub fn spawn_background_refresh(state: AppState) {
    tokio::spawn(async move {
        loop {
            refresh_models(&state).await;
            tokio::time::sleep(MODELS_CACHE_TTL).await;
        }
    });
}
