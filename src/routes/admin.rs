use std::collections::HashMap;
use std::path::Path;

use axum::Json;
use axum::extract::{Path as UrlPath, Query, State};
use serde::Deserialize;
use serde_json::{Value, json};

use crate::configs::{ProfileConfig, mask_key};
use crate::consts::{DEFAULT_INSIGHTS_MIN_REQUESTS, DEFAULT_INSIGHTS_RANGE, ENV_FILE, VERSION};
use crate::dependencies::state::AppState;
use crate::routes::models::send_models_request;
use crate::schemas::api_key_input::ApiKeyInput;
use crate::schemas::api_key_record::ApiKeyRecord;
use crate::schemas::app_error::AppError;
use crate::schemas::copilot_prices_input::CopilotPricesInput;
use crate::schemas::errors_report::ErrorsReport;
use crate::schemas::fallbacks_input::FallbacksInput;
use crate::schemas::insights::insights_report::InsightsReport;
use crate::schemas::insights::model_catalog_entry::ModelCatalogEntry;
use crate::schemas::insights::pick_by::PickBy;
use crate::schemas::profile_input::ProfileInput;
use crate::schemas::usage_report::UsageReport;
use crate::services::env_writer;
use crate::services::insights;
use crate::services::stats::RequestOutcome;
use crate::services::store::{StatsFilter, hash_key, now_ms};
use crate::utils::hex::to_hex;
use crate::utils::time::elapsed_ms;

pub async fn list_profiles(State(state): State<AppState>) -> Json<Value> {
    let config = state.config();
    let health = state.upstream_health();
    let profiles: Vec<Value> = config
        .profiles
        .iter()
        .map(|p| {
            json!({
                "name": p.name,
                "display_name": p.display_name,
                "label": p.label(),
                "base_url": p.base_url,
                "api_key_masked": mask_key(&p.api_key),
                "headers": p.extra_headers.iter().cloned().collect::<HashMap<_, _>>(),
                "timeout_ms": p.timeout_ms,
                "enabled": p.enabled,
                "models": p.static_models,
                "healthy": health.get(&p.name).copied(),
                "managed": p.managed.is_some(),
                "copilot": p.copilot.is_some(),
                "running": state.managed().is_running(&p.name),
            })
        })
        .collect();
    Json(json!({
        "profiles": profiles,
        "default_fallbacks": config.default_fallbacks,
        "readonly": config.config_readonly,
        "copilot_prices_refresh_secs": config.copilot_prices_refresh.map_or(0, |d| d.as_secs()),
        "persistent": config.persistent,
        "auth_enabled": config.master_key.is_some() || !state.api_key_hashes().is_empty(),
        "version": VERSION,
    }))
}

pub async fn upsert_profile(
    State(state): State<AppState>,
    Json(input): Json<ProfileInput>,
) -> Result<Json<Value>, AppError> {
    let config = state.config();
    if config.config_readonly {
        return Err(AppError::ReadOnly);
    }
    let name = input.name.trim().to_string();
    if name.contains('/') || name.is_empty() {
        return Err(AppError::BadRequest(
            "profile name must be non-empty and contain no '/'".into(),
        ));
    }

    let existing = config.profile(&name);
    let uses_copilot_auth = existing.is_some_and(|p| p.copilot.is_some());
    if uses_copilot_auth && input.api_key.as_deref().is_some_and(|k| !k.is_empty()) {
        return Err(AppError::BadRequest(format!(
            "profile {name} uses LLM_HUB_{}_AUTH, so it takes no API key",
            crate::configs::env_name(&name)
        )));
    }
    let api_key = match input.api_key.filter(|k| !k.is_empty()) {
        Some(key) => key,
        None => existing.map(|p| p.api_key.clone()).unwrap_or_default(),
    };
    let display_name = input
        .display_name
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty());
    let profile = ProfileConfig {
        name,
        display_name,
        base_url: input.base_url.trim().trim_end_matches('/').to_string(),
        api_key,
        extra_headers: input.headers.unwrap_or_default().into_iter().collect(),
        timeout_ms: input.timeout_ms,
        enabled: input.enabled.unwrap_or(true),
        static_models: input.models.unwrap_or_default(),
        pricing: existing.map(|p| p.pricing).unwrap_or_default(),
        model_prices: existing.map(|p| p.model_prices.clone()).unwrap_or_default(),
        managed: existing.and_then(|p| p.managed.clone()),
        copilot: existing.and_then(|p| p.copilot.clone()),
    };
    if !profile.base_url.starts_with("http://") && !profile.base_url.starts_with("https://") {
        return Err(AppError::BadRequest(
            "base_url must start with http:// or https://".into(),
        ));
    }

    let mut names: Vec<String> = config.profiles.iter().map(|p| p.name.clone()).collect();
    if !names.contains(&profile.name) {
        names.push(profile.name.clone());
    }
    env_writer::upsert_profile(Path::new(ENV_FILE), &profile, &names)
        .map_err(AppError::Internal)?;
    reload_config(&state)?;
    Ok(Json(json!({ "ok": true })))
}

