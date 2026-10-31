use std::collections::HashMap;

use axum::Json;
use axum::extract::State;
use futures_util::future::join_all;
use reqwest::header::HeaderMap;
use serde_json::{Value, json};

use crate::configs::ProfileConfig;
use crate::consts::{MODELS_CACHE_TTL, MODELS_FANOUT_TIMEOUT};
use crate::dependencies::state::AppState;
use crate::schemas::model_context::{
    advertised_model_id, inferred_max_input_tokens, max_input_tokens_from_item,
};
use crate::schemas::upstream_model::UpstreamModel;
use crate::services::upstream_auth;

pub(crate) const MODELS_PATH: &str = "/models";
/// The bare model id a Copilot profile advertises for Copilot Auto routing.
pub(crate) const COPILOT_AUTO_MODEL: &str = "auto";
const COPILOT_AUTO_ENDPOINTS: [&str; 3] = ["/chat/completions", "/responses", "/v1/messages"];
const COPILOT_POLICY_DISABLED: &str = "disabled";
const CLAUDE_ID_PREFIX: &str = "claude-";
const NATIVE_MESSAGES_ENDPOINT: &str = "/v1/messages";
const ANTHROPIC_VERSION_HEADER: &str = "anthropic-version";

pub async fn list_models(State(state): State<AppState>, headers: HeaderMap) -> Json<Value> {
    let mut payload = enabled_models(&state).await;
    if headers.contains_key(ANTHROPIC_VERSION_HEADER) {
        add_anthropic_list_fields(&mut payload);
    }
    Json(payload)
}

/// The cached model list, whatever its age, limited to enabled profiles.
/// The background loop keeps it fresh, so only an empty cache fans out.
pub async fn enabled_models(state: &AppState) -> Value {
    let mut payload = match state.model_cache() {
        Some(cached) => cached.payload.clone(),
        None => refresh_models(state).await,
    };
    let config = state.config();
    filter_enabled_models(&mut payload, &config.profiles);
    payload
}

/// Anthropic SDK clients read a model list as `{data, has_more, first_id, last_id}`
/// with `type: "model"` items. The `object` and `owned_by` fields stay for other clients.
fn add_anthropic_list_fields(payload: &mut Value) {
    let Some(obj) = payload.as_object_mut() else {
        return;
    };
    let mut first_id = Value::Null;
    let mut last_id = Value::Null;
    if let Some(models) = obj.get_mut("data").and_then(Value::as_array_mut) {
        for model in models.iter_mut() {
            if let Some(model) = model.as_object_mut() {
                model.entry("type").or_insert_with(|| json!("model"));
            }
        }
        first_id = models.first().map_or(Value::Null, |m| m["id"].clone());
        last_id = models.last().map_or(Value::Null, |m| m["id"].clone());
    }
    obj.insert("has_more".into(), json!(false));
    obj.insert("first_id".into(), first_id);
    obj.insert("last_id".into(), last_id);
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
    let body = tokio::time::timeout(
        MODELS_FANOUT_TIMEOUT,
        fetch_models_body(state, &client, profile),
    )
    .await
    .map_err(|_| "timeout".to_string())??;
    let items = body
        .get("data")
        .and_then(Value::as_array)
        .ok_or("no data array in /models response")?;

    let usable: Vec<&Value> = items
        .iter()
        .filter(|item| profile.copilot.is_none() || is_copilot_model_usable(item))
        .collect();
    if profile.copilot.is_none() {
        return Ok(usable
            .into_iter()
            .map(|item| rewrite_model(profile, item))
            .collect());
    }
    state.set_upstream_models(&profile.name, upstream_catalog(profile, &usable));
    state.set_copilot_catalog(&profile.name, usable.iter().copied().cloned().collect());
    let lists_auto = usable
        .iter()
        .any(|item| item.get("id").and_then(Value::as_str) == Some(COPILOT_AUTO_MODEL));
    let mut models: Vec<Value> = usable
        .into_iter()
        .map(|item| rewrite_model(profile, item))
        .collect();
    if !lists_auto {
        models.push(copilot_auto_model(profile));
    }
    Ok(models)
}

