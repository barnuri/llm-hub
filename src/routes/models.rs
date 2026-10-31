use std::collections::HashMap;

use axum::Json;
use axum::extract::{Query, Request, State};
use axum::middleware::Next;
use axum::response::Response;
use futures_util::future::join_all;
use reqwest::header::HeaderMap;
use serde::Deserialize;
use serde_json::{Value, json};

use crate::configs::ProfileConfig;
use crate::consts::{MODELS_CACHE_TTL, MODELS_FANOUT_TIMEOUT};
use crate::dependencies::state::AppState;
use crate::schemas::launch::profile_filter::ProfileFilter;
use crate::schemas::model_context::{
    advertised_model_id, inferred_max_input_tokens, max_input_tokens_from_item,
};
use crate::schemas::upstream_model::UpstreamModel;
use crate::services::copilot_auto::{AUTO_MODEL, AutoTier};
use crate::services::model_limits::ModelLimit;
use crate::services::upstream_auth;

pub(crate) const MODELS_PATH: &str = "/models";
/// The bare model id a Copilot profile advertises for Copilot Auto routing.
pub(crate) const COPILOT_AUTO_MODEL: &str = AUTO_MODEL;
const COPILOT_AUTO_ENDPOINTS: [&str; 3] = ["/chat/completions", "/responses", "/v1/messages"];
const COPILOT_POLICY_DISABLED: &str = "disabled";
const CLAUDE_ID_PREFIX: &str = "claude-";
const NATIVE_MESSAGES_ENDPOINT: &str = "/v1/messages";
const ANTHROPIC_VERSION_HEADER: &str = "anthropic-version";

/// `?profiles=a,b` limits a model list to those profiles, like the
/// `x-llm-hub-profiles` header.
#[derive(Debug, Deserialize, Default)]
pub struct ProfilesQuery {
    pub profiles: Option<String>,
}

pub async fn list_models(
    State(state): State<AppState>,
    Query(query): Query<ProfilesQuery>,
    headers: HeaderMap,
) -> Json<Value> {
    let mut payload = enabled_models(&state).await;
    ProfileFilter::from_request(query.profiles.as_deref(), &headers).retain_models(&mut payload);
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
                merged.extend(fallback_models(state, profile));
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
            .map(|item| rewrite_model(profile, item, None))
            .collect());
    }
    state.update_copilot_catalog(
        &profile.name,
        usable.iter().copied().cloned().collect(),
        upstream_catalog(profile, &usable),
    );
    Ok(copilot_models(state, profile, &usable))
}

/// A failed fetch lists the profile's last Copilot catalog, so a restart with
/// a slow upstream does not cache an empty list. Other profiles list their
/// static models.
fn fallback_models(state: &AppState, profile: &ProfileConfig) -> Vec<Value> {
    let catalog = state.copilot_catalog(&profile.name);
    if profile.copilot.is_none() || catalog.is_empty() {
        return static_models(profile);
    }
    let items: Vec<&Value> = catalog.iter().collect();
    copilot_models(state, profile, &items)
}

/// Rewrites Copilot catalog entries to `<profile>/<id>` and adds
/// `<profile>/auto` plus one `<profile>/auto-<tier>` per Auto tier, each only
/// when Copilot does not list that id itself.
fn copilot_models(state: &AppState, profile: &ProfileConfig, items: &[&Value]) -> Vec<Value> {
    let lists = |id: &str| {
        items
            .iter()
            .any(|item| item.get("id").and_then(Value::as_str) == Some(id))
    };
    let limits = state.model_limits().for_profile(&profile.name);
    let mut models: Vec<Value> = items
        .iter()
        .map(|item| rewrite_model(profile, item, Some(&limits)))
        .collect();
    let auto_ids = std::iter::once((COPILOT_AUTO_MODEL.to_string(), AutoTier::default())).chain(
        AutoTier::ALL
            .into_iter()
            .map(|tier| (tier.model_id(), tier)),
    );
    for (id, tier) in auto_ids {
        if !lists(&id) {
            models.push(copilot_auto_model(profile, &id, tier));
        }
    }
    models
}

/// Loads the Copilot catalogs saved by earlier runs, so the first request after
/// a restart already maps dashed Claude ids and picks native `/v1/messages`.
/// Call before the server accepts requests.
pub fn restore_catalogs(state: &AppState) {
    let Some(store) = state.store() else {
        return;
    };
    let mut stored = match store.load_model_catalogs() {
        Ok(stored) => stored,
        Err(e) => {
            tracing::warn!("cannot load the saved model catalogs: {e}");
            return;
        }
    };
    let config = state.config();
    for profile in config.profiles.iter().filter(|p| p.copilot.is_some()) {
        let Some(entries) = stored.remove(&profile.name) else {
            continue;
        };
        let items: Vec<&Value> = entries.iter().collect();
        state.set_upstream_models(&profile.name, upstream_catalog(profile, &items));
        state.set_copilot_catalog(&profile.name, entries);
    }
}