pub async fn set_default_fallbacks(
    State(state): State<AppState>,
    Json(input): Json<FallbacksInput>,
) -> Result<Json<Value>, AppError> {
    if state.config().config_readonly {
        return Err(AppError::ReadOnly);
    }
    let chain = crate::services::fallback::validate_chain(&input.fallbacks)
        .map_err(AppError::BadRequest)?;
    env_writer::set_default_fallbacks(Path::new(ENV_FILE), &chain).map_err(AppError::Internal)?;
    reload_config(&state)?;
    Ok(Json(json!({ "ok": true })))
}

pub async fn set_copilot_prices(
    State(state): State<AppState>,
    Json(input): Json<CopilotPricesInput>,
) -> Result<Json<Value>, AppError> {
    if state.config().config_readonly {
        return Err(AppError::ReadOnly);
    }
    env_writer::set_copilot_prices_refresh(Path::new(ENV_FILE), input.refresh_secs)
        .map_err(AppError::Internal)?;
    reload_config(&state)?;
    state.copilot_prices().request_refresh();
    Ok(Json(json!({ "ok": true })))
}

pub async fn delete_profile(
    State(state): State<AppState>,
    UrlPath(name): UrlPath<String>,
) -> Result<Json<Value>, AppError> {
    let config = state.config();
    if config.config_readonly {
        return Err(AppError::ReadOnly);
    }
    if config.profile(&name).is_none() {
        return Err(AppError::NotFound(format!("no such profile: {name}")));
    }
    let remaining: Vec<String> = config
        .profiles
        .iter()
        .map(|p| p.name.clone())
        .filter(|n| *n != name)
        .collect();
    env_writer::remove_profile(Path::new(ENV_FILE), &name, &remaining)
        .map_err(AppError::Internal)?;
    reload_config(&state)?;
    Ok(Json(json!({ "ok": true })))
}

pub async fn test_profile(
    State(state): State<AppState>,
    UrlPath(name): UrlPath<String>,
) -> Result<Json<Value>, AppError> {
    let config = state.config();
    let profile = config
        .profile(&name)
        .ok_or_else(|| AppError::NotFound(format!("no such profile: {name}")))?;
    let client = state
        .client(&name)
        .ok_or_else(|| AppError::Internal("no client".into()))?;

    let started = std::time::Instant::now();
    let probe = send_models_request(&state, &client, profile);
    match tokio::time::timeout(crate::consts::MODELS_FANOUT_TIMEOUT, probe).await {
        Ok(Ok(response)) => {
            let healthy = response.status().is_success();
            state.set_upstream_health(&name, healthy);
            Ok(Json(json!({
                "ok": healthy,
                "status": response.status().as_u16(),
                "latency_ms": elapsed_ms(started),
            })))
        }
        Ok(Err(e)) => {
            state.set_upstream_health(&name, false);
            Ok(Json(json!({ "ok": false, "error": e })))
        }
        Err(_) => {
            state.set_upstream_health(&name, false);
            Ok(Json(json!({ "ok": false, "error": "timeout" })))
        }
    }
}