/// `<profile>/auto`: Copilot's router picks the model, on every wire API.
fn copilot_auto_model(profile: &ProfileConfig) -> Value {
    json!({
        "id": format!("{}/{COPILOT_AUTO_MODEL}", profile.name),
        "object": "model",
        "type": "model",
        "owned_by": profile.name,
        "display_name": format!("Auto · {}", profile.label()),
        "description": format!(
            "Copilot Auto picks the model per conversation, via llm-hub profile {}",
            profile.label()
        ),
        "supported_endpoints": COPILOT_AUTO_ENDPOINTS,
    })
}

/// Advertised bare id → the id upstream expects and whether it speaks `/v1/messages`.
fn upstream_catalog(profile: &ProfileConfig, items: &[&Value]) -> HashMap<String, UpstreamModel> {
    items
        .iter()
        .filter_map(|item| {
            let upstream_id = item.get("id").and_then(Value::as_str)?;
            let native_messages = item
                .get("supported_endpoints")
                .and_then(Value::as_array)
                .is_some_and(|endpoints| {
                    endpoints
                        .iter()
                        .any(|e| e.as_str() == Some(NATIVE_MESSAGES_ENDPOINT))
                });
            let entry = UpstreamModel {
                upstream_id: upstream_id.to_string(),
                native_messages,
            };
            Some((advertised_bare_id(profile, upstream_id), entry))
        })
        .collect()
}

/// Copilot lists Claude ids dotted (`claude-opus-4.5`); Claude Code recognises
/// only the dashed Anthropic form (`claude-opus-4-5`).
pub(crate) fn advertised_bare_id(profile: &ProfileConfig, raw_id: &str) -> String {
    if profile.copilot.is_none() || !raw_id.starts_with(CLAUDE_ID_PREFIX) {
        return raw_id.to_string();
    }
    let chars: Vec<char> = raw_id.chars().collect();
    chars
        .iter()
        .enumerate()
        .map(|(i, &c)| {
            let between_digits = i > 0
                && chars[i - 1].is_ascii_digit()
                && chars.get(i + 1).is_some_and(char::is_ascii_digit);
            if c == '.' && between_digits { '-' } else { c }
        })
        .collect()
}

async fn fetch_models_body(
    state: &AppState,
    client: &reqwest::Client,
    profile: &ProfileConfig,
) -> Result<Value, String> {
    let mut headers = HeaderMap::new();
    let (base, copilot_bearer) =
        upstream_auth::resolve(state, client, profile, MODELS_PATH, &[], &mut headers).await?;
    let mut request = client.get(format!("{base}{MODELS_PATH}")).headers(headers);
    for (name, value) in &profile.extra_headers {
        request = request.header(name, value);
    }

    let response = request.send().await.map_err(|e| e.to_string())?;
    let status = response.status();
    upstream_auth::invalidate_if_unauthorized(
        state,
        &profile.name,
        status,
        copilot_bearer.as_deref(),
    )
    .await;
    if !status.is_success() {
        return Err(format!("status {status}"));
    }
    response.json().await.map_err(|e| e.to_string())
}

/// Copilot lists models its own clients hide or the account's policy disables.
fn is_copilot_model_usable(item: &Value) -> bool {
    let picker_disabled = item.get("model_picker_enabled").and_then(Value::as_bool) == Some(false);
    let policy_disabled =
        item.pointer("/policy/state").and_then(Value::as_str) == Some(COPILOT_POLICY_DISABLED);
    !picker_disabled && !policy_disabled
}

