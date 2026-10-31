use axum::Json;
use axum::extract::State;
use serde_json::{Value, json};

use crate::dependencies::state::AppState;
use crate::routes::models::enabled_models;

/// `claude --settings` input listing every hub model, since gateway
/// discovery only keeps ids that mention `claude` or `anthropic`.
pub async fn settings(State(state): State<AppState>) -> Json<Value> {
    let payload = enabled_models(&state).await;
    let options: Vec<Value> = payload
        .get("data")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(picker_option)
        .collect();
    Json(json!({ "modelPicker": { "options": options } }))
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

    #[tokio::test]
    async fn settings_lists_every_enabled_model_including_non_claude() {
        let vars: HashMap<String, String> = [
            ("LLM_HUB_PROFILES", "copilot,off"),
            ("LLM_HUB_COPILOT_BASE_URL", "http://127.0.0.1:9"),
            ("LLM_HUB_OFF_BASE_URL", "http://127.0.0.1:9"),
            ("LLM_HUB_OFF_ENABLED", "false"),
        ]
        .into_iter()
        .map(|(key, value)| (key.to_string(), value.to_string()))
        .collect();
        let state = AppState::with_seams(
            HubConfig::from_map(&vars).unwrap(),
            None,
            Arc::new(MemoryCredentialStore::default()),
            Arc::new(FakeGhCli::new(GhOutcome::NotInstalled)),
        )
        .unwrap();
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

        let Json(settings) = super::settings(State(state)).await;

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
}