/// Triggers a graceful restart: supervised installs are restarted by their
/// supervisor; standalone runs respawn themselves after the server drains.
pub async fn restart_server(State(state): State<AppState>) -> Json<Value> {
    crate::services::restart::request_restart(state.restart_notify());
    Json(json!({ "ok": true, "restarting": true }))
}

#[derive(Debug, Deserialize, Default)]
pub struct StatsQuery {
    /// `live` (default), `1d`, `7d`, `30d`, or `all`.
    #[serde(default)]
    pub range: Option<String>,
    pub profile: Option<String>,
    pub model: Option<String>,
}

pub async fn stats(
    State(state): State<AppState>,
    Query(query): Query<StatsQuery>,
) -> Result<Json<Value>, AppError> {
    let range = query.range.as_deref().unwrap_or("live");
    let persistent = state.store().is_some();
    let profile = query
        .profile
        .as_ref()
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty());
    let model = query
        .model
        .as_ref()
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty());

    let book = state.pricing_book();
    let snapshot = if range == "live" || !persistent {
        let mut snap = state.stats().snapshot(
            if persistent && range != "live" {
                "live"
            } else {
                range
            },
            persistent,
        );
        // Live registry cannot apply store filters; narrow client-side keys when asked.
        if let Some(profile) = &profile {
            snap.profiles.retain(|row| row.key == *profile);
            snap.models
                .retain(|row| row.key.starts_with(&format!("{profile}/")) || row.key == *profile);
        }
        if let Some(model) = &model {
            snap.models.retain(|row| row.key == *model);
        }
        if range != "live" && !persistent {
            snap.range = "live".into();
            snap.filterable = false;
        }
        crate::services::pricing::apply_costs(&mut snap, &book);
        snap
    } else {
        let since_ms = since_for_range(range, now_ms())?;
        let store = state.store().expect("persistent checked above");
        store
            .stats(
                &StatsFilter {
                    since_ms,
                    profile,
                    model,
                    range_label: range.to_string(),
                },
                &book,
            )
            .map_err(AppError::Internal)?
    };

    let body = serde_json::to_value(snapshot)
        .map_err(|e| AppError::Internal(format!("serialize stats failed: {e}")))?;
    Ok(Json(body))
}

/// Lower bound in epoch milliseconds for a range label (`today`, `1d`, `7d`, `30d`, `all`).
/// `all` has no lower bound.
pub(crate) fn since_for_range(range: &str, now_ms: u64) -> Result<Option<u64>, AppError> {
    match range {
        "today" => Ok(Some(local_midnight_ms(now_ms))),
        "1d" => Ok(Some(now_ms.saturating_sub(86_400_000))),
        "7d" => Ok(Some(now_ms.saturating_sub(7 * 86_400_000))),
        "30d" => Ok(Some(now_ms.saturating_sub(30 * 86_400_000))),
        "all" => Ok(None),
        other => Err(AppError::BadRequest(format!(
            "unknown range '{other}' (expected today, 1d, 7d, 30d, all)"
        ))),
    }
}

/// Epoch milliseconds of the most recent local midnight at or before `now`. Uses the local UTC
/// offset from `localtime_r`, so a day that starts in DST is measured from its first hour.
#[cfg(unix)]
fn local_midnight_ms(now_ms: u64) -> u64 {
    let secs = libc::time_t::try_from(now_ms / 1000).unwrap_or(libc::time_t::MAX);
    // SAFETY: `localtime_r` writes only into `tm`, which lives on this stack frame.
    let mut tm: libc::tm = unsafe { std::mem::zeroed() };
    if unsafe { libc::localtime_r(&secs, &mut tm) }.is_null() {
        return now_ms.saturating_sub(86_400_000);
    }
    let into_day = u64::try_from(tm.tm_hour).unwrap_or(0) * 3600
        + u64::try_from(tm.tm_min).unwrap_or(0) * 60
        + u64::try_from(tm.tm_sec).unwrap_or(0);
    (now_ms / 1000)
        .saturating_sub(into_day)
        .saturating_mul(1000)
}