fn rewrite_model(profile: &ProfileConfig, item: &Value) -> Value {
    let mut model = item.clone();
    let raw_id = item.get("id").and_then(Value::as_str).unwrap_or_default();
    let upstream_tokens = max_input_tokens_from_item(item);
    let bare = advertised_bare_id(profile, raw_id);
    let qualified = format!("{}/{}", profile.name, bare);
    let advertised = advertised_model_id(&qualified, upstream_tokens);
    if let Some(obj) = model.as_object_mut() {
        obj.insert("id".into(), json!(advertised.clone()));
        obj.insert("owned_by".into(), json!(profile.name));
        annotate_labels(obj, profile, &bare);
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
                annotate_labels(obj, profile, id);
                annotate_context(obj, &advertised, None);
            }
            model
        })
        .collect()
}

fn filter_enabled_models(payload: &mut Value, profiles: &[ProfileConfig]) {
    let Some(models) = payload.get_mut("data").and_then(Value::as_array_mut) else {
        return;
    };

    models.retain(|model| {
        let Some(owner) = model.get("owned_by").and_then(Value::as_str) else {
            return false;
        };
        profiles
            .iter()
            .any(|profile| profile.enabled && profile.name == owner)
    });
}

/// Claude Code's picker shows `display_name` when it differs from the id.
fn annotate_labels(obj: &mut serde_json::Map<String, Value>, profile: &ProfileConfig, bare: &str) {
    let label = profile.label();
    obj.insert("display_name".into(), json!(format!("{bare} · {label}")));
    obj.insert(
        "description".into(),
        json!(format!("via llm-hub profile {label}")),
    );
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

/// Spawned from main: keeps the cache warm so client startups never fan out.
pub fn spawn_background_refresh(state: AppState) {
    tokio::spawn(async move {
        loop {
            refresh_models(&state).await;
            tokio::time::sleep(MODELS_CACHE_TTL).await;
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::configs::{CopilotFlow, HubConfig, ProfileConfig, TokenRates};
    use crate::consts::CONTEXT_TOKENS_1M;
    use crate::services::fake_gh_cli::FakeGhCli;
    use crate::services::gh_cli::GhOutcome;
    use crate::services::memory_credential_store::MemoryCredentialStore;
    use std::collections::HashMap;
    use std::sync::Arc;
    use wiremock::matchers::{method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    const DEAD_BASE_URL: &str = "http://127.0.0.1:9";

    fn state_from(vars: &[(&str, &str)]) -> AppState {
        let vars: HashMap<String, String> = vars
            .iter()
            .map(|(key, value)| ((*key).to_string(), (*value).to_string()))
            .collect();
        AppState::with_seams(
            HubConfig::from_map(&vars).unwrap(),
            None,
            Arc::new(MemoryCredentialStore::default()),
            Arc::new(FakeGhCli::new(GhOutcome::Token("gho_test".to_string()))),
        )
        .unwrap()
    }

    fn copilot_state(github_api_url: &str) -> AppState {
        state_from(&[
            ("LLM_HUB_PROFILES", "copilot"),
            ("LLM_HUB_COPILOT_BASE_URL", DEAD_BASE_URL),
            ("LLM_HUB_COPILOT_AUTH", CopilotFlow::Opencode.auth_value()),
            ("LLM_HUB_COPILOT_GITHUB_API_URL", github_api_url),
        ])
    }

    fn copilot_profile() -> ProfileConfig {
        copilot_state(DEAD_BASE_URL).config().profiles[0].clone()
    }

    fn ids(payload: &Value) -> Vec<&str> {
        payload["data"]
            .as_array()
            .unwrap()
            .iter()
            .filter_map(|model| model["id"].as_str())
            .collect()
    }

    #[tokio::test]
    async fn cached_list_is_served_without_fan_out() {
        let upstream = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path(MODELS_PATH))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({"data": []})))
            .expect(0)
            .mount(&upstream)
            .await;
        let state = state_from(&[
            ("LLM_HUB_PROFILES", "proxy"),
            ("LLM_HUB_PROXY_BASE_URL", &upstream.uri()),
        ]);
        let cached = json!({"object": "list", "data": [{"id": "proxy/old", "owned_by": "proxy"}]});
        state.set_model_cache(cached);

        let Json(payload) = list_models(State(state), HeaderMap::new()).await;

        assert_eq!(ids(&payload), ["proxy/old"]);
    }

    #[tokio::test]
    async fn empty_cache_fans_out_once() {
        let upstream = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path(MODELS_PATH))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({"data": [{"id": "m"}]})))
            .expect(1)
            .mount(&upstream)
            .await;
        let state = state_from(&[
            ("LLM_HUB_PROFILES", "proxy"),
            ("LLM_HUB_PROXY_BASE_URL", &upstream.uri()),
        ]);

        let Json(payload) = list_models(State(state), HeaderMap::new()).await;

        assert_eq!(ids(&payload), ["proxy/m"]);
    }

    #[test]
    fn copilot_claude_ids_dash_only_digit_dot_digit() {
        let copilot = copilot_profile();
        assert_eq!(
            advertised_bare_id(&copilot, "claude-opus-4.5"),
            "claude-opus-4-5"
        );
        assert_eq!(
            advertised_bare_id(&copilot, "claude-3.5-sonnet"),
            "claude-3-5-sonnet"
        );
        assert_eq!(
            advertised_bare_id(&copilot, "claude-sonnet-4"),
            "claude-sonnet-4"
        );
        assert_eq!(advertised_bare_id(&copilot, "claude-v.1"), "claude-v.1");
        assert_eq!(advertised_bare_id(&copilot, "gpt-4.1"), "gpt-4.1");
    }

    #[test]
    fn static_profile_claude_ids_keep_dots() {
        assert_eq!(
            advertised_bare_id(&profile("proxy", &[]), "claude-opus-4.5"),
            "claude-opus-4.5"
        );
    }

    #[tokio::test]
    async fn copilot_refresh_advertises_dashed_ids_and_records_catalog() {
        let github = MockServer::start().await;
        let copilot = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/copilot_internal/user"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "chat_enabled": true,
                "endpoints": {"api": copilot.uri()},
            })))
            .mount(&github)
            .await;
        Mock::given(method("GET"))
            .and(path(MODELS_PATH))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({"data": [
                {"id": "claude-opus-4.5", "supported_endpoints": ["/chat/completions", "/v1/messages"]},
                {"id": "gpt-4.1", "supported_endpoints": ["/chat/completions"]},
            ]})))
            .mount(&copilot)
            .await;
        let state = copilot_state(&github.uri());

        let payload = refresh_models(&state).await;

        assert_eq!(
            ids(&payload),
            ["copilot/claude-opus-4-5", "copilot/gpt-4.1", "copilot/auto"]
        );
        assert_eq!(state.copilot_catalog("copilot")[0]["id"], "claude-opus-4.5");
        let auto = &payload["data"][2];
        assert_eq!(auto["display_name"], "Auto · copilot");
        assert_eq!(auto["type"], "model");
        assert_eq!(
            auto["supported_endpoints"],
            json!(["/chat/completions", "/responses", "/v1/messages"])
        );
        let Json(settings) = crate::routes::claude_code::settings(State(state.clone())).await;
        assert!(
            settings["modelPicker"]["options"]
                .as_array()
                .unwrap()
                .iter()
                .any(|option| option["model"] == "copilot/auto"
                    && option["label"] == "Auto · copilot")
        );
        assert_eq!(
            state.upstream_model("copilot", "claude-opus-4-5"),
            Some(UpstreamModel {
                upstream_id: "claude-opus-4.5".to_string(),
                native_messages: true,
            })
        );
        assert_eq!(
            state.upstream_model("copilot", "gpt-4.1"),
            Some(UpstreamModel {
                upstream_id: "gpt-4.1".to_string(),
                native_messages: false,
            })
        );
        assert_eq!(state.upstream_model("copilot", "claude-opus-4.5"), None);
    }

    #[test]
    fn rewrite_model_adds_display_name_and_description() {
        let mut labelled = profile("llmgw", &[]);
        labelled.display_name = Some("Gateway".to_string());
        let out = rewrite_model(&labelled, &json!({"id": "gpt-4.1"}));
        assert_eq!(out["display_name"], "gpt-4.1 · Gateway");
        assert_eq!(out["description"], "via llm-hub profile Gateway");
    }

    #[test]
    fn static_models_add_display_name_and_description() {
        let models = static_models(&profile("local", &["qwen3"]));
        assert_eq!(models[0]["display_name"], "qwen3 · local");
        assert_eq!(models[0]["description"], "via llm-hub profile local");
    }

    #[tokio::test]
    async fn anthropic_version_header_adds_anthropic_list_fields() {
        let state = state_from(&[
            ("LLM_HUB_PROFILES", "proxy"),
            ("LLM_HUB_PROXY_BASE_URL", DEAD_BASE_URL),
        ]);
        state.set_model_cache(json!({"object": "list", "data": [
            {"id": "proxy/claude-a", "owned_by": "proxy"},
            {"id": "proxy/claude-b", "owned_by": "proxy"},
        ]}));
        let mut headers = HeaderMap::new();
        headers.insert(ANTHROPIC_VERSION_HEADER, "2023-06-01".parse().unwrap());

        let Json(payload) = list_models(State(state), headers).await;

        assert_eq!(payload["has_more"], false);
        assert_eq!(payload["first_id"], "proxy/claude-a");
        assert_eq!(payload["last_id"], "proxy/claude-b");
        assert_eq!(payload["data"][0]["type"], "model");
        assert_eq!(payload["object"], "list");
    }

    #[tokio::test]
    async fn no_anthropic_version_header_keeps_openai_list() {
        let state = state_from(&[
            ("LLM_HUB_PROFILES", "proxy"),
            ("LLM_HUB_PROXY_BASE_URL", DEAD_BASE_URL),
        ]);
        state.set_model_cache(
            json!({"object": "list", "data": [{"id": "proxy/m", "owned_by": "proxy"}]}),
        );

        let Json(payload) = list_models(State(state), HeaderMap::new()).await;

        assert!(payload.get("has_more").is_none());
        assert!(payload["data"][0].get("type").is_none());
    }

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
            managed: None,
            copilot: None,
        }
    }

    #[test]
    fn rewrite_claude_opus_advertises_1m_id_and_tokens() {
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
    fn rewrite_claude_opus_200k_upstream_keeps_plain_id() {
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
    fn rewrite_local_model_keeps_plain_id() {
        let item = json!({"id": "meta/muse-glimmer", "object": "model"});
        let out = rewrite_model(&profile("llama_swap", &[]), &item);
        assert_eq!(out["id"], "llama_swap/meta/muse-glimmer");
        assert!(out.get("max_input_tokens").is_none());
    }

    #[test]
    fn static_claude_sonnet_no_tokens_means_no_1m_suffix() {
        let models = static_models(&profile("llmgw", &["bedrock/anthropic.claude-sonnet-5"]));
        assert_eq!(models[0]["id"], "llmgw/bedrock/anthropic.claude-sonnet-5");
        assert!(models[0].get("max_input_tokens").is_none());
    }

    #[test]
    fn disabled_profile_models_only_enabled_profiles_remain() {
        let enabled = profile("enabled", &[]);
        let mut disabled = profile("disabled", &[]);
        disabled.enabled = false;
        let profiles = vec![enabled, disabled];
        let mut payload = json!({
            "object": "list",
            "data": [
                {"id": "enabled/model", "owned_by": "enabled"},
                {"id": "disabled/model", "owned_by": "disabled"},
                {"id": "unknown/model"}
            ]
        });

        filter_enabled_models(&mut payload, &profiles);

        assert_eq!(
            payload,
            json!({
                "object": "list",
                "data": [{"id": "enabled/model", "owned_by": "enabled"}]
            })
        );
    }
}