/// Holds a proxied request while an enabled Copilot profile has no catalog
/// and the first fan-out since startup is still running, so the request is
/// not sent upstream with an unmapped id. Once that fan-out has finished this
/// costs one atomic load.
pub async fn await_first_catalog(
    State(state): State<AppState>,
    request: Request,
    next: Next,
) -> Response {
    if !state.first_refresh().initialized() && lacks_copilot_catalog(&state) {
        let wait = tokio::time::timeout(MODELS_FANOUT_TIMEOUT, first_refresh(&state));
        if wait.await.is_err() {
            tracing::warn!("first model refresh is still running, sending the request anyway");
        }
    }
    next.run(request).await
}

fn lacks_copilot_catalog(state: &AppState) -> bool {
    state
        .config()
        .profiles
        .iter()
        .any(|p| p.enabled && p.copilot.is_some() && !state.has_upstream_models(&p.name))
}

/// The first fan-out since startup. Concurrent callers share one run.
async fn first_refresh(state: &AppState) {
    state
        .first_refresh()
        .get_or_init(|| async {
            refresh_models(state).await;
        })
        .await;
}

/// `<profile>/<bare_id>` on `tier`: Copilot's router picks the model, on every wire API.
fn copilot_auto_model(profile: &ProfileConfig, bare_id: &str, tier: AutoTier) -> Value {
    json!({
        "id": format!("{}/{bare_id}", profile.name),
        "object": "model",
        "type": "model",
        "owned_by": profile.name,
        "display_name": format!("Auto ({}) · {}", tier.label(), profile.label()),
        "description": format!(
            "Copilot Auto picks the model per conversation on the {} tier, via llm-hub profile {}",
            tier.as_str(),
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

/// `limits` holds a Copilot profile's learned or seeded prompt limits. They win
/// over the upstream's listed window, which Copilot understates for Claude.
fn rewrite_model(
    profile: &ProfileConfig,
    item: &Value,
    limits: Option<&HashMap<String, ModelLimit>>,
) -> Value {
    let mut model = item.clone();
    let raw_id = item.get("id").and_then(Value::as_str).unwrap_or_default();
    let upstream_tokens = limits
        .and_then(|limits| limits.get(raw_id))
        .map(|known| known.limit)
        .or_else(|| max_input_tokens_from_item(item));
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
        first_refresh(&state).await;
        loop {
            tokio::time::sleep(MODELS_CACHE_TTL).await;
            refresh_models(&state).await;
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
    use crate::services::store::Store;
    use std::collections::HashMap;
    use std::sync::Arc;
    use std::time::Duration;
    use wiremock::matchers::{method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    const DEAD_BASE_URL: &str = "http://127.0.0.1:9";

    fn state_from(vars: &[(&str, &str)]) -> AppState {
        state_with_store(vars, None)
    }

    fn state_with_store(vars: &[(&str, &str)], store: Option<Store>) -> AppState {
        let vars: HashMap<String, String> = vars
            .iter()
            .map(|(key, value)| ((*key).to_string(), (*value).to_string()))
            .collect();
        AppState::with_seams(
            HubConfig::from_map(&vars).unwrap(),
            store,
            Arc::new(MemoryCredentialStore::default()),
            Arc::new(FakeGhCli::new(GhOutcome::Token("gho_test".to_string()))),
        )
        .unwrap()
    }

    fn copilot_state(github_api_url: &str) -> AppState {
        copilot_state_with_store(github_api_url, None)
    }

    fn copilot_state_with_store(github_api_url: &str, store: Option<Store>) -> AppState {
        state_with_store(
            &[
                ("LLM_HUB_PROFILES", "copilot"),
                ("LLM_HUB_COPILOT_BASE_URL", DEAD_BASE_URL),
                ("LLM_HUB_COPILOT_AUTH", CopilotFlow::Opencode.auth_value()),
                ("LLM_HUB_COPILOT_GITHUB_API_URL", github_api_url),
            ],
            store,
        )
    }

    fn sqlite_store(path: &std::path::Path) -> Store {
        Store::open("sqlite", Some(path.to_str().unwrap())).unwrap()
    }

    /// A GitHub API and a Copilot API whose `/models` lists `models`, served
    /// after `delay`, at most `expected` times when given.
    async fn copilot_upstream(
        models: Value,
        delay: Duration,
        expected: Option<u64>,
    ) -> (MockServer, MockServer) {
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
        mount_models(&copilot, models, delay, expected).await;
        (github, copilot)
    }

    async fn mount_models(
        copilot: &MockServer,
        models: Value,
        delay: Duration,
        expected: Option<u64>,
    ) {
        let mock = Mock::given(method("GET"))
            .and(path(MODELS_PATH))
            .respond_with(
                ResponseTemplate::new(200)
                    .set_body_json(json!({ "data": models }))
                    .set_delay(delay),
            );
        match expected {
            Some(times) => mock.expect(times).mount(copilot).await,
            None => mock.mount(copilot).await,
        }
    }

    fn opus_catalog() -> Value {
        json!([{"id": "claude-opus-5.5", "supported_endpoints": ["/v1/messages"]}])
    }

    fn stored_ids(state: &AppState) -> Vec<String> {
        state.store().unwrap().load_model_catalogs().unwrap()["copilot"]
            .iter()
            .filter_map(|entry| entry["id"].as_str().map(str::to_string))
            .collect()
    }

    /// Serves a route behind [`await_first_catalog`] that answers with the
    /// upstream id `claude-opus-5-5` maps to when the handler runs.
    async fn serve_mapping_probe(state: AppState) -> String {
        async fn probe(State(state): State<AppState>) -> String {
            state
                .upstream_model("copilot", "claude-opus-5-5")
                .map_or_else(|| "unmapped".to_string(), |m| m.upstream_id)
        }
        let router = axum::Router::new()
            .route("/probe", axum::routing::post(probe))
            .route_layer(axum::middleware::from_fn_with_state(
                state.clone(),
                await_first_catalog,
            ))
            .with_state(state);
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let base = format!("http://{}/probe", listener.local_addr().unwrap());
        tokio::spawn(async move { axum::serve(listener, router).await });
        base
    }

    #[tokio::test]
    async fn copilot_models_advertise_the_learned_or_seeded_prompt_limit() {
        let listed = |id: &str| {
            json!({"id": id, "supported_endpoints": ["/v1/messages"],
                   "capabilities": {"limits": {"max_prompt_tokens": 136_000}}})
        };
        let models = json!([
            listed("claude-opus-5.5"),
            listed("claude-haiku-4.5"),
            listed("gpt-9")
        ]);
        let (github, _copilot) = copilot_upstream(models, Duration::ZERO, None).await;
        let state = copilot_state(&github.uri());
        state
            .model_limits()
            .learn("copilot", "claude-haiku-4.5", 180_000, 0);

        let payload = refresh_models(&state).await;

        let window = |id: &str| {
            let model = payload["data"]
                .as_array()
                .unwrap()
                .iter()
                .find(|m| m["id"] == id)
                .unwrap_or_else(|| panic!("{id} missing from {payload}"));
            assert_eq!(model["context_window"], model["max_input_tokens"]);
            model["max_input_tokens"].as_u64()
        };
        assert_eq!(window("copilot/claude-opus-5-5[1m]"), Some(1_000_000));
        assert_eq!(window("copilot/claude-haiku-4-5"), Some(180_000));
        assert_eq!(window("copilot/gpt-9"), Some(136_000));
    }

    #[tokio::test]
    async fn restart_restores_saved_catalog_before_any_fetch() {
        let dir = tempfile::tempdir().unwrap();
        let db = dir.path().join("hub.db");
        let (github, _copilot) = copilot_upstream(opus_catalog(), Duration::ZERO, None).await;
        let first_run = copilot_state_with_store(&github.uri(), Some(sqlite_store(&db)));
        refresh_models(&first_run).await;
        drop(first_run);

        let restarted = copilot_state_with_store(DEAD_BASE_URL, Some(sqlite_store(&db)));
        restore_catalogs(&restarted);

        assert_eq!(
            restarted.upstream_model("copilot", "claude-opus-5-5"),
            Some(UpstreamModel {
                upstream_id: "claude-opus-5.5".to_string(),
                native_messages: true,
            })
        );
        assert_eq!(
            restarted.copilot_catalog("copilot")[0]["id"],
            "claude-opus-5.5"
        );
    }

    #[tokio::test]
    async fn failed_fetch_after_restart_lists_the_restored_catalog() {
        let dir = tempfile::tempdir().unwrap();
        let store = sqlite_store(&dir.path().join("hub.db"));
        let saved = [
            json!({"id": "claude-opus-5.5", "supported_endpoints": ["/v1/messages"]}),
            json!({"id": "claude-haiku-4.5", "supported_endpoints": ["/v1/messages"]}),
        ];
        store.save_model_catalog("copilot", &saved).unwrap();
        let restarted = copilot_state_with_store(DEAD_BASE_URL, Some(store));
        restore_catalogs(&restarted);

        let payload = enabled_models(&restarted).await;

        let listed = ids(&payload);
        assert_eq!(listed.len(), 7, "unexpected list {payload}");
        assert!(
            listed
                .iter()
                .any(|id| id.starts_with("copilot/claude-opus-5-5"))
        );
        assert!(listed.contains(&"copilot/claude-haiku-4-5"));
        assert!(listed.contains(&"copilot/auto"));
        assert_eq!(restarted.model_cache().unwrap().payload, payload);
    }

    #[tokio::test]
    async fn refresh_writes_the_store_only_when_the_catalog_changed() {
        let dir = tempfile::tempdir().unwrap();
        let (github, copilot) = copilot_upstream(opus_catalog(), Duration::ZERO, None).await;
        let state = copilot_state_with_store(
            &github.uri(),
            Some(sqlite_store(&dir.path().join("hub.db"))),
        );
        refresh_models(&state).await;
        assert_eq!(stored_ids(&state), ["claude-opus-5.5"]);
        let sentinel = [json!({"id": "sentinel"})];
        state
            .store()
            .unwrap()
            .save_model_catalog("copilot", &sentinel)
            .unwrap();

        refresh_models(&state).await;
        assert_eq!(stored_ids(&state), ["sentinel"]);

        copilot.reset().await;
        let changed = json!([{"id": "claude-opus-5.5"}, {"id": "gpt-5.5"}]);
        mount_models(&copilot, changed, Duration::ZERO, None).await;
        refresh_models(&state).await;
        assert_eq!(stored_ids(&state), ["claude-opus-5.5", "gpt-5.5"]);
        assert!(state.upstream_model("copilot", "gpt-5.5").is_some());
    }

    #[tokio::test]
    async fn missing_catalog_holds_requests_on_one_shared_fetch() {
        let delay = Duration::from_millis(200);
        let (github, _copilot) = copilot_upstream(opus_catalog(), delay, Some(1)).await;
        let url = serve_mapping_probe(copilot_state(&github.uri())).await;
        let client = reqwest::Client::new();

        let (first, second) = tokio::join!(client.post(&url).send(), client.post(&url).send());

        assert_eq!(first.unwrap().text().await.unwrap(), "claude-opus-5.5");
        assert_eq!(second.unwrap().text().await.unwrap(), "claude-opus-5.5");
    }

    #[tokio::test]
    async fn restored_catalog_lets_requests_through_without_a_fetch() {
        let (github, _copilot) = copilot_upstream(opus_catalog(), Duration::ZERO, Some(0)).await;
        let state = copilot_state(&github.uri());
        let entry = UpstreamModel {
            upstream_id: "claude-opus-5.5".to_string(),
            native_messages: true,
        };
        state.set_upstream_models(
            "copilot",
            HashMap::from([("claude-opus-5-5".to_string(), entry)]),
        );
        let url = serve_mapping_probe(state).await;

        let response = reqwest::Client::new().post(&url).send().await.unwrap();

        assert_eq!(response.text().await.unwrap(), "claude-opus-5.5");
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

        let Json(payload) = list_models(
            State(state),
            Query(ProfilesQuery::default()),
            HeaderMap::new(),
        )
        .await;

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

        let Json(payload) = list_models(
            State(state),
            Query(ProfilesQuery::default()),
            HeaderMap::new(),
        )
        .await;

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
    async fn copilot_listing_its_own_auto_ids_is_not_duplicated() {
        let catalog = json!([
            {"id": "auto", "name": "Copilot Auto"},
            {"id": "auto-fast", "name": "Copilot Auto Fast"},
        ]);
        let (github, _copilot) = copilot_upstream(catalog, Duration::ZERO, None).await;
        let state = copilot_state(&github.uri());

        let payload = refresh_models(&state).await;

        assert_eq!(
            ids(&payload),
            [
                "copilot/auto",
                "copilot/auto-fast",
                "copilot/auto-efficiency",
                "copilot/auto-balance",
                "copilot/auto-intelligence",
            ]
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
            [
                "copilot/claude-opus-4-5",
                "copilot/gpt-4.1",
                "copilot/auto",
                "copilot/auto-efficiency",
                "copilot/auto-balance",
                "copilot/auto-intelligence",
                "copilot/auto-fast",
            ]
        );
        assert_eq!(state.copilot_catalog("copilot")[0]["id"], "claude-opus-4.5");
        let labels: Vec<&str> = payload["data"].as_array().unwrap()[2..]
            .iter()
            .map(|model| model["display_name"].as_str().unwrap())
            .collect();
        assert_eq!(
            labels,
            [
                "Auto (Balance) · copilot",
                "Auto (Efficiency) · copilot",
                "Auto (Balance) · copilot",
                "Auto (Intelligence) · copilot",
                "Auto (Fast) · copilot",
            ]
        );
        let auto = &payload["data"][2];
        assert_eq!(auto["type"], "model");
        assert_eq!(
            auto["supported_endpoints"],
            json!(["/chat/completions", "/responses", "/v1/messages"])
        );
        let Json(settings) = crate::routes::claude_code::settings(
            State(state.clone()),
            Query(ProfilesQuery::default()),
            HeaderMap::new(),
        )
        .await;
        let options = settings["modelPicker"]["options"].as_array().unwrap();
        for (model, label) in [
            ("copilot/auto", "Auto (Balance) · copilot"),
            ("copilot/auto-efficiency", "Auto (Efficiency) · copilot"),
            ("copilot/auto-intelligence", "Auto (Intelligence) · copilot"),
            ("copilot/auto-fast", "Auto (Fast) · copilot"),
        ] {
            assert!(
                options
                    .iter()
                    .any(|option| option["model"] == model && option["label"] == label),
                "{model} missing from {settings}"
            );
        }
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
        let out = rewrite_model(&labelled, &json!({"id": "gpt-4.1"}), None);
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

        let Json(payload) =
            list_models(State(state), Query(ProfilesQuery::default()), headers).await;

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

        let Json(payload) = list_models(
            State(state),
            Query(ProfilesQuery::default()),
            HeaderMap::new(),
        )
        .await;

        assert!(payload.get("has_more").is_none());
        assert!(payload["data"][0].get("type").is_none());
    }

    fn two_profile_state() -> AppState {
        let state = state_from(&[
            ("LLM_HUB_PROFILES", "copilot,ivrit,local"),
            ("LLM_HUB_COPILOT_BASE_URL", DEAD_BASE_URL),
            ("LLM_HUB_IVRIT_BASE_URL", DEAD_BASE_URL),
            ("LLM_HUB_LOCAL_BASE_URL", DEAD_BASE_URL),
        ]);
        state.set_model_cache(json!({"object": "list", "data": [
            {"id": "copilot/auto", "owned_by": "copilot"},
            {"id": "local/qwen", "owned_by": "local"},
            {"id": "ivrit/whisper", "owned_by": "ivrit"},
        ]}));
        state
    }

    #[tokio::test]
    async fn profiles_header_limits_the_list() {
        let mut headers = HeaderMap::new();
        headers.insert(ProfileFilter::HEADER, "copilot, ivrit".parse().unwrap());
        headers.insert(ANTHROPIC_VERSION_HEADER, "2023-06-01".parse().unwrap());

        let Json(payload) = list_models(
            State(two_profile_state()),
            Query(ProfilesQuery::default()),
            headers,
        )
        .await;

        assert_eq!(ids(&payload), ["copilot/auto", "ivrit/whisper"]);
        assert_eq!(payload["last_id"], "ivrit/whisper");
    }

    #[tokio::test]
    async fn profiles_query_limits_the_list() {
        let query = ProfilesQuery {
            profiles: Some("local".to_string()),
        };

        let Json(payload) =
            list_models(State(two_profile_state()), Query(query), HeaderMap::new()).await;

        assert_eq!(ids(&payload), ["local/qwen"]);
    }

    #[tokio::test]
    async fn no_profiles_filter_lists_everything() {
        let Json(payload) = list_models(
            State(two_profile_state()),
            Query(ProfilesQuery::default()),
            HeaderMap::new(),
        )
        .await;

        assert_eq!(
            ids(&payload),
            ["copilot/auto", "local/qwen", "ivrit/whisper"]
        );
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
        let out = rewrite_model(&profile("llmgw", &[]), &item, None);
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
        let out = rewrite_model(&profile("llmgw", &[]), &item, None);
        assert_eq!(out["id"], "llmgw/bedrock/anthropic.claude-opus-5");
        assert_eq!(out["max_input_tokens"], 200_000);
    }

    #[test]
    fn rewrite_local_model_keeps_plain_id() {
        let item = json!({"id": "meta/muse-glimmer", "object": "model"});
        let out = rewrite_model(&profile("llama_swap", &[]), &item, None);
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