/// Without a POSIX local clock, the day starts at UTC midnight.
#[cfg(not(unix))]
fn local_midnight_ms(now_ms: u64) -> u64 {
    (now_ms / 86_400_000) * 86_400_000
}

#[cfg(test)]
mod local_midnight_tests {
    use super::local_midnight_ms;

    #[test]
    fn midnight_is_not_after_now_and_within_a_day() {
        let now = 1_783_000_000_123_u64;
        let midnight = local_midnight_ms(now);
        assert!(midnight <= now, "{midnight} > {now}");
        assert!(
            now - midnight < 86_400_000 + 3_600_000,
            "{}",
            now - midnight
        );
        assert_eq!(midnight % 1000, 0);
    }
}

pub async fn usage(State(state): State<AppState>) -> Result<Json<Value>, AppError> {
    let Some(store) = state.store() else {
        return Err(AppError::BadRequest(
            "persistence is off — set LLM_HUB_PERSISTENT=true to keep usage history".into(),
        ));
    };
    let mut report = store.usage().map_err(AppError::Internal)?;
    let book = state.pricing_book();
    crate::services::pricing::apply_usage_costs(&mut report, &book);
    let body = serde_json::to_value(report)
        .map_err(|e| AppError::Internal(format!("serialize usage report failed: {e}")))?;
    Ok(Json(body))
}

#[derive(Debug, Deserialize, Default)]
pub struct ErrorsQuery {
    /// `1d`, `7d` (default), `30d`, or `all`.
    #[serde(default)]
    pub range: Option<String>,
}

pub async fn errors(
    State(state): State<AppState>,
    Query(query): Query<ErrorsQuery>,
) -> Result<Json<Value>, AppError> {
    let Some(store) = state.store() else {
        return Err(AppError::BadRequest(
            "persistence is off — set LLM_HUB_PERSISTENT=true to keep usage history".into(),
        ));
    };
    let range = query.range.as_deref().unwrap_or("7d");
    let since_ms = match range {
        "1d" => Some(now_ms().saturating_sub(86_400_000)),
        "7d" => Some(now_ms().saturating_sub(7 * 86_400_000)),
        "30d" => Some(now_ms().saturating_sub(30 * 86_400_000)),
        "all" => None,
        other => {
            return Err(AppError::BadRequest(format!(
                "unknown errors range '{other}' (expected 1d, 7d, 30d, all)"
            )));
        }
    };
    let (total_errors, recent) = store.errors(since_ms).map_err(AppError::Internal)?;
    let mut priced = UsageReport {
        total_requests: total_errors,
        total_errors,
        total_tokens_in: 0,
        total_tokens_out: 0,
        recent,
    };
    let book = state.pricing_book();
    crate::services::pricing::apply_usage_costs(&mut priced, &book);
    let body = serde_json::to_value(ErrorsReport {
        range: range.to_string(),
        total_errors,
        recent: priced.recent,
    })
    .map_err(|e| AppError::Internal(format!("serialize errors report failed: {e}")))?;
    Ok(Json(body))
}

// --- hub api keys ---

pub async fn list_api_keys(State(state): State<AppState>) -> Result<Json<Value>, AppError> {
    let Some(store) = state.store() else {
        return Ok(Json(json!({ "persistent": false, "keys": [] })));
    };
    let keys = store.list_api_keys().map_err(AppError::Internal)?;
    let rows: Vec<Value> = keys
        .iter()
        .map(|k| json!({ "name": k.name, "masked": k.masked, "enabled": k.enabled, "created_ms": k.created_ms }))
        .collect();
    Ok(Json(json!({ "persistent": true, "keys": rows })))
}

