use std::collections::HashMap;
use std::path::Path;
use utoipa::IntoParams;
use utoipa::ToSchema;

use axum::Json;
use axum::extract::{Path as UrlPath, Query, State};
use serde::Deserialize;
use serde_json::{Value, json};

use crate::configs::{KeepWarmConfig, ProfileConfig, mask_key};
use crate::consts::{DEFAULT_INSIGHTS_MIN_REQUESTS, DEFAULT_INSIGHTS_RANGE, ENV_FILE, VERSION};
use crate::dependencies::state::AppState;
use crate::ext::ConfigKind;
use crate::routes::models::send_models_request;
use crate::schemas::api_key_input::ApiKeyInput;
use crate::schemas::api_key_record::ApiKeyRecord;
use crate::schemas::app_error::AppError;
use crate::schemas::copilot_prices_input::CopilotPricesInput;
use crate::schemas::error_body::ErrorBody;
use crate::schemas::errors_report::ErrorsReport;
use crate::schemas::fallbacks_input::FallbacksInput;
use crate::schemas::insights::insights_report::InsightsReport;
use crate::schemas::insights::model_catalog_entry::ModelCatalogEntry;
use crate::schemas::insights::model_cost::ModelCost;
use crate::schemas::insights::pick_by::PickBy;
use crate::schemas::keep_warm_input::KeepWarmInput;
use crate::schemas::keep_warm_summary::KeepWarmSummary;
use crate::schemas::profile_input::ProfileInput;
use crate::schemas::usage_report::UsageReport;
use crate::services::env_writer;
use crate::services::extension_phases;
use crate::services::insights;
use crate::services::pricing::PricingBook;
use crate::services::session_insights;
use crate::services::stats::RequestOutcome;
use crate::services::store::{StatsFilter, TierRoleRow, hash_key, now_ms};
use crate::utils::hex::to_hex;
use crate::utils::time::elapsed_ms;

/// Liveness probe. Needs no key.
#[utoipa::path(get, path = "/healthz", tag = "system", summary = "Liveness", security(()),
    responses((status = 200, description = "The text `ok`", body = String)))]
pub async fn healthz() -> &'static str {
    "ok"
}

#[utoipa::path(get, path = "/api/profiles", tag = "profiles", summary = "List profiles", responses((status = 200, description = "Profiles with managed and running flags, API keys masked", body = Object), (status = 401, description = "Missing or invalid hub key.", body = ErrorBody)))]
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
                "keep_warm": p.keep_warm,
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
        "keep_warm": {
            "enabled": config.keep_warm.enabled,
            "idle_cap_minutes": config.keep_warm.idle_cap.as_secs() / 60,
            "overridden_by_env": keep_warm_pinned_by(|key| std::env::var_os(key).is_some()),
        },
        "persistent": config.persistent,
        "auth_enabled": config.master_key.is_some() || !state.api_key_hashes().is_empty(),
        "version": VERSION,
    }))
}

#[utoipa::path(post, path = "/api/profiles", tag = "profiles", summary = "Create or update a profile", request_body(content = ProfileInput, description = "Profile fields. An omitted or empty api_key keeps the stored key.", content_type = "application/json"), responses((status = 200, description = "The saved profile", body = Object), (status = 400, description = "Invalid request.", body = ErrorBody), (status = 403, description = "Config is read-only (LLM_HUB_CONFIG_READONLY=true).", body = ErrorBody), (status = 500, description = "Internal error.", body = ErrorBody), (status = 401, description = "Missing or invalid hub key.", body = ErrorBody)))]
pub async fn upsert_profile(
    State(state): State<AppState>,
    Json(input): Json<ProfileInput>,
) -> Result<Json<Value>, AppError> {
    upsert_profile_in(&state, input, Path::new(ENV_FILE), reload_config)
}

fn upsert_profile_in(
    state: &AppState,
    input: ProfileInput,
    env_file: &Path,
    reload: impl Fn(&AppState) -> Result<(), AppError>,
) -> Result<Json<Value>, AppError> {
    let name = input.name.trim().to_owned();
    extension_phases::changing(state, ConfigKind::ProfileSaved, Some(&name), || {
        upsert_profile_apply(state, input, env_file, reload)
    })
}

fn upsert_profile_apply(
    state: &AppState,
    input: ProfileInput,
    env_file: &Path,
    reload: impl Fn(&AppState) -> Result<(), AppError>,
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
        keep_warm: input
            .keep_warm
            .or_else(|| existing.map(|p| p.keep_warm))
            .unwrap_or(true),
        first_byte_timeout_ms: existing.and_then(|p| p.first_byte_timeout_ms),
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
    env_writer::upsert_profile(env_file, &profile, &names).map_err(AppError::Internal)?;
    reload(state)?;
    Ok(Json(json!({ "ok": true })))
}

#[utoipa::path(post, path = "/api/fallbacks", tag = "profiles", summary = "Set the default fallback chain", request_body(content = FallbacksInput, description = "Ordered model ids", content_type = "application/json"), responses((status = 200, description = "Saved", body = Object), (status = 400, description = "Invalid request.", body = ErrorBody), (status = 403, description = "Config is read-only (LLM_HUB_CONFIG_READONLY=true).", body = ErrorBody), (status = 500, description = "Internal error.", body = ErrorBody), (status = 401, description = "Missing or invalid hub key.", body = ErrorBody)))]
pub async fn set_default_fallbacks(
    State(state): State<AppState>,
    Json(input): Json<FallbacksInput>,
) -> Result<Json<Value>, AppError> {
    set_default_fallbacks_in(&state, &input, Path::new(ENV_FILE), reload_config)
}

fn set_default_fallbacks_in(
    state: &AppState,
    input: &FallbacksInput,
    env_file: &Path,
    reload: impl Fn(&AppState) -> Result<(), AppError>,
) -> Result<Json<Value>, AppError> {
    extension_phases::changing(state, ConfigKind::DefaultFallbacks, None, || {
        set_default_fallbacks_apply(state, input, env_file, reload)
    })
}

fn set_default_fallbacks_apply(
    state: &AppState,
    input: &FallbacksInput,
    env_file: &Path,
    reload: impl Fn(&AppState) -> Result<(), AppError>,
) -> Result<Json<Value>, AppError> {
    if state.config().config_readonly {
        return Err(AppError::ReadOnly);
    }
    let chain = crate::services::fallback::validate_chain(&input.fallbacks)
        .map_err(AppError::BadRequest)?;
    env_writer::set_default_fallbacks(env_file, &chain).map_err(AppError::Internal)?;
    reload(state)?;
    Ok(Json(json!({ "ok": true })))
}

#[utoipa::path(post, path = "/api/copilot/prices", tag = "copilot", summary = "Set the Copilot price refresh interval", request_body(content = CopilotPricesInput, description = "Refresh interval in seconds; 0 turns the fetch off", content_type = "application/json"), responses((status = 200, description = "Saved", body = Object), (status = 400, description = "Invalid request.", body = ErrorBody), (status = 403, description = "Config is read-only (LLM_HUB_CONFIG_READONLY=true).", body = ErrorBody), (status = 500, description = "Internal error.", body = ErrorBody), (status = 401, description = "Missing or invalid hub key.", body = ErrorBody)))]
pub async fn set_copilot_prices(
    State(state): State<AppState>,
    Json(input): Json<CopilotPricesInput>,
) -> Result<Json<Value>, AppError> {
    set_copilot_prices_in(&state, &input, Path::new(ENV_FILE), reload_config)
}

fn set_copilot_prices_in(
    state: &AppState,
    input: &CopilotPricesInput,
    env_file: &Path,
    reload: impl Fn(&AppState) -> Result<(), AppError>,
) -> Result<Json<Value>, AppError> {
    extension_phases::changing(state, ConfigKind::CopilotPrices, None, || {
        set_copilot_prices_apply(state, input, env_file, reload)
    })
}

