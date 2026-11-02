use axum::Json;
use axum::extract::{Query, State};
use axum::http::HeaderMap;
use serde_json::{Value, json};

use crate::dependencies::state::AppState;
use crate::routes::models::{ProfilesQuery, enabled_models};
use crate::schemas::launch::profile_filter::ProfileFilter;
use crate::services::model_picker::sort_models;

/// `claude --settings` input listing every hub model, since gateway discovery keeps only ids
/// with `claude` or `anthropic`. Auto models first, then by request count; `?profiles=a,b` limits it.
pub async fn settings(
    State(state): State<AppState>,
    Query(query): Query<ProfilesQuery>,
    headers: HeaderMap,
) -> Json<Value> {
    let mut payload = enabled_models(&state).await;
    ProfileFilter::from_request(query.profiles.as_deref(), &headers).retain_models(&mut payload);
    sort_models(&mut payload, &state);
    let models: Vec<&Value> = payload
        .get("data")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .collect();
    let options: Vec<Value> = models.into_iter().filter_map(picker_option).collect();
    Json(json!({ "modelPicker": { "replaceBuiltInOptions": true, "options": options } }))
}

fn picker_option(model: &Value) -> Option<Value> {
    let id = model.get("id").and_then(Value::as_str)?;
    let name = model
        .get("display_name")
        .and_then(Value::as_str)
        .unwrap_or(id);
    let label = if model.get("recommended").and_then(Value::as_bool) == Some(true) {
        format!("{name} (recommended)")
    } else {
        name.to_string()
    };
    let mut option = json!({ "model": id, "label": label });
    if let Some(description) = model.get("description").and_then(Value::as_str) {
        option["description"] = json!(description);
    }
    Some(option)
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

    fn state_from(pairs: &[(&str, &str)]) -> AppState {
        let vars: HashMap<String, String> = pairs
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

    fn picker_models(settings: &Value) -> Vec<&str> {
        settings["modelPicker"]["options"]
            .as_array()
            .unwrap()
            .iter()
            .filter_map(|option| option["model"].as_str())
            .collect()
    }

    fn three_profile_state() -> AppState {
        let state = state_from(&[
            ("LLM_HUB_PROFILES", "copilot,ivrit,local"),
            ("LLM_HUB_COPILOT_BASE_URL", "http://127.0.0.1:9"),
            ("LLM_HUB_IVRIT_BASE_URL", "http://127.0.0.1:9"),
            ("LLM_HUB_LOCAL_BASE_URL", "http://127.0.0.1:9"),
        ]);
        state.set_model_cache(json!({"object": "list", "data": [
            {"id": "ivrit/whisper", "owned_by": "ivrit"},
            {"id": "local/qwen", "owned_by": "local"},
            {"id": "copilot/gpt-4.1", "owned_by": "copilot"},
            {"id": "copilot/auto", "owned_by": "copilot"},
        ]}));
        state
    }

    #[tokio::test]
    async fn profiles_query_limits_settings_and_keeps_auto_first() {
        let query = ProfilesQuery {
            profiles: Some("copilot,ivrit".to_string()),
        };

        let Json(settings) =
            super::settings(State(three_profile_state()), Query(query), HeaderMap::new()).await;

        assert_eq!(
            picker_models(&settings),
            ["copilot/auto", "copilot/gpt-4.1", "ivrit/whisper"]
        );
    }

    #[tokio::test]
    async fn profiles_header_limits_settings() {
        let mut headers = HeaderMap::new();
        headers.insert(ProfileFilter::HEADER, "local".parse().unwrap());

        let Json(settings) = super::settings(
            State(three_profile_state()),
            Query(ProfilesQuery::default()),
            headers,
        )
        .await;

        assert_eq!(picker_models(&settings), ["local/qwen"]);
    }

    #[tokio::test]
    async fn settings_lists_every_enabled_model_including_non_claude() {
        let state = state_from(&[
            ("LLM_HUB_PROFILES", "copilot,off"),
            ("LLM_HUB_COPILOT_BASE_URL", "http://127.0.0.1:9"),
            ("LLM_HUB_OFF_BASE_URL", "http://127.0.0.1:9"),
            ("LLM_HUB_OFF_ENABLED", "false"),
        ]);
        state.set_model_cache(json!({"object": "list", "data": [
            {
                "id": "copilot/claude-opus-4-5",
                "owned_by": "copilot",
                "display_name": "claude-opus-4-5 · copilot",
                "description": "via llm-hub profile copilot",
            },
            {"id": "copilot/gpt-4.1", "owned_by": "copilot"},
            {"id": "off/hidden", "owned_by": "off"},
        ]}));

        let Json(settings) = super::settings(
            State(state),
            Query(ProfilesQuery::default()),
            HeaderMap::new(),
        )
        .await;

        assert_eq!(
            settings,
            json!({"modelPicker": {"replaceBuiltInOptions": true, "options": [
                {
                    "model": "copilot/claude-opus-4-5",
                    "label": "claude-opus-4-5 · copilot",
                    "description": "via llm-hub profile copilot",
                },
                {"model": "copilot/gpt-4.1", "label": "copilot/gpt-4.1"},
            ]}})
        );
    }

    #[tokio::test]
    async fn recommended_smart_auto_balance_is_first_and_marked() {
        let state = state_from(&[
            ("LLM_HUB_PROFILES", "copilot"),
            ("LLM_HUB_COPILOT_BASE_URL", "http://127.0.0.1:9"),
        ]);
        state.set_model_cache(json!({"data": [
            {"id": "copilot/gpt-4.1", "owned_by": "copilot"},
            {
                "id": "hub/smart-auto-intelligence",
                "owned_by": "hub",
                "display_name": "Smart Auto (Intelligence)",
                "recommended": false,
            },
            {
                "id": "hub/smart-auto-balance",
                "owned_by": "hub",
                "display_name": "Smart Auto (Balance)",
                "recommended": true,
            },
        ]}));

        let Json(settings) = super::settings(
            State(state),
            Query(ProfilesQuery::default()),
            HeaderMap::new(),
        )
        .await;

        let options = settings["modelPicker"]["options"].as_array().unwrap();
        assert_eq!(options[0]["model"], "hub/smart-auto-balance");
        assert_eq!(options[0]["label"], "Smart Auto (Balance) (recommended)");
        assert!(options.iter().any(|option| {
            option["model"] == "hub/smart-auto-intelligence"
                && option["label"] == "Smart Auto (Intelligence)"
        }));
    }

    #[tokio::test]
    async fn picker_reads_cached_counts_and_preserves_context_ids() {
        let state = state_from(&[
            ("LLM_HUB_PROFILES", "copilot"),
            ("LLM_HUB_COPILOT_BASE_URL", "http://127.0.0.1:9"),
        ]);
        state.set_model_cache(json!({"data": [
            {"id": "copilot/auto-intelligence", "owned_by": "copilot"},
            {"id": "copilot/auto", "owned_by": "copilot"},
            {"id": "copilot/opus[1m]", "owned_by": "copilot"},
            {"id": "copilot/unused", "owned_by": "copilot"},
        ]}));
        let outcome = crate::services::stats::RequestOutcome {
            profile: "copilot".into(),
            model_key: "copilot/opus".into(),
            status: 200,
            ts_ms: 0,
            latency_ms: 1,
            ttft_ms: None,
            tokens_in: 0,
            tokens_out: 0,
            cache_read_tokens: 0,
            cache_write_tokens: 0,
            error: None,
            client: None,
        };
        state.stats().record(&outcome);
        let Json(settings) = super::settings(
            State(state),
            Query(ProfilesQuery::default()),
            HeaderMap::new(),
        )
        .await;
        assert_eq!(settings["modelPicker"]["replaceBuiltInOptions"], true);
        assert_eq!(
            picker_models(&settings),
            [
                "copilot/auto",
                "copilot/auto-intelligence",
                "copilot/opus[1m]",
                "copilot/unused"
            ]
        );
    }
}