/// Creates a key and returns it ONCE — only the hash is stored.
pub async fn create_api_key(
    State(state): State<AppState>,
    Json(input): Json<ApiKeyInput>,
) -> Result<Json<Value>, AppError> {
    let Some(store) = state.store() else {
        return Err(AppError::BadRequest(
            "persistence is off — set LLM_HUB_PERSISTENT=true to manage api keys".into(),
        ));
    };
    if input.name.trim().is_empty() {
        return Err(AppError::BadRequest("key name must not be empty".into()));
    }
    let key = generate_key();
    let record = ApiKeyRecord {
        name: input.name.trim().to_string(),
        key_hash: hash_key(&key),
        masked: mask_key(&key),
        enabled: true,
        created_ms: now_ms(),
    };
    store
        .insert_api_key(&record)
        .map_err(AppError::BadRequest)?;
    state.refresh_api_key_hashes();
    Ok(Json(json!({ "name": record.name, "key": key })))
}

pub async fn delete_api_key(
    State(state): State<AppState>,
    UrlPath(name): UrlPath<String>,
) -> Result<Json<Value>, AppError> {
    let Some(store) = state.store() else {
        return Err(AppError::BadRequest("persistence is off".into()));
    };
    store.delete_api_key(&name).map_err(AppError::Internal)?;
    state.refresh_api_key_hashes();
    Ok(Json(json!({ "ok": true })))
}

fn generate_key() -> String {
    let bytes: [u8; 24] = rand::random();
    format!("sk-hub-{}", to_hex(&bytes))
}

fn reload_config(state: &AppState) -> Result<(), AppError> {
    crate::services::config_reload::reload(state)
        .map(|_| ())
        .map_err(AppError::Internal)
}

#[derive(Debug, Deserialize, Default)]
pub struct InsightsQuery {
    /// `1d`, `7d` (default), `30d`, or `all`.
    #[serde(default)]
    pub range: Option<String>,
    pub profile: Option<String>,
    pub min_requests: Option<u64>,
    /// Criterion for `/api/insights/pick`; `overall` when omitted.
    pub by: Option<PickBy>,
}

/// Per-model speed, stability, health and category leaders.
pub async fn insights(
    State(state): State<AppState>,
    Query(query): Query<InsightsQuery>,
) -> Result<Json<Value>, AppError> {
    let report = insights_report(&state, &query).await?;
    serde_json::to_value(report)
        .map(Json)
        .map_err(|e| AppError::Internal(format!("serialize insights failed: {e}")))
}

/// The best ranked model for `by`, so scripts can choose a model from stats.
pub async fn insights_pick(
    State(state): State<AppState>,
    Query(query): Query<InsightsQuery>,
) -> Result<Json<Value>, AppError> {
    let report = insights_report(&state, &query).await?;
    let by = query.by.unwrap_or_default();
    let Some(picked) = insights::pick(&report, by) else {
        return Err(AppError::NotFound(format!(
            "no ranked model: none has {} or more calls in {} without a failure streak",
            report.min_requests, report.range
        )));
    };
    serde_json::to_value(picked)
        .map(Json)
        .map_err(|e| AppError::Internal(format!("serialize pick failed: {e}")))
}

/// Full insights for agents: plain-language summary, every pick, per-model
/// metrics and failure reasons in one JSON document.
pub async fn insights_agent_report(
    State(state): State<AppState>,
    Query(query): Query<InsightsQuery>,
) -> Result<Json<Value>, AppError> {
    let window = insights_window(&state, &query).await?;
    let report = insights::build_agent_report(
        &window.outcomes,
        &window.catalog,
        &window.range,
        window.profile,
        window.min_requests,
        now_ms(),
    );
    serde_json::to_value(report)
        .map(Json)
        .map_err(|e| AppError::Internal(format!("serialize agent report failed: {e}")))
}