fn set_copilot_prices_apply(
    state: &AppState,
    input: &CopilotPricesInput,
    env_file: &Path,
    reload: impl Fn(&AppState) -> Result<(), AppError>,
) -> Result<Json<Value>, AppError> {
    if state.config().config_readonly {
        return Err(AppError::ReadOnly);
    }
    env_writer::set_copilot_prices_refresh(env_file, input.refresh_secs)
        .map_err(AppError::Internal)?;
    reload(state)?;
    state.copilot_prices().request_refresh();
    Ok(Json(json!({ "ok": true })))
}

#[utoipa::path(post, path = "/api/keep-warm", tag = "profiles", summary = "Configure keep-warm pings", request_body(content = KeepWarmInput, description = "Keep-warm switch and idle cap", content_type = "application/json"), responses((status = 200, description = "Saved summary", body = Object), (status = 400, description = "Invalid request.", body = ErrorBody), (status = 403, description = "Config is read-only (LLM_HUB_CONFIG_READONLY=true).", body = ErrorBody), (status = 500, description = "Internal error.", body = ErrorBody), (status = 401, description = "Missing or invalid hub key.", body = ErrorBody)))]
pub async fn set_keep_warm(
    State(state): State<AppState>,
    Json(input): Json<KeepWarmInput>,
) -> Result<Json<Value>, AppError> {
    set_keep_warm_in(&state, &input, Path::new(ENV_FILE), reload_config)
}

fn set_keep_warm_in(
    state: &AppState,
    input: &KeepWarmInput,
    env_file: &Path,
    reload: impl Fn(&AppState) -> Result<(), AppError>,
) -> Result<Json<Value>, AppError> {
    extension_phases::changing(state, ConfigKind::KeepWarm, None, || {
        set_keep_warm_apply(state, input, env_file, reload)
    })
}

fn set_keep_warm_apply(
    state: &AppState,
    input: &KeepWarmInput,
    env_file: &Path,
    reload: impl Fn(&AppState) -> Result<(), AppError>,
) -> Result<Json<Value>, AppError> {
    if state.config().config_readonly {
        return Err(AppError::ReadOnly);
    }
    KeepWarmConfig::check_idle_cap_minutes(input.idle_cap_minutes).map_err(AppError::BadRequest)?;
    env_writer::set_keep_warm(env_file, input.enabled, input.idle_cap_minutes)
        .map_err(AppError::Internal)?;
    reload(state)?;
    Ok(Json(json!({ "ok": true })))
}

/// True when the service environment sets a keep-warm variable, which a saved `.env` value cannot override.
fn keep_warm_pinned_by(is_set: impl Fn(&str) -> bool) -> bool {
    is_set("LLM_HUB_KEEP_WARM") || is_set("LLM_HUB_KEEP_WARM_IDLE_CAP_MINUTES")
}

#[utoipa::path(delete, path = "/api/profiles/{name}", tag = "profiles", summary = "Delete a profile", params(("name" = String, Path, description = "Profile name")), responses((status = 200, description = "Deleted", body = Object), (status = 404, description = "Not found.", body = ErrorBody), (status = 403, description = "Config is read-only (LLM_HUB_CONFIG_READONLY=true).", body = ErrorBody), (status = 500, description = "Internal error.", body = ErrorBody), (status = 401, description = "Missing or invalid hub key.", body = ErrorBody)))]
pub async fn delete_profile(
    State(state): State<AppState>,
    UrlPath(name): UrlPath<String>,
) -> Result<Json<Value>, AppError> {
    delete_profile_in(&state, &name, Path::new(ENV_FILE), reload_config)
}

fn delete_profile_in(
    state: &AppState,
    name: &str,
    env_file: &Path,
    reload: impl Fn(&AppState) -> Result<(), AppError>,
) -> Result<Json<Value>, AppError> {
    extension_phases::changing(state, ConfigKind::ProfileDeleted, Some(name), || {
        delete_profile_apply(state, name, env_file, reload)
    })
}

fn delete_profile_apply(
    state: &AppState,
    name: &str,
    env_file: &Path,
    reload: impl Fn(&AppState) -> Result<(), AppError>,
) -> Result<Json<Value>, AppError> {
    let config = state.config();
    if config.config_readonly {
        return Err(AppError::ReadOnly);
    }
    if config.profile(name).is_none() {
        return Err(AppError::NotFound(format!("no such profile: {name}")));
    }
    let remaining: Vec<String> = config
        .profiles
        .iter()
        .map(|p| p.name.clone())
        .filter(|n| n != name)
        .collect();
    env_writer::remove_profile(env_file, name, &remaining).map_err(AppError::Internal)?;
    reload(state)?;
    Ok(Json(json!({ "ok": true })))
}

#[utoipa::path(post, path = "/api/profiles/{name}/test", tag = "profiles", summary = "Test a profile upstream", params(("name" = String, Path, description = "Profile name")), responses((status = 200, description = "Test result with status and latency", body = Object), (status = 404, description = "Not found.", body = ErrorBody), (status = 401, description = "Missing or invalid hub key.", body = ErrorBody)))]
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
    // A managed profile's server starts on demand; the probe must not run against a stopped one.
    if let Err(e) = state.managed().ensure_running(profile).await {
        state.set_upstream_health(&name, false);
        return Ok(Json(json!({ "ok": false, "error": e })));
    }
    let probe = send_models_request(&state, &client, profile);
    match tokio::time::timeout(crate::consts::MODELS_FANOUT_TIMEOUT, probe).await {
        Ok(Ok(response)) => {
            // A server with no `/v1/models` (whisper-server) is up when the profile lists its models statically.
            let no_models_route = response.status() == reqwest::StatusCode::NOT_FOUND
                && !profile.static_models.is_empty();
            let healthy = response.status().is_success() || no_models_route;
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
#[utoipa::path(post, path = "/api/restart", tag = "system", summary = "Restart the hub", responses((status = 200, description = "Restart requested", body = Object), (status = 401, description = "Missing or invalid hub key.", body = ErrorBody)))]
pub async fn restart_server(State(state): State<AppState>) -> Json<Value> {
    crate::services::restart::request_restart(state.restart_notify());
    Json(json!({ "ok": true, "restarting": true }))
}

#[derive(Debug, Deserialize, Default, IntoParams)]
#[into_params(parameter_in = Query)]
pub struct StatsQuery {
    /// `live` (default), `1d`, `7d`, `30d`, or `all`.
    #[serde(default)]
    pub range: Option<String>,
    pub profile: Option<String>,
    pub model: Option<String>,
    /// Keeps requests whose route starts with this text, for example `smart-auto`.
    pub routed_by: Option<String>,
}

#[utoipa::path(get, path = "/api/stats", tag = "stats", summary = "Request statistics", params(StatsQuery), responses((status = 200, description = "Aggregated request stats", body = Object), (status = 400, description = "Invalid request.", body = ErrorBody), (status = 500, description = "Internal error.", body = ErrorBody), (status = 401, description = "Missing or invalid hub key.", body = ErrorBody)))]
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

    let routed_by_prefix = query
        .routed_by
        .as_ref()
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty());

    let book = state.pricing_book();
    let (snapshot, by_tier_role): (_, Vec<TierRoleRow>) = if range == "live" || !persistent {
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
        snap.overview.keep_warm = in_memory_keep_warm(&state, persistent);
        crate::services::pricing::apply_costs(&mut snap, &book);
        (snap, Vec::new())
    } else {
        let since_ms = since_for_range(range, now_ms())?;
        let store = state
            .store()
            .ok_or_else(|| AppError::Internal("the store is not open".to_owned()))?;
        let filter = StatsFilter {
            since_ms,
            profile,
            model,
            routed_by_prefix,
            range_label: range.to_string(),
        };
        let mut snapshot = store.stats(&filter, &book).map_err(AppError::Internal)?;
        let by_tier_role = store
            .by_tier_role(&filter, &book)
            .map_err(AppError::Internal)?;
        snapshot.overview.keep_warm = stored_keep_warm(state.clone(), since_ms, book).await?;
        (snapshot, by_tier_role)
    };

    let mut body = serde_json::to_value(snapshot)
        .map_err(|e| AppError::Internal(format!("serialize stats failed: {e}")))?;
    body["by_tier_role"] = json!(by_tier_role);
    Ok(Json(body))
}

