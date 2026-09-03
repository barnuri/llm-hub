use axum::Json;
use axum::extract::State;
use futures_util::future::join_all;
use serde_json::{Value, json};

use crate::configs::ProfileConfig;
use crate::consts::{MODELS_CACHE_TTL, MODELS_FANOUT_TIMEOUT};
use crate::dependencies::state::AppState;
use crate::schemas::model_context::{
    advertised_model_id, inferred_max_input_tokens, max_input_tokens_from_item,
};

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
    let upstream_tokens = max_input_tokens_from_item(item);
    let qualified = format!("{}/{}", profile.name, raw_id);
    let advertised = advertised_model_id(&qualified, upstream_tokens);
    if let Some(obj) = model.as_object_mut() {
        obj.insert("id".into(), json!(advertised.clone()));
        obj.insert("owned_by".into(), json!(profile.name));
        annotate_context(obj, &advertised, upstream_tokens);
    }
    model
}

fn static_models(profile: &ProfileConfig) -> Vec<Value> {
    profile
        .static_models
        .iter()
        .map(|id| {
            let qualified = format!("{}/{}", profile.name, id);
            let advertised = advertised_model_id(&qualified, None);
            let mut model = json!({
                "id": advertised.clone(),
                "object": "model",
                "owned_by": profile.name,
            });
            if let Some(obj) = model.as_object_mut() {
                annotate_context(obj, &advertised, None);
            }
            model
        })
        .collect()
}

fn annotate_context(
    obj: &mut serde_json::Map<String, Value>,
    advertised_id: &str,
    upstream_tokens: Option<u64>,
) {
    let Some(tokens) = inferred_max_input_tokens(advertised_id, upstream_tokens) else {
        return;
    };
    obj.insert("max_input_tokens".into(), json!(tokens));
    obj.insert("context_length".into(), json!(tokens));
    obj.insert("context_window".into(), json!(tokens));
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::configs::{ProfileConfig, TokenRates};
    use crate::consts::CONTEXT_TOKENS_1M;
    use std::collections::HashMap;

    fn profile(name: &str, static_ids: &[&str]) -> ProfileConfig {
        ProfileConfig {
            name: name.to_string(),
            display_name: None,
            base_url: String::new(),
            api_key: String::new(),
            extra_headers: Vec::new(),
            timeout_ms: None,
            enabled: true,
            static_models: static_ids.iter().map(|id| (*id).to_string()).collect(),
            pricing: TokenRates::default(),
            model_prices: HashMap::new(),
        }
    }

    #[test]
    fn rewrite_claude_opus__advertises_1m_id_and_tokens() {
        let item = json!({
            "id": "bedrock/anthropic.claude-opus-5",
            "object": "model",
            "max_input_tokens": CONTEXT_TOKENS_1M,
            "context_length": CONTEXT_TOKENS_1M,
        });
        let out = rewrite_model(&profile("llmgw", &[]), &item);
        assert_eq!(out["id"], "llmgw/bedrock/anthropic.claude-opus-5[1m]");
        assert_eq!(out["max_input_tokens"], CONTEXT_TOKENS_1M);
        assert_eq!(out["context_window"], CONTEXT_TOKENS_1M);
    }

    #[test]
    fn rewrite_claude_opus_200k_upstream__keeps_plain_id() {
        let item = json!({
            "id": "bedrock/anthropic.claude-opus-5",
            "object": "model",
            "max_input_tokens": 200_000,
        });
        let out = rewrite_model(&profile("llmgw", &[]), &item);
        assert_eq!(out["id"], "llmgw/bedrock/anthropic.claude-opus-5");
        assert_eq!(out["max_input_tokens"], 200_000);
    }

    #[test]
    fn rewrite_local_model__keeps_plain_id() {
        let item = json!({"id": "meta/muse-glimmer", "object": "model"});
        let out = rewrite_model(&profile("llama_swap", &[]), &item);
        assert_eq!(out["id"], "llama_swap/meta/muse-glimmer");
        assert!(out.get("max_input_tokens").is_none());
    }

    #[test]
    fn static_claude_sonnet__no_tokens_means_no_1m_suffix() {
        let models = static_models(&profile("llmgw", &["bedrock/anthropic.claude-sonnet-5"]));
        assert_eq!(models[0]["id"], "llmgw/bedrock/anthropic.claude-sonnet-5");
        assert!(models[0].get("max_input_tokens").is_none());
    }
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