struct InsightsWindow {
    outcomes: Vec<RequestOutcome>,
    catalog: HashMap<String, ModelCatalogEntry>,
    range: String,
    profile: Option<String>,
    min_requests: u64,
}

async fn insights_report(
    state: &AppState,
    query: &InsightsQuery,
) -> Result<InsightsReport, AppError> {
    let window = insights_window(state, query).await?;
    Ok(insights::build_report(
        &window.outcomes,
        &window.catalog,
        &window.range,
        window.min_requests,
        now_ms(),
    ))
}

async fn insights_window(
    state: &AppState,
    query: &InsightsQuery,
) -> Result<InsightsWindow, AppError> {
    let Some(store) = state.store() else {
        return Err(AppError::BadRequest(
            "persistence is off — set LLM_HUB_PERSISTENT=true to keep usage history".into(),
        ));
    };
    let range = query.range.as_deref().unwrap_or(DEFAULT_INSIGHTS_RANGE);
    let since_ms = match range {
        "1d" => Some(now_ms().saturating_sub(86_400_000)),
        "7d" => Some(now_ms().saturating_sub(7 * 86_400_000)),
        "30d" => Some(now_ms().saturating_sub(30 * 86_400_000)),
        "all" => None,
        other => {
            return Err(AppError::BadRequest(format!(
                "unknown insights range '{other}' (expected 1d, 7d, 30d, all)"
            )));
        }
    };
    let profile = query
        .profile
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_string);
    let outcomes = store
        .filtered_outcomes(&StatsFilter {
            since_ms,
            profile: profile.clone(),
            model: None,
            range_label: range.to_string(),
        })
        .map_err(AppError::Internal)?;
    let catalog = insights_catalog(state, profile.as_deref()).await;
    Ok(InsightsWindow {
        outcomes,
        catalog,
        range: range.to_string(),
        profile,
        min_requests: query
            .min_requests
            .unwrap_or(DEFAULT_INSIGHTS_MIN_REQUESTS)
            .max(1),
    })
}

