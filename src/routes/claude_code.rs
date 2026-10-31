use std::collections::HashMap;

use axum::Json;
use axum::extract::{Query, State};
use axum::http::HeaderMap;
use serde_json::{Value, json};

use crate::dependencies::state::AppState;
use crate::routes::models::{ProfilesQuery, enabled_models};
use crate::schemas::launch::profile_filter::ProfileFilter;
use crate::services::store::{StatsFilter, now_ms};

const USAGE_WINDOW_MS: u64 = 30 * 86_400_000;
const USAGE_RANGE_LABEL: &str = "30d";

/// `claude --settings` input listing every hub model, since gateway
/// discovery only keeps ids that mention `claude` or `anthropic`.
/// Auto models come first, then the rest by request count, most used first.
/// `?profiles=a,b` or the `x-llm-hub-profiles` header limits it to those profiles.
pub async fn settings(
    State(state): State<AppState>,
    Query(query): Query<ProfilesQuery>,
    headers: HeaderMap,
) -> Json<Value> {
    let mut payload = enabled_models(&state).await;
    ProfileFilter::from_request(query.profiles.as_deref(), &headers).retain_models(&mut payload);
    let usage = model_usage(&state);
    let mut models: Vec<&Value> = payload
        .get("data")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .collect();
    models.sort_by_key(|model| picker_rank(model, &usage));
    let options: Vec<Value> = models.into_iter().filter_map(picker_option).collect();
    Json(json!({ "modelPicker": { "options": options } }))
}

/// Requests per qualified model id: the stored last 30 days when the hub
/// persists stats, otherwise the counts since this process started.
fn model_usage(state: &AppState) -> HashMap<String, u64> {
    let stored = state.store().and_then(|store| {
        store
            .stats(&StatsFilter {
                since_ms: Some(now_ms().saturating_sub(USAGE_WINDOW_MS)),
                profile: None,
                model: None,
                range_label: USAGE_RANGE_LABEL.to_string(),
            })
            .ok()
    });
    let snapshot = stored.unwrap_or_else(|| state.stats().snapshot("live", false));
    snapshot
        .models
        .into_iter()
        .map(|row| (row.key, row.requests))
        .collect()
}

/// Sort key: Auto models first, then the most-used models. The sort is stable,
/// so equal keys keep the listing order.
fn picker_rank(model: &Value, usage: &HashMap<String, u64>) -> (bool, std::cmp::Reverse<u64>) {
    let id = model.get("id").and_then(Value::as_str).unwrap_or_default();
    let is_auto = id
        .split_once('/')
        .is_some_and(|(_, bare)| bare == "auto" || bare.starts_with("auto-"));
    let requests = usage.get(id).copied().unwrap_or(0);
    (
        !is_auto,
        std::cmp::Reverse(if is_auto { 0 } else { requests }),
    )
}

fn picker_option(model: &Value) -> Option<Value> {
    let id = model.get("id").and_then(Value::as_str)?;
    let label = model
        .get("display_name")
        .and_then(Value::as_str)
        .unwrap_or(id);
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
            ["copilot/auto", "ivrit/whisper", "copilot/gpt-4.1"]
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
            json!({"modelPicker": {"options": [
                {
                    "model": "copilot/claude-opus-4-5",
                    "label": "claude-opus-4-5 · copilot",
                    "description": "via llm-hub profile copilot",
                },
                {"model": "copilot/gpt-4.1", "label": "copilot/gpt-4.1"},
            ]}})
        );
    }

    #[test]
    fn picker_puts_auto_first_then_most_used() {
        let models = [
            json!({"id": "copilot/gpt-6-luna"}),
            json!({"id": "copilot/claude-opus-5-5"}),
            json!({"id": "copilot/auto"}),
            json!({"id": "local/qwen"}),
            json!({"id": "copilot/auto-intelligence"}),
        ];
        let usage = HashMap::from([
            ("copilot/claude-opus-5-5".to_string(), 40),
            ("local/qwen".to_string(), 7),
        ]);
        let mut ordered: Vec<&Value> = models.iter().collect();

        ordered.sort_by_key(|model| picker_rank(model, &usage));

        let ids: Vec<&str> = ordered.iter().filter_map(|m| m["id"].as_str()).collect();
        assert_eq!(
            ids,
            [
                "copilot/auto",
                "copilot/auto-intelligence",
                "copilot/claude-opus-5-5",
                "local/qwen",
                "copilot/gpt-6-luna"
            ]
        );
    }
}