/// Keep-warm figures since startup, from the in-memory counters.
fn in_memory_keep_warm(state: &AppState, persisted: bool) -> KeepWarmSummary {
    let (pings, cost_usd) = state.keep_warm().ping_totals();
    KeepWarmSummary {
        keepwarm_pings: pings,
        keepwarm_cost_usd: cost_usd,
        persisted,
        ..KeepWarmSummary::default()
    }
}

/// Keep-warm figures for a stored range, read on the blocking pool.
async fn stored_keep_warm(
    state: AppState,
    since_ms: Option<u64>,
    book: PricingBook,
) -> Result<KeepWarmSummary, AppError> {
    tokio::task::spawn_blocking(move || {
        let store = state.store().ok_or("persistence is off")?;
        session_insights::keep_warm_summary(store, since_ms, &book)
    })
    .await
    .map_err(|e| AppError::Internal(format!("keep-warm read task failed: {e}")))?
    .map_err(AppError::Internal)
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
    // SAFETY: `secs` and `tm` are valid for the call, and `localtime_r` keeps no pointer to them.
    if unsafe { libc::localtime_r(&raw const secs, &raw mut tm) }.is_null() {
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

#[utoipa::path(get, operation_id = "usage_history_all", path = "/api/usage", tag = "stats", summary = "Usage history with costs", responses((status = 200, description = "Usage and cost history. Needs persistence.", body = Object), (status = 500, description = "Internal error.", body = ErrorBody), (status = 401, description = "Missing or invalid hub key.", body = ErrorBody)))]
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

#[derive(Debug, Deserialize, Default, IntoParams)]
#[into_params(parameter_in = Query)]
pub struct ErrorsQuery {
    /// `1d`, `7d` (default), `30d`, or `all`.
    #[serde(default)]
    pub range: Option<String>,
}

#[utoipa::path(get, path = "/api/errors", tag = "stats", summary = "Failed requests", params(ErrorsQuery), responses((status = 200, description = "Recent failed requests. Needs persistence.", body = Object), (status = 400, description = "Invalid request.", body = ErrorBody), (status = 500, description = "Internal error.", body = ErrorBody), (status = 401, description = "Missing or invalid hub key.", body = ErrorBody)))]
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
    let (total_errors, mut recent) = store.errors(since_ms).map_err(AppError::Internal)?;
    let solved_errors = store
        .solved_error_count(since_ms)
        .map_err(AppError::Internal)?;
    let mut priced = UsageReport {
        total_requests: total_errors,
        total_errors,
        total_tokens_in: 0,
        total_tokens_out: 0,
        recent: recent.iter().map(|entry| entry.row.clone()).collect(),
    };
    let book = state.pricing_book();
    crate::services::pricing::apply_usage_costs(&mut priced, &book);
    for (entry, row) in recent.iter_mut().zip(priced.recent) {
        entry.row = row;
    }
    let body = serde_json::to_value(ErrorsReport {
        range: range.to_string(),
        total_errors,
        open_errors: total_errors.saturating_sub(solved_errors),
        recent,
    })
    .map_err(|e| AppError::Internal(format!("serialize errors report failed: {e}")))?;
    Ok(Json(body))
}

#[derive(Debug, Deserialize, ToSchema)]
pub struct ResolveErrorsInput {
    /// Request ids from `/api/errors` `recent[].id`.
    pub ids: Vec<i64>,
    /// `true` marks the errors solved, `false` reopens them.
    pub solved: bool,
}

/// Marks failed requests solved (or reopens them). Solved errors stay in the DB for history.
#[utoipa::path(post, path = "/api/errors/resolve", tag = "stats", summary = "Mark errors solved or reopen them", request_body(content = ResolveErrorsInput, description = "Request ids and the solved flag", content_type = "application/json"), responses((status = 200, description = "Updated count", body = Object), (status = 400, description = "Invalid request.", body = ErrorBody), (status = 500, description = "Internal error.", body = ErrorBody), (status = 401, description = "Missing or invalid hub key.", body = ErrorBody)))]
pub async fn resolve_errors(
    State(state): State<AppState>,
    Json(input): Json<ResolveErrorsInput>,
) -> Result<Json<Value>, AppError> {
    let Some(store) = state.store() else {
        return Err(AppError::BadRequest(
            "persistence is off — set LLM_HUB_PERSISTENT=true to track solved errors".into(),
        ));
    };
    if input.ids.is_empty() {
        return Err(AppError::BadRequest("ids must not be empty".into()));
    }
    if input.ids.len() > MAX_RESOLVE_IDS {
        return Err(AppError::BadRequest(format!(
            "at most {MAX_RESOLVE_IDS} ids per request"
        )));
    }
    let resolved_ms = input.solved.then(now_ms);
    let updated = store
        .set_errors_resolved(&input.ids, resolved_ms)
        .map_err(AppError::Internal)?;
    Ok(Json(json!({ "updated": updated, "solved": input.solved })))
}

const MAX_RESOLVE_IDS: usize = 500;

// --- hub api keys ---

#[utoipa::path(get, path = "/api/keys", tag = "keys", summary = "List hub API keys", responses((status = 200, description = "Keys without secrets", body = Object), (status = 500, description = "Internal error.", body = ErrorBody), (status = 401, description = "Missing or invalid hub key.", body = ErrorBody)))]
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
#[utoipa::path(post, path = "/api/keys", tag = "keys", summary = "Create a hub API key", request_body(content = ApiKeyInput, description = "Key name", content_type = "application/json"), responses((status = 200, description = "The new key, shown once", body = Object), (status = 400, description = "Invalid request.", body = ErrorBody), (status = 403, description = "Config is read-only (LLM_HUB_CONFIG_READONLY=true).", body = ErrorBody), (status = 500, description = "Internal error.", body = ErrorBody), (status = 401, description = "Missing or invalid hub key.", body = ErrorBody)))]
pub async fn create_api_key(
    State(state): State<AppState>,
    Json(input): Json<ApiKeyInput>,
) -> Result<Json<Value>, AppError> {
    extension_phases::changing(
        &state,
        ConfigKind::ApiKeyCreated,
        Some(input.name.trim()),
        || create_api_key_apply(&state, &input),
    )
}

fn create_api_key_apply(state: &AppState, input: &ApiKeyInput) -> Result<Json<Value>, AppError> {
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

#[utoipa::path(delete, path = "/api/keys/{name}", tag = "keys", summary = "Delete a hub API key", params(("name" = String, Path, description = "Key name")), responses((status = 200, description = "Deleted", body = Object), (status = 404, description = "Not found.", body = ErrorBody), (status = 403, description = "Config is read-only (LLM_HUB_CONFIG_READONLY=true).", body = ErrorBody), (status = 500, description = "Internal error.", body = ErrorBody), (status = 401, description = "Missing or invalid hub key.", body = ErrorBody)))]
pub async fn delete_api_key(
    State(state): State<AppState>,
    UrlPath(name): UrlPath<String>,
) -> Result<Json<Value>, AppError> {
    extension_phases::changing(&state, ConfigKind::ApiKeyDeleted, Some(&name), || {
        delete_api_key_apply(&state, &name)
    })
}

fn delete_api_key_apply(state: &AppState, name: &str) -> Result<Json<Value>, AppError> {
    let Some(store) = state.store() else {
        return Err(AppError::BadRequest("persistence is off".into()));
    };
    store.delete_api_key(name).map_err(AppError::Internal)?;
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

#[derive(Debug, Deserialize, Default, IntoParams)]
#[into_params(parameter_in = Query)]
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
#[utoipa::path(get, path = "/api/insights", tag = "insights", summary = "Per-model speed, stability and health", params(InsightsQuery), responses((status = 200, description = "Insights report", body = Object), (status = 500, description = "Internal error.", body = ErrorBody), (status = 401, description = "Missing or invalid hub key.", body = ErrorBody)))]
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
#[utoipa::path(get, path = "/api/insights/pick", tag = "insights", summary = "Best model by a criterion", params(InsightsQuery), responses((status = 200, description = "The picked model", body = Object), (status = 500, description = "Internal error.", body = ErrorBody), (status = 401, description = "Missing or invalid hub key.", body = ErrorBody)))]
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
#[utoipa::path(get, path = "/api/insights/report", tag = "insights", summary = "Insights as one document for agents", params(InsightsQuery), responses((status = 200, description = "Insights document", body = Object), (status = 500, description = "Internal error.", body = ErrorBody), (status = 401, description = "Missing or invalid hub key.", body = ErrorBody)))]
pub async fn insights_agent_report(
    State(state): State<AppState>,
    Query(query): Query<InsightsQuery>,
) -> Result<Json<Value>, AppError> {
    let window = insights_window(&state, &query).await?;
    let report = insights::build_agent_report_priced(
        &window.outcomes,
        &window.catalog,
        &window.range,
        window.profile,
        window.min_requests,
        now_ms(),
        &window.evidence,
    );
    serde_json::to_value(report)
        .map(Json)
        .map_err(|e| AppError::Internal(format!("serialize agent report failed: {e}")))
}

struct InsightsWindow {
    outcomes: Vec<RequestOutcome>,
    catalog: HashMap<String, ModelCatalogEntry>,
    evidence: insights::ModelEvidence,
    range: String,
    profile: Option<String>,
    min_requests: u64,
}

async fn insights_report(
    state: &AppState,
    query: &InsightsQuery,
) -> Result<InsightsReport, AppError> {
    let window = insights_window(state, query).await?;
    Ok(insights::build_report_priced(
        &window.outcomes,
        &window.catalog,
        &window.range,
        window.min_requests,
        now_ms(),
        &window.evidence,
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
            routed_by_prefix: None,
            range_label: range.to_string(),
        })
        .map_err(AppError::Internal)?;
    let catalog = insights_catalog(state, profile.as_deref()).await;
    let book = state.pricing_book();
    let costs = model_costs(&outcomes, &book);
    let verdicts = store
        .solver_verdicts(since_ms)
        .map_err(AppError::Internal)?
        .into_iter()
        .filter(|verdict| {
            profile
                .as_deref()
                .is_none_or(|wanted| verdict.model.split('/').next() == Some(wanted))
        })
        .collect();
    Ok(InsightsWindow {
        outcomes,
        catalog,
        evidence: insights::ModelEvidence { costs, verdicts },
        range: range.to_string(),
        profile,
        min_requests: query
            .min_requests
            .unwrap_or(DEFAULT_INSIGHTS_MIN_REQUESTS)
            .max(1),
    })
}

/// Spend and free flag per model over the window. Keep-warm pings cost nothing a user chose, so they are skipped.
fn model_costs(
    outcomes: &[RequestOutcome],
    book: &crate::services::pricing::PricingBook,
) -> insights::ModelCosts {
    let mut costs = insights::ModelCosts::new();
    for outcome in outcomes
        .iter()
        .filter(|outcome| !crate::services::harness::is_keep_warm(outcome.client.as_deref()))
    {
        let entry = costs
            .entry(outcome.model_key.clone())
            .or_insert_with(|| ModelCost {
                usd: 0.0,
                free: book.is_local_model(&outcome.model_key),
            });
        entry.usd += crate::services::pricing::outcome_cost_parts(book, outcome).total();
    }
    costs
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
    use std::path::PathBuf;
    use std::sync::Arc;

    use tempfile::TempDir;
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
    use crate::services::store::Store;

    const DEAD_BASE_URL: &str = "http://127.0.0.1:9";
    const LOGIN_TOKEN: &str = "gho_from_login";
    const FIRST_COPILOT_TOKEN: &str = "tid=first";
    const SECOND_COPILOT_TOKEN: &str = "tid=second";
    const EXCHANGE_PATH: &str = "/copilot_internal/v2/token";
    const STATIC_KEY: &str = "sk-static";

    fn state_for(vars: &[(&str, &str)]) -> AppState {
        state_with(vars, None)
    }

    fn state_with(vars: &[(&str, &str)], hub_store: Option<Store>) -> AppState {
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
            hub_store,
            Arc::new(store),
            Arc::new(gh),
        )
        .unwrap()
    }

    fn persistent_state(kind: &str, vars: &[(&str, &str)]) -> (TempDir, AppState) {
        let dir = tempfile::tempdir().unwrap();
        let db = dir.path().join("hub.db");
        let db = db.to_str().unwrap();
        let mut all = vec![("LLM_HUB_PERSISTENT", "true"), ("LLM_HUB_STORE_PATH", db)];
        all.extend_from_slice(vars);
        let state = state_with(&all, Some(Store::open(kind, Some(db)).unwrap()));
        (dir, state)
    }

    fn env_file(dir: &TempDir, vars: &[(&str, &str)]) -> PathBuf {
        let path = dir.path().join(".env");
        let body: String = vars
            .iter()
            .map(|(key, value)| format!("{key}={value}"))
            .collect::<Vec<_>>()
            .join("\n");
        std::fs::write(&path, body).unwrap();
        path
    }

    fn reload_from(env_file: &Path) -> impl Fn(&AppState) -> Result<(), AppError> + '_ {
        move |state| {
            let vars: HashMap<String, String> = dotenvy::from_path_iter(env_file)
                .unwrap()
                .flatten()
                .collect();
            let config = HubConfig::from_map(&vars).map_err(AppError::Internal)?;
            state.swap_config(config).map_err(AppError::Internal)
        }
    }

    fn outcome(model_key: &str, status: u16) -> RequestOutcome {
        RequestOutcome {
            profile: model_key.split('/').next().unwrap().to_string(),
            model_key: model_key.to_string(),
            status,
            ts_ms: now_ms(),
            latency_ms: 1000,
            ttft_ms: Some(100),
            tokens_in: 1000,
            tokens_out: 100,
            cache_read_tokens: 0,
            cache_write_tokens: 0,
            error: (status >= 400).then(|| format!("{model_key} failed with {status}")),
            client: None,
            worker: None,
            routed_by: None,
            attempt: None,
            role: None,
        }
    }

    #[test]
    fn model_costs_price_calls_skip_keep_warm_and_mark_local_free() {
        let mut book = PricingBook::default();
        book.local_profiles.insert("llama".into());
        let cloud = crate::configs::TokenRates {
            input_per_1m: 1.0,
            ..Default::default()
        };
        book.listed = Arc::new(HashMap::from([(
            "cloud/a".to_string(),
            crate::configs::PriceCard::flat(cloud),
        )]));
        let mut warm = outcome("cloud/a", 200);
        warm.client = Some(crate::consts::KEEP_WARM_CLIENT.into());
        let outcomes = [
            outcome("cloud/a", 200),
            outcome("cloud/a", 200),
            warm,
            outcome("llama/b", 200),
        ];

        let costs = model_costs(&outcomes, &book);

        assert!((costs["cloud/a"].usd - 0.002).abs() < 1e-9);
        assert!(!costs["cloud/a"].free);
        assert!(costs["llama/b"].free);
        assert!(costs["llama/b"].usd.abs() < f64::EPSILON);
    }

    fn record(state: &AppState, outcomes: &[RequestOutcome]) -> Vec<i64> {
        let store = state.store().unwrap();
        outcomes
            .iter()
            .map(|outcome| store.record(outcome).unwrap().unwrap())
            .collect()
    }

    fn profile_input(name: &str, base_url: &str, api_key: Option<&str>) -> ProfileInput {
        ProfileInput {
            name: name.to_string(),
            display_name: None,
            base_url: base_url.to_string(),
            api_key: api_key.map(str::to_string),
            headers: None,
            timeout_ms: None,
            enabled: None,
            keep_warm: None,
            models: None,
        }
    }

    fn assert_saved(result: Result<Json<Value>, AppError>) {
        let Json(body) = result.unwrap();
        assert_eq!(body, json!({"ok": true}));
    }

    fn keys(rows: &Value) -> Vec<String> {
        rows.as_array()
            .unwrap()
            .iter()
            .map(|row| row["key"].as_str().unwrap().to_string())
            .collect()
    }

    fn insights_query(range: Option<&str>, profile: Option<&str>) -> InsightsQuery {
        InsightsQuery {
            range: range.map(str::to_string),
            profile: profile.map(str::to_string),
            min_requests: None,
            by: None,
        }
    }

    #[test]
    fn keep_warm_is_pinned_by_either_variable_only() {
        assert!(keep_warm_pinned_by(|key| key == "LLM_HUB_KEEP_WARM"));
        assert!(keep_warm_pinned_by(
            |key| key == "LLM_HUB_KEEP_WARM_IDLE_CAP_MINUTES"
        ));
        assert!(!keep_warm_pinned_by(|_| false));
    }

    #[tokio::test]
    async fn profiles_list_reports_the_keep_warm_settings() {
        let Json(body) = list_profiles(State(state_for(&[]))).await;
        assert_eq!(body["keep_warm"]["enabled"], json!(true));
        assert_eq!(body["keep_warm"]["idle_cap_minutes"], json!(30));
        assert!(body["keep_warm"]["overridden_by_env"].is_boolean());
    }

    #[tokio::test]
    async fn set_keep_warm_refuses_a_cap_below_five_minutes_and_read_only_hubs() {
        let below_minimum = set_keep_warm(
            State(state_for(&[])),
            Json(KeepWarmInput {
                enabled: true,
                idle_cap_minutes: 4,
            }),
        )
        .await;
        assert!(matches!(below_minimum, Err(AppError::BadRequest(_))));

        let above_maximum = set_keep_warm(
            State(state_for(&[])),
            Json(KeepWarmInput {
                enabled: true,
                idle_cap_minutes: u64::MAX,
            }),
        )
        .await;
        assert!(matches!(above_maximum, Err(AppError::BadRequest(_))));

        let read_only = set_keep_warm(
            State(state_for(&[("LLM_HUB_CONFIG_READONLY", "true")])),
            Json(KeepWarmInput {
                enabled: true,
                idle_cap_minutes: 30,
            }),
        )
        .await;
        assert!(matches!(read_only, Err(AppError::ReadOnly)));
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
    async fn test_profile_counts_a_static_model_server_without_models_route_as_healthy() {
        let upstream = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path(MODELS_PATH))
            .respond_with(ResponseTemplate::new(404))
            .expect(1)
            .mount(&upstream)
            .await;
        let state = state_for(&[
            ("LLM_HUB_PROFILES", "whisper"),
            ("LLM_HUB_WHISPER_BASE_URL", &upstream.uri()),
            ("LLM_HUB_WHISPER_MODELS", "whisper-large-v3"),
        ]);

        let body = probe(&state, "whisper").await;

        assert_eq!(body["ok"], true, "{body}");
        assert_eq!(state.upstream_health().get("whisper").copied(), Some(true));
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

    #[tokio::test]
    async fn test_profile_reports_an_unknown_profile_as_not_found() {
        let result = test_profile(State(state_for(&[])), UrlPath("ghost".into())).await;

        assert!(matches!(result, Err(AppError::NotFound(_))));
    }

    #[tokio::test]
    async fn test_profile_marks_an_unreachable_upstream_unhealthy() {
        let state = state_for(&[
            ("LLM_HUB_PROFILES", "dead"),
            ("LLM_HUB_DEAD_BASE_URL", DEAD_BASE_URL),
        ]);

        let body = probe(&state, "dead").await;

        assert_eq!(body["ok"], false, "{body}");
        assert!(body["error"].is_string(), "{body}");
        assert_eq!(state.upstream_health().get("dead").copied(), Some(false));
    }

    #[tokio::test]
    async fn profiles_list_masks_keys_and_reports_auth() {
        let state = state_for(&[
            ("LLM_HUB_PROFILES", "groq"),
            ("LLM_HUB_GROQ_BASE_URL", DEAD_BASE_URL),
            ("LLM_HUB_GROQ_API_KEY", "sk-groq-secret-value"),
            ("LLM_HUB_GROQ_DISPLAY_NAME", "Groq Cloud"),
            ("LLM_HUB_GROQ_HEADERS", r#"{"x-org":"me"}"#),
            ("LLM_HUB_MASTER_KEY", "secret"),
        ]);

        let Json(body) = list_profiles(State(state)).await;

        let groq = &body["profiles"][0];
        assert_eq!(groq["name"], "groq");
        assert_eq!(groq["display_name"], "Groq Cloud");
        assert_eq!(groq["headers"], json!({"x-org": "me"}));
        assert_ne!(groq["api_key_masked"], "sk-groq-secret-value");
        assert_eq!(groq["managed"], false);
        assert_eq!(groq["copilot"], false);
        assert_eq!(groq["running"], false);
        assert_eq!(body["auth_enabled"], true);
    }

    #[test]
    fn upsert_profile_refuses_a_read_only_hub() {
        let state = state_for(&[("LLM_HUB_CONFIG_READONLY", "true")]);
        let dir = tempfile::tempdir().unwrap();
        let env = env_file(&dir, &[]);

        let result = upsert_profile_in(
            &state,
            profile_input("groq", DEAD_BASE_URL, None),
            &env,
            reload_from(&env),
        );

        assert!(matches!(result, Err(AppError::ReadOnly)));
    }

    #[test]
    fn upsert_profile_rejects_bad_names_and_base_urls() {
        let state = state_for(&[]);
        let dir = tempfile::tempdir().unwrap();
        let env = env_file(&dir, &[]);

        for input in [
            profile_input("a/b", DEAD_BASE_URL, None),
            profile_input("  ", DEAD_BASE_URL, None),
            profile_input("groq", "ftp://example.test", None),
        ] {
            let result = upsert_profile_in(&state, input, &env, reload_from(&env));
            assert!(matches!(result, Err(AppError::BadRequest(_))));
        }
        assert_eq!(std::fs::read_to_string(&env).unwrap(), "");
    }

    #[test]
    fn upsert_profile_refuses_an_api_key_for_a_copilot_profile() {
        let state = state_for(&[
            ("LLM_HUB_PROFILES", "copilot"),
            ("LLM_HUB_COPILOT_BASE_URL", DEAD_BASE_URL),
            ("LLM_HUB_COPILOT_AUTH", CopilotFlow::Vscode.auth_value()),
        ]);
        let dir = tempfile::tempdir().unwrap();
        let env = env_file(&dir, &[]);

        let result = upsert_profile_in(
            &state,
            profile_input("copilot", DEAD_BASE_URL, Some("sk-nope")),
            &env,
            reload_from(&env),
        );

        let Err(AppError::BadRequest(message)) = result else {
            panic!("expected a bad request");
        };
        assert!(message.contains("LLM_HUB_COPILOT_AUTH"), "{message}");
    }

    #[test]
    fn upsert_profile_saves_a_new_profile_and_keeps_its_key_on_update() {
        let state = state_for(&[]);
        let dir = tempfile::tempdir().unwrap();
        let env = env_file(&dir, &[]);
        let mut created = profile_input("groq", "https://api.groq.test/v1/", Some("sk-groq"));
        created.display_name = Some("  Groq  ".into());
        created.timeout_ms = Some(30_000);
        created.keep_warm = Some(false);
        created.models = Some(vec!["llama".into()]);

        assert_saved(upsert_profile_in(&state, created, &env, reload_from(&env)));
        let mut renamed = profile_input("groq", "https://api.groq.test/v2", Some(""));
        renamed.display_name = Some("   ".into());
        assert_saved(upsert_profile_in(&state, renamed, &env, reload_from(&env)));

        let config = state.config();
        let groq = config.profile("groq").unwrap();
        assert_eq!(groq.base_url, "https://api.groq.test/v2");
        assert_eq!(groq.api_key, "sk-groq");
        assert_eq!(groq.display_name, None);
        assert!(!groq.keep_warm);
        assert!(groq.enabled);
    }

    #[test]
    fn upsert_profile_reports_an_unwritable_env_file() {
        let state = state_for(&[]);
        let dir = tempfile::tempdir().unwrap();

        let result = upsert_profile_in(
            &state,
            profile_input("groq", DEAD_BASE_URL, None),
            dir.path(),
            reload_from(dir.path()),
        );

        assert!(matches!(result, Err(AppError::Internal(_))));
    }

    #[test]
    fn delete_profile_removes_it_and_reloads() {
        let vars = [
            ("LLM_HUB_PROFILES", "a,b"),
            ("LLM_HUB_A_BASE_URL", DEAD_BASE_URL),
            ("LLM_HUB_B_BASE_URL", DEAD_BASE_URL),
        ];
        let state = state_for(&vars);
        let dir = tempfile::tempdir().unwrap();
        let env = env_file(&dir, &vars);

        assert_saved(delete_profile_in(&state, "a", &env, reload_from(&env)));

        let config = state.config();
        assert!(config.profile("a").is_none());
        assert!(config.profile("b").is_some());
    }

    #[test]
    fn delete_profile_refuses_unknown_profiles_and_read_only_hubs() {
        let dir = tempfile::tempdir().unwrap();
        let env = env_file(&dir, &[]);

        let missing = delete_profile_in(&state_for(&[]), "ghost", &env, reload_from(&env));
        let read_only = delete_profile_in(
            &state_for(&[("LLM_HUB_CONFIG_READONLY", "true")]),
            "ghost",
            &env,
            reload_from(&env),
        );

        assert!(matches!(missing, Err(AppError::NotFound(_))));
        assert!(matches!(read_only, Err(AppError::ReadOnly)));
    }

    #[test]
    fn set_default_fallbacks_saves_the_trimmed_chain() {
        let state = state_for(&[]);
        let dir = tempfile::tempdir().unwrap();
        let env = env_file(&dir, &[]);
        let input = FallbacksInput {
            fallbacks: vec![" a/m1 ".into(), "b/m2".into()],
        };

        assert_saved(set_default_fallbacks_in(
            &state,
            &input,
            &env,
            reload_from(&env),
        ));

        assert_eq!(state.config().default_fallbacks, vec!["a/m1", "b/m2"]);
    }

    type SavedChanges = std::sync::Arc<std::sync::Mutex<Vec<(ConfigKind, Option<String>)>>>;

    /// Rejects the kinds it is built with, and notes every change it sees saved.
    struct ConfigGate {
        rejects: Vec<ConfigKind>,
        saved: SavedChanges,
    }

    impl crate::ext::Extension for ConfigGate {
        fn name(&self) -> &'static str {
            "config-gate"
        }

        fn on_config_change(&self, ctx: &crate::ext::ConfigChangeCtx<'_>) -> crate::ext::Flow {
            if self.rejects.contains(&ctx.kind()) {
                crate::ext::Flow::reject(403, "settings are frozen")
            } else {
                crate::ext::Flow::Continue
            }
        }

        fn on_config_changed(&self, ctx: &crate::ext::ConfigChangeCtx<'_>) {
            self.saved
                .lock()
                .unwrap()
                .push((ctx.kind(), ctx.subject().map(str::to_owned)));
        }
    }

    fn gate_state(state: &AppState, rejects: Vec<ConfigKind>) -> SavedChanges {
        let saved = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
        let mut all = crate::ext::builtin::builtins();
        all.push(std::sync::Arc::new(ConfigGate {
            rejects,
            saved: std::sync::Arc::clone(&saved),
        }));
        state.set_extensions(crate::ext::ExtensionSet::new(all));
        saved
    }

    #[test]
    fn an_extension_can_stop_a_settings_save_and_sees_the_ones_that_succeed() {
        let state = state_for(&[]);
        let saved = gate_state(&state, vec![ConfigKind::DefaultFallbacks]);
        let dir = tempfile::tempdir().unwrap();
        let env = env_file(&dir, &[]);
        let input = FallbacksInput {
            fallbacks: vec!["a/m1".into()],
        };

        let stopped = set_default_fallbacks_in(&state, &input, &env, reload_from(&env));
        let allowed = set_keep_warm_in(
            &state,
            &KeepWarmInput {
                enabled: true,
                idle_cap_minutes: 30,
            },
            &env,
            reload_from(&env),
        );

        assert!(matches!(
            stopped,
            Err(AppError::Rejected(axum::http::StatusCode::FORBIDDEN, _))
        ));
        assert!(state.config().default_fallbacks.is_empty());
        assert_saved(allowed);
        assert_eq!(*saved.lock().unwrap(), [(ConfigKind::KeepWarm, None)]);
    }

    #[test]
    fn a_failed_save_does_not_reach_the_changed_phase() {
        let state = state_for(&[("LLM_HUB_CONFIG_READONLY", "true")]);
        let saved = gate_state(&state, Vec::new());
        let dir = tempfile::tempdir().unwrap();
        let env = env_file(&dir, &[]);
        let input = FallbacksInput {
            fallbacks: vec!["a/m1".into()],
        };

        let result = set_default_fallbacks_in(&state, &input, &env, reload_from(&env));

        assert!(matches!(result, Err(AppError::ReadOnly)));
        assert!(saved.lock().unwrap().is_empty());
    }

    #[test]
    fn profile_prices_and_delete_saves_name_their_subject() {
        let state = state_for(&[]);
        let saved = gate_state(&state, Vec::new());
        let dir = tempfile::tempdir().unwrap();
        let env = env_file(&dir, &[]);

        assert_saved(upsert_profile_in(
            &state,
            profile_input(" groq ", "https://api.groq.com/openai/v1", Some("gsk_x")),
            &env,
            reload_from(&env),
        ));
        assert_saved(set_copilot_prices_in(
            &state,
            &CopilotPricesInput { refresh_secs: 3600 },
            &env,
            reload_from(&env),
        ));
        assert_saved(delete_profile_in(&state, "groq", &env, reload_from(&env)));

        assert_eq!(
            *saved.lock().unwrap(),
            [
                (ConfigKind::ProfileSaved, Some("groq".to_owned())),
                (ConfigKind::CopilotPrices, None),
                (ConfigKind::ProfileDeleted, Some("groq".to_owned())),
            ]
        );
    }

    #[tokio::test]
    async fn api_key_changes_pass_the_config_phases() {
        let (_dir, state) = persistent_state("sqlite", &[]);
        let saved = gate_state(&state, vec![ConfigKind::ApiKeyDeleted]);

        let Json(created) = create_api_key(
            State(state.clone()),
            Json(ApiKeyInput { name: "ci".into() }),
        )
        .await
        .unwrap();
        assert_eq!(created["name"], "ci");
        let stopped = delete_api_key(State(state.clone()), UrlPath("ci".into())).await;

        assert!(matches!(
            stopped,
            Err(AppError::Rejected(axum::http::StatusCode::FORBIDDEN, _))
        ));
        assert_eq!(
            *saved.lock().unwrap(),
            [(ConfigKind::ApiKeyCreated, Some("ci".to_owned()))]
        );
        assert_eq!(state.api_key_hashes().len(), 1);
    }

    #[test]
    fn set_default_fallbacks_rejects_empty_entries_and_read_only_hubs() {
        let dir = tempfile::tempdir().unwrap();
        let env = env_file(&dir, &[]);
        let input = FallbacksInput {
            fallbacks: vec![" ".into()],
        };

        let invalid = set_default_fallbacks_in(&state_for(&[]), &input, &env, reload_from(&env));
        let read_only = set_default_fallbacks_in(
            &state_for(&[("LLM_HUB_CONFIG_READONLY", "true")]),
            &input,
            &env,
            reload_from(&env),
        );

        assert!(matches!(invalid, Err(AppError::BadRequest(_))));
        assert!(matches!(read_only, Err(AppError::ReadOnly)));
    }

    #[test]
    fn set_copilot_prices_saves_the_refresh_interval() {
        let state = state_for(&[]);
        let dir = tempfile::tempdir().unwrap();
        let env = env_file(&dir, &[]);
        let input = CopilotPricesInput { refresh_secs: 600 };

        assert_saved(set_copilot_prices_in(
            &state,
            &input,
            &env,
            reload_from(&env),
        ));

        assert_eq!(
            state.config().copilot_prices_refresh,
            Some(std::time::Duration::from_mins(10))
        );
    }

    #[test]
    fn set_copilot_prices_refuses_a_read_only_hub() {
        let dir = tempfile::tempdir().unwrap();
        let env = env_file(&dir, &[]);

        let result = set_copilot_prices_in(
            &state_for(&[("LLM_HUB_CONFIG_READONLY", "true")]),
            &CopilotPricesInput { refresh_secs: 0 },
            &env,
            reload_from(&env),
        );

        assert!(matches!(result, Err(AppError::ReadOnly)));
    }

    #[test]
    fn set_keep_warm_saves_the_switch_and_idle_cap() {
        let state = state_for(&[]);
        let dir = tempfile::tempdir().unwrap();
        let env = env_file(&dir, &[]);
        let input = KeepWarmInput {
            enabled: false,
            idle_cap_minutes: 10,
        };

        assert_saved(set_keep_warm_in(&state, &input, &env, reload_from(&env)));

        let config = state.config();
        assert!(!config.keep_warm.enabled);
        assert_eq!(config.keep_warm.idle_cap.as_secs(), 600);
    }

    #[test]
    fn since_for_range_maps_each_label_to_its_lower_bound() {
        let now = 100 * 86_400_000;

        assert!(since_for_range("today", now).unwrap().unwrap() <= now);
        assert_eq!(since_for_range("1d", now).unwrap(), Some(now - 86_400_000));
        assert_eq!(
            since_for_range("7d", now).unwrap(),
            Some(now - 7 * 86_400_000)
        );
        assert_eq!(
            since_for_range("30d", now).unwrap(),
            Some(now - 30 * 86_400_000)
        );
        assert_eq!(since_for_range("all", now).unwrap(), None);
        assert!(matches!(
            since_for_range("90d", now),
            Err(AppError::BadRequest(_))
        ));
    }

    fn stats_query(range: Option<&str>, profile: Option<&str>, model: Option<&str>) -> StatsQuery {
        StatsQuery {
            range: range.map(str::to_string),
            profile: profile.map(str::to_string),
            model: model.map(str::to_string),
            routed_by: Some(" ".into()),
        }
    }

    #[tokio::test]
    async fn live_stats_narrow_rows_to_the_profile_and_model_filters() {
        let state = state_for(&[]);
        state.stats().record(&outcome("a/m1", 200));
        state.stats().record(&outcome("b/m2", 200));

        let Json(by_profile) = stats(
            State(state.clone()),
            Query(stats_query(None, Some(" a "), None)),
        )
        .await
        .unwrap();
        let Json(by_model) = stats(State(state), Query(stats_query(None, None, Some("b/m2"))))
            .await
            .unwrap();

        assert_eq!(keys(&by_profile["profiles"]), vec!["a"]);
        assert_eq!(keys(&by_profile["models"]), vec!["a/m1"]);
        assert_eq!(keys(&by_model["models"]), vec!["b/m2"]);
        assert_eq!(by_model["by_tier_role"], json!([]));
    }

    #[tokio::test]
    async fn stats_fall_back_to_live_without_persistence() {
        let Json(body) = stats(
            State(state_for(&[])),
            Query(stats_query(Some("7d"), None, None)),
        )
        .await
        .unwrap();

        assert_eq!(body["range"], "live");
        assert_eq!(body["filterable"], false);
        assert_eq!(body["overview"]["persisted"], false);
    }

    #[tokio::test]
    async fn stored_stats_read_the_window_from_the_store() {
        let (_dir, state) = persistent_state("sqlite", &[]);
        record(&state, &[outcome("a/m1", 200), outcome("a/m1", 500)]);

        let Json(body) = stats(
            State(state),
            Query(stats_query(Some("7d"), Some("a"), Some("a/m1"))),
        )
        .await
        .unwrap();

        assert_eq!(body["range"], "7d");
        assert_eq!(body["overview"]["requests"], 2);
        assert_eq!(body["overview"]["errors"], 1);
        assert!(body["by_tier_role"].is_array());
    }

    #[tokio::test]
    async fn stored_stats_keep_live_range_and_reject_unknown_ranges() {
        let (_dir, state) = persistent_state("sqlite", &[]);

        let Json(live) = stats(State(state.clone()), Query(stats_query(None, None, None)))
            .await
            .unwrap();
        let unknown = stats(State(state), Query(stats_query(Some("90d"), None, None))).await;

        assert_eq!(live["range"], "live");
        assert_eq!(live["persistent"], true);
        assert_eq!(live["overview"]["persisted"], true);
        assert!(matches!(unknown, Err(AppError::BadRequest(_))));
    }

    #[tokio::test]
    async fn persistence_off_refuses_the_history_endpoints() {
        let state = state_for(&[]);

        let usage = usage(State(state.clone())).await;
        let errors = errors(State(state.clone()), Query(ErrorsQuery::default())).await;
        let resolve = resolve_errors(
            State(state.clone()),
            Json(ResolveErrorsInput {
                ids: vec![1],
                solved: true,
            }),
        )
        .await;
        let create = create_api_key(
            State(state.clone()),
            Json(ApiKeyInput { name: "ci".into() }),
        )
        .await;
        let delete = delete_api_key(State(state.clone()), UrlPath("ci".into())).await;
        let insights = insights(State(state), Query(InsightsQuery::default())).await;

        for result in [usage, errors, resolve, create, delete, insights] {
            assert!(matches!(result, Err(AppError::BadRequest(_))));
        }
    }

    #[tokio::test]
    async fn usage_reports_stored_requests() {
        let (_dir, state) = persistent_state("sqlite", &[]);
        record(&state, &[outcome("a/m1", 200), outcome("a/m1", 502)]);

        let Json(body) = usage(State(state)).await.unwrap();

        assert_eq!(body["total_requests"], 2);
        assert_eq!(body["total_errors"], 1);
    }

    #[tokio::test]
    async fn errors_count_open_failures_until_they_are_resolved() {
        let (_dir, state) = persistent_state("sqlite", &[]);
        let ids = record(
            &state,
            &[
                outcome("a/m1", 200),
                outcome("a/m1", 500),
                outcome("b/m2", 429),
            ],
        );

        let Json(solved) = resolve_errors(
            State(state.clone()),
            Json(ResolveErrorsInput {
                ids: vec![ids[1]],
                solved: true,
            }),
        )
        .await
        .unwrap();
        let Json(report) = errors(
            State(state.clone()),
            Query(ErrorsQuery {
                range: Some("all".into()),
            }),
        )
        .await
        .unwrap();
        let Json(reopened) = resolve_errors(
            State(state),
            Json(ResolveErrorsInput {
                ids: vec![ids[1]],
                solved: false,
            }),
        )
        .await
        .unwrap();

        assert_eq!(solved, json!({"updated": 1, "solved": true}));
        assert_eq!(report["range"], "all");
        assert_eq!(report["total_errors"], 2);
        assert_eq!(report["open_errors"], 1);
        assert_eq!(report["recent"].as_array().unwrap().len(), 2);
        assert_eq!(reopened, json!({"updated": 1, "solved": false}));
    }

    #[tokio::test]
    async fn errors_accept_each_window_and_reject_unknown_ones() {
        let (_dir, state) = persistent_state("sqlite", &[]);
        record(&state, &[outcome("a/m1", 500)]);

        for range in [None, Some("1d"), Some("7d"), Some("30d")] {
            let Json(body) = errors(
                State(state.clone()),
                Query(ErrorsQuery {
                    range: range.map(str::to_string),
                }),
            )
            .await
            .unwrap();
            assert_eq!(body["total_errors"], 1, "{range:?}");
        }
        let unknown = errors(
            State(state),
            Query(ErrorsQuery {
                range: Some("90d".into()),
            }),
        )
        .await;
        assert!(matches!(unknown, Err(AppError::BadRequest(_))));
    }

    #[tokio::test]
    async fn resolve_errors_bounds_the_id_list() {
        let (_dir, state) = persistent_state("sqlite", &[]);

        let empty = resolve_errors(
            State(state.clone()),
            Json(ResolveErrorsInput {
                ids: Vec::new(),
                solved: true,
            }),
        )
        .await;
        let too_many = resolve_errors(
            State(state),
            Json(ResolveErrorsInput {
                ids: (0..=i64::try_from(MAX_RESOLVE_IDS).unwrap()).collect(),
                solved: true,
            }),
        )
        .await;

        assert!(matches!(empty, Err(AppError::BadRequest(_))));
        assert!(matches!(too_many, Err(AppError::BadRequest(_))));
    }

    #[tokio::test]
    async fn resolve_errors_reports_a_store_without_row_ids() {
        let (_dir, state) = persistent_state("json", &[]);

        let result = resolve_errors(
            State(state),
            Json(ResolveErrorsInput {
                ids: vec![1],
                solved: true,
            }),
        )
        .await;

        assert!(matches!(result, Err(AppError::Internal(_))));
    }

    #[tokio::test]
    async fn api_keys_are_listed_without_persistence_as_empty() {
        let Json(body) = list_api_keys(State(state_for(&[]))).await.unwrap();

        assert_eq!(body, json!({"persistent": false, "keys": []}));
    }

    #[tokio::test]
    async fn api_keys_are_created_once_listed_masked_and_deleted() {
        let (_dir, state) = persistent_state("sqlite", &[]);

        let Json(created) = create_api_key(
            State(state.clone()),
            Json(ApiKeyInput {
                name: " ci ".into(),
            }),
        )
        .await
        .unwrap();
        let duplicate = create_api_key(
            State(state.clone()),
            Json(ApiKeyInput { name: "ci".into() }),
        )
        .await;
        let Json(listed) = list_api_keys(State(state.clone())).await.unwrap();
        let hashes_after_create = state.api_key_hashes().len();
        assert_saved(delete_api_key(State(state.clone()), UrlPath("ci".into())).await);
        let Json(after_delete) = list_api_keys(State(state.clone())).await.unwrap();

        let key = created["key"].as_str().unwrap();
        assert_eq!(created["name"], "ci");
        assert!(key.starts_with("sk-hub-"), "{key}");
        assert_eq!(key.len(), "sk-hub-".len() + 48);
        assert!(matches!(duplicate, Err(AppError::BadRequest(_))));
        assert_eq!(listed["keys"][0]["name"], "ci");
        assert_ne!(listed["keys"][0]["masked"], key);
        assert_eq!(listed["keys"][0]["enabled"], true);
        assert_eq!(hashes_after_create, 1);
        assert_eq!(after_delete["keys"], json!([]));
        assert!(state.api_key_hashes().is_empty());
    }

    #[tokio::test]
    async fn create_api_key_rejects_a_blank_name() {
        let (_dir, state) = persistent_state("sqlite", &[]);

        let result = create_api_key(State(state), Json(ApiKeyInput { name: "  ".into() })).await;

        assert!(matches!(result, Err(AppError::BadRequest(_))));
    }

    const INSIGHT_PROFILES: [(&str, &str); 5] = [
        ("LLM_HUB_PROFILES", "a,b,c"),
        ("LLM_HUB_A_BASE_URL", DEAD_BASE_URL),
        ("LLM_HUB_B_BASE_URL", DEAD_BASE_URL),
        ("LLM_HUB_C_BASE_URL", DEAD_BASE_URL),
        ("LLM_HUB_C_ENABLED", "false"),
    ];

    fn insights_state() -> (TempDir, AppState) {
        let (dir, state) = persistent_state("sqlite", &INSIGHT_PROFILES);
        state.set_model_cache(json!({"data": [
            {"id": "a/m1"},
            {"id": "b/m2"},
            {"id": "c/m3"},
        ]}));
        let calls: Vec<RequestOutcome> = (0..5).map(|_| outcome("a/m1", 200)).collect();
        record(&state, &calls);
        (dir, state)
    }

    fn model_ids(models: &Value) -> Vec<String> {
        let mut ids: Vec<String> = models
            .as_array()
            .unwrap()
            .iter()
            .map(|model| model["model"].as_str().unwrap().to_string())
            .collect();
        ids.sort();
        ids
    }

    #[tokio::test]
    async fn insights_list_enabled_catalog_models_beside_measured_ones() {
        let (_dir, state) = insights_state();

        let Json(body) = insights(State(state), Query(insights_query(Some("all"), None)))
            .await
            .unwrap();

        assert_eq!(body["range"], "all");
        assert_eq!(model_ids(&body["models"]), vec!["a/m1", "b/m2"]);
    }

    #[tokio::test]
    async fn insights_accept_each_window_and_reject_unknown_ones() {
        let (_dir, state) = insights_state();

        for range in ["1d", "7d", "30d"] {
            let Json(body) = insights(
                State(state.clone()),
                Query(insights_query(Some(range), None)),
            )
            .await
            .unwrap();
            assert_eq!(body["range"], range);
        }
        let unknown = insights(State(state), Query(insights_query(Some("90d"), None))).await;
        assert!(matches!(unknown, Err(AppError::BadRequest(_))));
    }

    #[tokio::test]
    async fn insights_pick_returns_the_ranked_model_or_not_found() {
        let (_dir, state) = insights_state();
        let mut strict = insights_query(None, None);
        strict.min_requests = Some(50);

        let Json(picked) = insights_pick(State(state.clone()), Query(insights_query(None, None)))
            .await
            .unwrap();
        let none = insights_pick(State(state), Query(strict)).await;

        assert_eq!(picked["model"], "a/m1");
        assert_eq!(picked["by"], "overall");
        assert!(matches!(none, Err(AppError::NotFound(_))));
    }

    #[tokio::test]
    async fn insights_agent_report_narrows_to_the_profile() {
        let (_dir, state) = insights_state();

        let Json(body) =
            insights_agent_report(State(state), Query(insights_query(None, Some(" b "))))
                .await
                .unwrap();

        assert_eq!(body["profile"], "b");
        assert_eq!(model_ids(&body["models"]), vec!["b/m2"]);
    }

    #[tokio::test]
    async fn insights_refresh_a_cold_model_list() {
        let (_dir, state) = persistent_state("sqlite", &[]);

        let Json(body) = insights(State(state.clone()), Query(insights_query(None, None)))
            .await
            .unwrap();

        assert_eq!(body["models"], json!([]));
        assert!(state.model_cache().is_some());
    }
}
