use std::collections::HashMap;
use std::path::Path;

use axum::Json;
use axum::extract::{Path as UrlPath, Query, State};
use serde::Deserialize;
use serde_json::{Value, json};

use crate::configs::{ProfileConfig, mask_key};
use crate::consts::{DEFAULT_INSIGHTS_MIN_REQUESTS, DEFAULT_INSIGHTS_RANGE, ENV_FILE, VERSION};
use crate::dependencies::state::AppState;
use crate::schemas::api_key_input::ApiKeyInput;
use crate::schemas::api_key_record::ApiKeyRecord;
use crate::schemas::app_error::AppError;
use crate::schemas::errors_report::ErrorsReport;
use crate::schemas::fallbacks_input::FallbacksInput;
use crate::schemas::insights::insights_report::InsightsReport;
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
            })
        })
        .collect();
    Json(json!({
        "profiles": profiles,
        "default_fallbacks": config.default_fallbacks,
        "readonly": config.config_readonly,
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

    let mut request = client.get(format!("{}/models", profile.base_url));
    if !profile.api_key.is_empty() {
        request = request.bearer_auth(&profile.api_key);
    }
    for (header_name, value) in &profile.extra_headers {
        request = request.header(header_name, value);
    }
    let started = std::time::Instant::now();
    match tokio::time::timeout(crate::consts::MODELS_FANOUT_TIMEOUT, request.send()).await {
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
            Ok(Json(json!({ "ok": false, "error": e.to_string() })))
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

    let mut snapshot = if range == "live" || !persistent {
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
        snap
    } else {
        let since_ms = match range {
            "1d" => Some(now_ms().saturating_sub(86_400_000)),
            "7d" => Some(now_ms().saturating_sub(7 * 86_400_000)),
            "30d" => Some(now_ms().saturating_sub(30 * 86_400_000)),
            "all" => None,
            other => {
                return Err(AppError::BadRequest(format!(
                    "unknown stats range '{other}' (expected live, 1d, 7d, 30d, all)"
                )));
            }
        };
        let store = state.store().expect("persistent checked above");
        store
            .stats(&StatsFilter {
                since_ms,
                profile,
                model,
                range_label: range.to_string(),
            })
            .map_err(AppError::Internal)?
    };

    let book = crate::services::pricing::PricingBook::from_hub(&state.config());
    crate::services::pricing::apply_costs(&mut snapshot, &book);

    let body = serde_json::to_value(snapshot)
        .map_err(|e| AppError::Internal(format!("serialize stats failed: {e}")))?;
    Ok(Json(body))
}

pub async fn usage(State(state): State<AppState>) -> Result<Json<Value>, AppError> {
    let Some(store) = state.store() else {
        return Err(AppError::BadRequest(
            "persistence is off — set LLM_HUB_PERSISTENT=true to keep usage history".into(),
        ));
    };
    let mut report = store.usage().map_err(AppError::Internal)?;
    let book = crate::services::pricing::PricingBook::from_hub(&state.config());
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
    let book = crate::services::pricing::PricingBook::from_hub(&state.config());
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
    let report = insights_report(&state, &query)?;
    serde_json::to_value(report)
        .map(Json)
        .map_err(|e| AppError::Internal(format!("serialize insights failed: {e}")))
}

/// The best ranked model for `by`, so scripts can choose a model from stats.
pub async fn insights_pick(
    State(state): State<AppState>,
    Query(query): Query<InsightsQuery>,
) -> Result<Json<Value>, AppError> {
    let report = insights_report(&state, &query)?;
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
    let window = insights_window(&state, &query)?;
    let report = insights::build_agent_report(
        &window.outcomes,
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
    range: String,
    profile: Option<String>,
    min_requests: u64,
}

fn insights_report(state: &AppState, query: &InsightsQuery) -> Result<InsightsReport, AppError> {
    let window = insights_window(state, query)?;
    Ok(insights::build_report(
        &window.outcomes,
        &window.range,
        window.min_requests,
        now_ms(),
    ))
}

fn insights_window(state: &AppState, query: &InsightsQuery) -> Result<InsightsWindow, AppError> {
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
    Ok(InsightsWindow {
        outcomes,
        range: range.to_string(),
        profile,
        min_requests: query
            .min_requests
            .unwrap_or(DEFAULT_INSIGHTS_MIN_REQUESTS)
            .max(1),
    })
}