/// Enabled models from the hub's model list (cached, refreshed when cold),
/// narrowed to `profile` when one is given.
async fn insights_catalog(
    state: &AppState,
    profile: Option<&str>,
) -> HashMap<String, ModelCatalogEntry> {
    let payload = match state.model_cache() {
        Some(cached) => cached.payload.clone(),
        None => crate::routes::models::refresh_models(state).await,
    };
    let config = state.config();
    insights::catalog_from_models(&payload)
        .into_iter()
        .filter(|(id, _)| {
            let model_profile = id.split('/').next().unwrap_or_default();
            let enabled = config
                .profiles
                .iter()
                .any(|p| p.enabled && p.name == model_profile);
            enabled && profile.is_none_or(|wanted| wanted == model_profile)
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;
    use std::sync::Arc;

    use wiremock::matchers::{header, method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    use super::*;
    use crate::configs::{CopilotFlow, HubConfig};
    use crate::consts::COPILOT_INTEGRATION_ID;
    use crate::routes::models::MODELS_PATH;
    use crate::services::copilot_headers::HEADER_API_VERSION;
    use crate::services::credential_store::CredentialStore;
    use crate::services::fake_gh_cli::FakeGhCli;
    use crate::services::gh_cli::GhOutcome;
    use crate::services::memory_credential_store::MemoryCredentialStore;

    const DEAD_BASE_URL: &str = "http://127.0.0.1:9";
    const LOGIN_TOKEN: &str = "gho_from_login";
    const FIRST_COPILOT_TOKEN: &str = "tid=first";
    const SECOND_COPILOT_TOKEN: &str = "tid=second";
    const EXCHANGE_PATH: &str = "/copilot_internal/v2/token";
    const STATIC_KEY: &str = "sk-static";

    fn state_for(vars: &[(&str, &str)]) -> AppState {
        let vars: HashMap<String, String> = vars
            .iter()
            .map(|(key, value)| ((*key).to_string(), (*value).to_string()))
            .collect();
        let store = MemoryCredentialStore::default();
        store
            .set(
                &CopilotFlow::Vscode.credential_account("github.com"),
                LOGIN_TOKEN,
            )
            .unwrap();
        let gh = FakeGhCli::new(GhOutcome::Failed("gh not used".into()));
        AppState::with_seams(
            HubConfig::from_map(&vars).unwrap(),
            None,
            Arc::new(store),
            Arc::new(gh),
        )
        .unwrap()
    }

    fn token_response(token: &str, copilot: &MockServer) -> ResponseTemplate {
        ResponseTemplate::new(200).set_body_json(json!({
            "token": token,
            "expires_at": 4_000_000_000_u64,
            "refresh_in": 1500,
            "endpoints": {"api": copilot.uri()},
        }))
    }

    async fn probe(state: &AppState, name: &str) -> Value {
        let Json(body) = test_profile(State(state.clone()), UrlPath(name.to_string()))
            .await
            .unwrap();
        body
    }

    #[tokio::test]
    async fn test_profile_sends_the_static_api_key() {
        let upstream = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path(MODELS_PATH))
            .and(header(
                "authorization",
                format!("Bearer {STATIC_KEY}").as_str(),
            ))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({"data": []})))
            .expect(1)
            .mount(&upstream)
            .await;
        let state = state_for(&[
            ("LLM_HUB_PROFILES", "openai"),
            ("LLM_HUB_OPENAI_BASE_URL", &upstream.uri()),
            ("LLM_HUB_OPENAI_API_KEY", STATIC_KEY),
        ]);

        let body = probe(&state, "openai").await;

        assert_eq!(body["ok"], true, "{body}");
        assert_eq!(state.upstream_health().get("openai").copied(), Some(true));
    }

    #[tokio::test]
    async fn test_profile_sends_copilot_auth_and_drops_the_session_on_401() {
        let github = MockServer::start().await;
        let copilot = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path(EXCHANGE_PATH))
            .respond_with(token_response(FIRST_COPILOT_TOKEN, &copilot))
            .up_to_n_times(1)
            .expect(1)
            .mount(&github)
            .await;
        Mock::given(method("GET"))
            .and(path(EXCHANGE_PATH))
            .respond_with(token_response(SECOND_COPILOT_TOKEN, &copilot))
            .expect(1)
            .mount(&github)
            .await;
        Mock::given(method("GET"))
            .and(path(MODELS_PATH))
            .and(header(
                "authorization",
                format!("Bearer {FIRST_COPILOT_TOKEN}").as_str(),
            ))
            .and(header("copilot-integration-id", COPILOT_INTEGRATION_ID))
            .respond_with(ResponseTemplate::new(401))
            .expect(1)
            .mount(&copilot)
            .await;
        Mock::given(method("GET"))
            .and(path(MODELS_PATH))
            .and(header(
                "authorization",
                format!("Bearer {SECOND_COPILOT_TOKEN}").as_str(),
            ))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({"data": []})))
            .expect(1)
            .mount(&copilot)
            .await;
        let state = state_for(&[
            ("LLM_HUB_PROFILES", "copilot"),
            ("LLM_HUB_COPILOT_BASE_URL", DEAD_BASE_URL),
            ("LLM_HUB_COPILOT_AUTH", CopilotFlow::Vscode.auth_value()),
            ("LLM_HUB_COPILOT_GITHUB_API_URL", &github.uri()),
        ]);

        let rejected = probe(&state, "copilot").await;
        let retried = probe(&state, "copilot").await;

        assert_eq!(rejected["status"], 401, "{rejected}");
        assert_eq!(retried["ok"], true, "{retried}");
        let received = copilot.received_requests().await.unwrap();
        assert!(received[0].headers.contains_key(HEADER_API_VERSION));
    }
}
