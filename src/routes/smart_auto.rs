use axum::Json;
use axum::body::Bytes;
use axum::extract::{Query, State};
use axum::response::{IntoResponse, Response};
use std::collections::HashMap;
use utoipa::IntoParams;
use utoipa::ToSchema;

use serde::{Deserialize, Serialize};

use crate::consts::{
    AUTO_DECISIONS_DEFAULT_LIMIT, AUTO_DECISIONS_MAX_LIMIT, SELECTION_HISTORY_DEFAULT_LIMIT,
    SELECTION_HISTORY_MAX_LIMIT,
};
use crate::dependencies::state::AppState;
use crate::ext::ConfigKind;
use crate::routes::proxy::{concrete_profiles, hub_policy};
use crate::schemas::affinity_key::AffinityKey;
use crate::schemas::app_error::AppError;
use crate::schemas::attempt_outcome::{AttemptOutcome, OutcomeCode, OutcomeSource};
use crate::schemas::current_selection::CurrentSelection;
use crate::schemas::effort_level::EffortLevel;
use crate::schemas::error_body::ErrorBody;
use crate::schemas::history_query::HistoryQuery;
use crate::schemas::hub_selector::HubSelector;
use crate::schemas::launch::profile_filter::ProfileFilter;
use crate::schemas::model_context::strip_1m_suffix;
use crate::schemas::model_descriptor::ModelDescriptor;
use crate::schemas::role_decision::RoleDecision;
use crate::schemas::role_reason::RoleReason;
use crate::schemas::route_reason::RouteReason;
use crate::schemas::scope_kind::ScopeKind;
use crate::schemas::selection_event::SelectionEvent;
use crate::schemas::selection_stage::SelectionStage;
use crate::schemas::selection_state::SelectionState;
use crate::schemas::selection_why::SelectionWhy;
use crate::schemas::smart_auto_decision::SmartAutoDecision;
use crate::schemas::smart_auto_headers::SmartAutoHeaders;
use crate::schemas::smart_auto_policy::SmartAutoPolicy;
use crate::schemas::smart_auto_role_tiers::{ROLE_TIER_DEFAULTS, role_from_name};
use crate::schemas::smart_auto_rules::SmartAutoRules;
use crate::schemas::smart_auto_tier::SmartAutoTier;
use crate::schemas::turn_signals::TurnSignals;
use crate::services::extension_phases;
use crate::services::model_health::{HealthRow, ProfileCooldownRow};
use crate::services::selection_log::SelectionLog;
use crate::services::smart_auto_live::SmartAutoLive;
use crate::services::smart_auto_pool::SmartAutoPool;
use crate::services::smart_auto_router::SmartAutoRouter;
use crate::services::smart_auto_rules::{LoadedRules, RuleEffect, RulesSource, rule_effects};
use crate::services::store::now_ms;
use std::collections::{BTreeMap, BTreeSet};
use std::time::Instant;

#[derive(Debug, Deserialize, Default, IntoParams)]
#[into_params(parameter_in = Query)]
pub struct CurrentQuery {
    pub worker: Option<String>,
    pub session: Option<String>,
}

#[derive(Debug, Deserialize, Default, IntoParams)]
#[into_params(parameter_in = Query)]
pub struct HistoryParams {
    pub workflow: Option<String>,
    pub worker: Option<String>,
    pub since_seq: Option<u64>,
    /// Read the saved history from this time on, instead of the in-memory ring.
    pub since_ts_ms: Option<u64>,
    pub limit: Option<usize>,
}

#[derive(Debug, Deserialize, Default, IntoParams)]
#[into_params(parameter_in = Query)]
pub struct DecisionsQuery {
    pub limit: Option<usize>,
}

#[derive(Debug, Deserialize, Default, IntoParams)]
#[into_params(parameter_in = Query)]
pub struct AttemptParams {
    pub attempt: Option<String>,
}

#[derive(Serialize)]
struct AttemptTextBody {
    attempt: String,
    prompt: Option<String>,
    answer: Option<String>,
}

#[derive(Serialize)]
struct CurrentBody {
    /// `null` until the worker has a selection.
    selection: Option<CurrentSelection>,
}

#[derive(Serialize)]
struct HistoryBody {
    events: Vec<HistoryEvent>,
}

/// A history event, with the verified outcome of its attempt on completed solver events.
#[derive(Serialize)]
struct HistoryEvent {
    #[serde(flatten)]
    event: SelectionEvent,
    #[serde(skip_serializing_if = "Option::is_none")]
    outcome: Option<AttemptOutcome>,
}

#[derive(Debug, Deserialize, ToSchema)]
pub struct OutcomeBody {
    /// The salted attempt key, as history shows it.
    pub attempt: Option<String>,
    /// The caller's raw Claude Code session id; the hub salts it into a worker key.
    pub session_id: Option<String>,
    /// An already-salted worker key, as history shows it.
    pub worker: Option<String>,
    pub code: OutcomeCode,
}

#[derive(Serialize)]
struct DecisionsBody {
    decisions: Vec<SmartAutoDecision>,
}

#[derive(Serialize)]
struct HealthBody {
    /// Each `(model, endpoint)` the hub attempted: failures, cooldown, error rate and speed.
    models: Vec<HealthRow>,
    /// Profiles skipped as a whole now, such as one out of credits, with the reason and when it ends.
    profiles: Vec<ProfileCooldownRow>,
    /// Cost score (0 dearest, 10 free) of each priced model, from the price book.
    costs: std::collections::BTreeMap<String, u8>,
}

#[derive(Serialize)]
struct RulesBody<'a> {
    source: RulesSource,
    readonly: bool,
    rules: &'a SmartAutoRules,
}

#[derive(Serialize)]
struct EffectBody {
    rows: Vec<RuleEffect>,
}

/// `GET /api/smart-auto/current?worker=<id>` or `?session=<id>`: the worker's main model, its active stages and freshness.
#[utoipa::path(get, path = "/api/smart-auto/current", tag = "smart-auto", summary = "Current selection of a worker", params(CurrentQuery), responses((status = 200, description = "The worker selection, or null", body = Object), (status = 400, description = "Invalid request.", body = ErrorBody), (status = 401, description = "Missing or invalid hub key.", body = ErrorBody)))]
pub async fn current(State(state): State<AppState>, Query(query): Query<CurrentQuery>) -> Response {
    let Some(raw) = non_empty(query.worker).or_else(|| non_empty(query.session)) else {
        return AppError::BadRequest("worker or session is required".into()).into_response();
    };
    let log = state.selection_log();
    let now = now_ms();
    let selection = log
        .current(&log.scope_key(ScopeKind::Worker, &raw), now)
        .or_else(|| {
            is_scope_key_prefix(&raw)
                .then(|| log.current_by_prefix(&raw, now))
                .flatten()
        });
    Json(CurrentBody { selection }).into_response()
}

/// A lowercase hex prefix of a scope key, as the history view shows it: 8 to 64 characters.
fn is_scope_key_prefix(raw: &str) -> bool {
    (8..=64).contains(&raw.len())
        && raw
            .bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
}

/// `GET /api/smart-auto/history?workflow=&worker=&since_seq=&limit=`: selection events after `since_seq`, oldest first.
#[utoipa::path(get, path = "/api/smart-auto/history", tag = "smart-auto", summary = "Selection events", params(HistoryParams), responses((status = 200, description = "Events after since_seq, oldest first", body = Object), (status = 401, description = "Missing or invalid hub key.", body = ErrorBody)))]
pub async fn history(
    State(state): State<AppState>,
    Query(params): Query<HistoryParams>,
) -> Response {
    let log = state.selection_log();
    let query = HistoryQuery {
        workflow: non_empty(params.workflow).map(|raw| log.scope_key(ScopeKind::Workflow, &raw)),
        worker: non_empty(params.worker).map(|raw| log.scope_key(ScopeKind::Worker, &raw)),
        since_seq: params.since_seq.unwrap_or(0),
        limit: params
            .limit
            .unwrap_or(SELECTION_HISTORY_DEFAULT_LIMIT)
            .clamp(1, SELECTION_HISTORY_MAX_LIMIT),
    };
    let events = match (params.since_ts_ms, state.store()) {
        (Some(since_ts_ms), Some(_)) => {
            let saved = state.clone();
            let read = tokio::task::spawn_blocking(move || {
                let store = saved.store().ok_or("persistence is off")?;
                SelectionLog::stored_history(store, &query, since_ts_ms)
            })
            .await;
            match read {
                Ok(Ok(events)) => events,
                Ok(Err(e)) => return AppError::Internal(e).into_response(),
                Err(e) => return AppError::Internal(e.to_string()).into_response(),
            }
        }
        _ => log.history(&query),
    };
    let events = match with_outcomes(&state, events).await {
        Ok(events) => events,
        Err(e) => return AppError::Internal(e).into_response(),
    };
    Json(HistoryBody { events }).into_response()
}

/// Attaches stored outcomes to completed solver events. Without a store, no event has one.
async fn with_outcomes(
    state: &AppState,
    events: Vec<SelectionEvent>,
) -> Result<Vec<HistoryEvent>, String> {
    let attempts: Vec<String> = events
        .iter()
        .filter(|event| {
            event.stage == SelectionStage::Solver && event.state == SelectionState::Completed
        })
        .filter_map(|event| event.attempt.clone())
        .collect();
    let outcomes = if attempts.is_empty() || state.store().is_none() {
        HashMap::new()
    } else {
        let saved = state.clone();
        tokio::task::spawn_blocking(move || {
            saved.store().map_or_else(
                || Ok(HashMap::new()),
                |store| store.load_attempt_outcomes(&attempts),
            )
        })
        .await
        .map_err(|e| e.to_string())??
    };
    Ok(events
        .into_iter()
        .map(|event| {
            let outcome = (event.stage == SelectionStage::Solver
                && event.state == SelectionState::Completed)
                .then(|| {
                    event
                        .attempt
                        .as_ref()
                        .and_then(|a| outcomes.get(a).cloned())
                })
                .flatten();
            HistoryEvent { event, outcome }
        })
        .collect())
}

/// `POST /api/smart-auto/outcome`: records a client-reported outcome for an attempt, or for the
/// worker's last completed attempt. A client outcome overrides review and heuristic ones.
#[utoipa::path(post, path = "/api/smart-auto/outcome", tag = "smart-auto", summary = "Report an attempt outcome", request_body(content = OutcomeBody, description = "The attempt, session or worker, and the outcome code", content_type = "application/json"), responses((status = 200, description = "Recorded", body = Object), (status = 400, description = "Invalid request.", body = ErrorBody), (status = 404, description = "Not found.", body = ErrorBody), (status = 401, description = "Missing or invalid hub key.", body = ErrorBody)))]
pub async fn report_outcome(
    State(state): State<AppState>,
    Json(body): Json<OutcomeBody>,
) -> Response {
    let log = state.selection_log();
    let worker = non_empty(body.worker)
        .or_else(|| non_empty(body.session_id).map(|raw| log.scope_key(ScopeKind::Worker, &raw)));
    let attempt = match (non_empty(body.attempt), &worker) {
        (Some(attempt), _) => attempt,
        (None, Some(worker)) => match state.attempt_tracker().last_completed(worker) {
            Some(attempt) => attempt,
            None => {
                return AppError::NotFound("this worker has no completed attempt".to_owned())
                    .into_response();
            }
        },
        (None, None) => {
            return AppError::BadRequest("name an attempt, session_id or worker".to_owned())
                .into_response();
        }
    };
    let Some(_) = state.store() else {
        return AppError::BadRequest("persistence is off".to_owned()).into_response();
    };
    let outcome = AttemptOutcome {
        code: body.code,
        source: OutcomeSource::Client,
    };
    let saved = state.clone();
    let key = attempt.clone();
    let stored = outcome.clone();
    let write = tokio::task::spawn_blocking(move || {
        let store = saved.store().ok_or("persistence is off")?;
        store.save_attempt_outcome(&key, worker.as_deref(), &stored, now_ms())
    })
    .await;
    match write {
        Ok(Ok(())) => {
            Json(serde_json::json!({ "attempt": attempt, "outcome": outcome })).into_response()
        }
        Ok(Err(e)) => AppError::Internal(e).into_response(),
        Err(e) => AppError::Internal(e.to_string()).into_response(),
    }
}

/// `GET /api/smart-auto/decisions?limit=N`: the newest Smart Auto routes, newest first.
#[utoipa::path(get, path = "/api/smart-auto/decisions", tag = "smart-auto", summary = "Recent Smart Auto routes", params(DecisionsQuery), responses((status = 200, description = "Routes, newest first", body = Object), (status = 401, description = "Missing or invalid hub key.", body = ErrorBody)))]
pub async fn decisions(
    State(state): State<AppState>,
    Query(query): Query<DecisionsQuery>,
) -> Response {
    let limit = query
        .limit
        .unwrap_or(AUTO_DECISIONS_DEFAULT_LIMIT)
        .clamp(1, AUTO_DECISIONS_MAX_LIMIT);
    Json(DecisionsBody {
        decisions: state.smart_auto_decisions().recent(limit),
    })
    .into_response()
}

/// `GET /api/smart-auto/health`: what Smart Auto measured about each model. A model with
/// `cooled_for_ms` above 0 is skipped until it elapses.
#[utoipa::path(get, path = "/api/smart-auto/health", tag = "smart-auto", summary = "Per-model measurements", responses((status = 200, description = "Failures, cooldown, error rate, speed and cost scores", body = Object), (status = 401, description = "Missing or invalid hub key.", body = ErrorBody)))]
pub async fn health(State(state): State<AppState>) -> Response {
    let now = std::time::Instant::now();
    Json(HealthBody {
        models: state.model_health().report(now),
        profiles: state.model_health().profile_report(now),
        costs: state
            .model_costs()
            .iter()
            .map(|(id, score)| (id.clone(), *score))
            .collect(),
    })
    .into_response()
}

/// `GET /api/smart-auto/rules`: the active allow, exclude and include rules, their source and whether they are read-only.
#[utoipa::path(get, path = "/api/smart-auto/rules", tag = "smart-auto", summary = "Active rules", responses((status = 200, description = "Rules, their source and the read-only flag", body = Object), (status = 401, description = "Missing or invalid hub key.", body = ErrorBody)))]
pub async fn get_rules(State(state): State<AppState>) -> Response {
    rules_response(&state.smart_auto_rules().current())
}

/// `PUT /api/smart-auto/rules`: validates the body, saves it atomically and returns the saved rules.
#[utoipa::path(put, path = "/api/smart-auto/rules", tag = "smart-auto", summary = "Replace the rules", request_body(content = Object, description = "Smart Auto rules document", content_type = "application/json"), responses((status = 200, description = "The saved rules", body = Object), (status = 400, description = "Invalid request.", body = ErrorBody), (status = 403, description = "Config is read-only (LLM_HUB_CONFIG_READONLY=true).", body = ErrorBody), (status = 500, description = "Internal error.", body = ErrorBody), (status = 401, description = "Missing or invalid hub key.", body = ErrorBody)))]
pub async fn put_rules(State(state): State<AppState>, body: Bytes) -> Response {
    let saved = extension_phases::changing(&state, ConfigKind::SmartAutoRules, None, || {
        state.smart_auto_rules().replace(&body)
    });
    match saved {
        Ok(loaded) => rules_response(&loaded),
        Err(error) => error.into_response(),
    }
}

/// `GET /api/smart-auto/rules/effect`: each model's `eligible`, `reason` and `matched` glob under the current rules.
/// Reasons are checked in this order: `not_chat`, `auto_alias`, `hub_namespace`, `allowed_override`, `excluded_by_global`, `excluded_by_profile`, `not_in_profile_include`, `not_in_global_include`, `eligible`.
#[utoipa::path(get, path = "/api/smart-auto/rules/effect", tag = "smart-auto", summary = "Effect of the rules per model", responses((status = 200, description = "Each model with eligible, reason and matched glob", body = Object), (status = 401, description = "Missing or invalid hub key.", body = ErrorBody)))]
pub async fn rules_effect(State(state): State<AppState>) -> Response {
    let rules = state.smart_auto_rules().current();
    let rows = rule_effects(&state.model_descriptors(), rules.policy());
    Json(EffectBody { rows }).into_response()
}

fn rules_response(loaded: &LoadedRules) -> Response {
    Json(RulesBody {
        source: loaded.source,
        readonly: loaded.readonly(),
        rules: &loaded.rules,
    })
    .into_response()
}

/// `GET /api/smart-auto/attempt?attempt=<key>`: the stored prompt and answer text of one dispatch attempt.
/// Both are `null` when none was kept, for example when persistence is off or the attempt predates capture.
#[utoipa::path(get, path = "/api/smart-auto/attempt", tag = "smart-auto", summary = "Stored text of one attempt", params(AttemptParams), responses((status = 200, description = "Prompt and answer text, each null when none was kept", body = Object), (status = 400, description = "Invalid request.", body = ErrorBody), (status = 401, description = "Missing or invalid hub key.", body = ErrorBody)))]
pub async fn attempt_text(
    State(state): State<AppState>,
    Query(params): Query<AttemptParams>,
) -> Response {
    let Some(attempt) = non_empty(params.attempt) else {
        return AppError::BadRequest("attempt is required".into()).into_response();
    };
    let Some(store) = state.store() else {
        return Json(AttemptTextBody {
            attempt,
            prompt: None,
            answer: None,
        })
        .into_response();
    };
    match store.load_selection_text(&attempt) {
        Ok(found) => {
            let (prompt, answer) = found.map_or((None, None), |(prompt, answer)| {
                (Some(prompt), Some(answer))
            });
            Json(AttemptTextBody {
                attempt,
                prompt,
                answer,
            })
            .into_response()
        }
        Err(e) => AppError::Internal(format!("selection text cannot be read: {e}")).into_response(),
    }
}

/// The body of `POST /api/smart-auto/resolve`. Every field is optional.
#[derive(Debug, Deserialize, Default, ToSchema)]
#[serde(default, deny_unknown_fields)]
pub struct ResolveBody {
    pub tier: Option<String>,
    pub role: Option<String>,
    pub worker: Option<String>,
    pub session: Option<String>,
    pub include: Vec<String>,
    pub exclude: Vec<String>,
    pub min_context: Option<u64>,
    pub text: Option<String>,
    /// Comma-separated profiles, the same filter as the `x-llm-hub-profiles` header.
    pub profiles: Option<String>,
    /// The parent workflow, the same as the `x-llm-hub-smart-auto-parent` header.
    pub parent: Option<String>,
    /// The parent policy handle, the same as the `x-llm-hub-smart-auto-policy` header.
    pub policy_handle: Option<String>,
}

#[derive(Serialize)]
struct ResolveReply {
    model: String,
    tier: &'static str,
    role: &'static str,
    plan_backed: bool,
    effort: Option<&'static str>,
    reason: &'static str,
    pinned: bool,
    alternatives: Vec<String>,
    why: SelectionWhy,
}

#[derive(Serialize)]
struct TierRow {
    name: &'static str,
    selector: String,
    pool: Vec<String>,
}

#[derive(Serialize)]
struct TiersReply {
    default_tier: &'static str,
    tiers: Vec<TierRow>,
    roles: Vec<&'static str>,
    role_tier_defaults: BTreeMap<&'static str, &'static str>,
}

/// The client route a dry run resolves for when no pin exists; affinity keys include it.
const RESOLVE_ENDPOINT: &str = "/v1/messages";
/// Every client route a pin can be kept under. A dry run looks up each and uses the latest.
const PIN_ENDPOINTS: [&str; 5] = [
    "/v1/messages",
    "/v1/chat/completions",
    "/chat/completions",
    "/v1/responses",
    "/responses",
];
/// How many other eligible models a dry run lists.
const RESOLVE_ALTERNATIVES: usize = 4;

/// `POST /api/smart-auto/resolve`: the model a `hub/smart-auto-<tier>` turn would get, without
/// calling an upstream or recording anything. With `worker` or `session`, the dry run reports a kept pin, whichever client route set it.
#[utoipa::path(post, path = "/api/smart-auto/resolve", tag = "smart-auto", summary = "Dry-run a Smart Auto route", request_body(content = ResolveBody, description = "Tier, role and optional worker or session", content_type = "application/json"), responses((status = 200, description = "The model the turn would get, without calling an upstream or recording anything", body = Object), (status = 400, description = "Invalid request.", body = ErrorBody), (status = 401, description = "Missing or invalid hub key.", body = ErrorBody)))]
pub async fn resolve(State(state): State<AppState>, Json(body): Json<ResolveBody>) -> Response {
    match resolve_dry_run(&state, body) {
        Ok(reply) => Json(reply).into_response(),
        Err(error) => error.into_response(),
    }
}

/// The caller layer a dry run stands for: the tier, role and filters the body names.
fn resolve_caller(body: &mut ResolveBody) -> Result<SmartAutoHeaders, AppError> {
    let tier = match non_empty(body.tier.take()) {
        None => SmartAutoTier::default(),
        Some(name) => SmartAutoTier::from_name(&name)
            .ok_or_else(|| AppError::BadRequest(format!("unknown tier {name:?}")))?,
    };
    let role = non_empty(body.role.take())
        .map(|name| {
            role_from_name(&name)
                .ok_or_else(|| AppError::BadRequest(format!("unknown role {name:?}")))
        })
        .transpose()?;
    let globs = |list: Vec<String>| -> BTreeSet<String> {
        list.into_iter().filter(|glob| !glob.is_empty()).collect()
    };
    let include = globs(std::mem::take(&mut body.include));
    Ok(SmartAutoHeaders {
        policy: SmartAutoPolicy {
            tier: Some(tier),
            include: if include.is_empty() {
                Vec::new()
            } else {
                vec![include]
            },
            exclude: globs(std::mem::take(&mut body.exclude)),
            min_context_tokens: body.min_context,
            role,
            ..SmartAutoPolicy::default()
        },
        worker: non_empty(body.worker.take()),
        parent: non_empty(body.parent.take()),
        policy_handle: non_empty(body.policy_handle.take()),
        ..SmartAutoHeaders::default()
    })
}

fn resolve_dry_run(state: &AppState, mut body: ResolveBody) -> Result<ResolveReply, AppError> {
    let caller = resolve_caller(&mut body)?;
    let tier = caller.policy.tier.unwrap_or_default();
    let config = state.config();
    let selector = tier_selector(&config.smart_auto_presets, tier)?;
    let rules = state.smart_auto_rules().current();
    let policy = hub_policy(&rules, &selector, &caller);
    let text = non_empty(body.text);
    let signals = TurnSignals {
        estimated_prompt_tokens: 0,
        tool_continuation: false,
        user_turns: usize::from(text.is_some()),
        last_user_text: text,
        has_tools: false,
        has_image: false,
        session_id: non_empty(body.session),
        prompt_cache_key: None,
        content_key: None,
    };
    let now = Instant::now();
    let affinity = state.smart_auto_affinity();
    let pinned = PIN_ENDPOINTS
        .into_iter()
        .filter_map(|endpoint| {
            let key = AffinityKey::new(&signals, &caller, &policy, endpoint)?;
            Some((affinity.pin_last_used(&key, now)?, endpoint, key))
        })
        .max_by_key(|(last_used, _, _)| *last_used);
    let (endpoint, key) = match pinned {
        Some((_, endpoint, key)) => (endpoint, Some(key)),
        None => (
            RESOLVE_ENDPOINT,
            AffinityKey::new(&signals, &caller, &policy, RESOLVE_ENDPOINT),
        ),
    };
    let snapshot = state.model_descriptors();
    let mut pool = SmartAutoPool::eligible(&snapshot, &policy, endpoint, 0);
    if let Some(profiles) = body
        .profiles
        .as_deref()
        .and_then(|list| concrete_profiles(ProfileFilter::from_list(list)))
    {
        pool.retain(|descriptor| profiles.allows(&descriptor.qualified_id));
    }
    let live = SmartAutoLive::collect(state, endpoint, &pool, now);
    let sits_out = |descriptor: &&ModelDescriptor| {
        live.unavailable
            .contains(strip_1m_suffix(&descriptor.qualified_id))
    };
    if pool.iter().any(|descriptor| !sits_out(descriptor)) {
        pool.retain(|descriptor| !sits_out(descriptor));
    }
    let detached = key.as_ref().map(|key| affinity.detached(key));
    let anchor = detached.as_ref().zip(key.as_ref());
    let usage = state.model_usage();
    let decision = SmartAutoRouter::route(
        &pool,
        &policy,
        &signals,
        anchor,
        &usage,
        &live.signals,
        None,
        now,
    )
    .ok_or_else(|| {
        AppError::Conflict(format!(
            "smart_auto_no_eligible_model: no model serves {} under the current rules and filters",
            tier.selector()
        ))
    })?;
    let alternatives = SmartAutoRouter::runners_up(
        &pool,
        tier,
        RoleDecision {
            role: decision.role,
            plan_backed: decision.plan_backed,
            reason: RoleReason::CallerHint,
        },
        std::slice::from_ref(&decision.model.qualified_id),
        &usage,
        &live.signals,
        RESOLVE_ALTERNATIVES,
    );
    Ok(ResolveReply {
        model: decision.model.qualified_id.clone(),
        tier: tier.as_str(),
        role: decision.role.as_str(),
        plan_backed: decision.plan_backed,
        effort: decision.effort.map(EffortLevel::as_str),
        reason: decision.reason.as_str(),
        pinned: matches!(
            decision.reason,
            RouteReason::Pinned | RouteReason::ToolContinuationReplay | RouteReason::UserPinned
        ),
        alternatives,
        why: decision.why,
    })
}

/// `GET /api/smart-auto/tiers`: each tier's selector and eligible pool under the current rules,
/// the role names and the advisory tier per role.
#[utoipa::path(get, path = "/api/smart-auto/tiers", tag = "smart-auto", summary = "Tiers, roles and pools", responses((status = 200, description = "Each tier selector and eligible pool, the roles and the advisory tier per role", body = Object), (status = 401, description = "Missing or invalid hub key.", body = ErrorBody)))]
pub async fn tiers(State(state): State<AppState>) -> Response {
    let config = state.config();
    let rules = state.smart_auto_rules().current();
    let snapshot = state.model_descriptors();
    let mut rows = Vec::with_capacity(SmartAutoTier::ALL.len());
    for tier in SmartAutoTier::ALL {
        let selector = match tier_selector(&config.smart_auto_presets, tier) {
            Ok(selector) => selector,
            Err(error) => return error.into_response(),
        };
        let policy = hub_policy(&rules, &selector, &SmartAutoHeaders::default());
        let pool = SmartAutoPool::eligible(&snapshot, &policy, RESOLVE_ENDPOINT, 0)
            .into_iter()
            .map(|descriptor| descriptor.qualified_id.clone())
            .collect();
        rows.push(TierRow {
            name: tier.as_str(),
            selector: tier_selector_id(tier),
            pool,
        });
    }
    Json(TiersReply {
        default_tier: SmartAutoTier::default().as_str(),
        tiers: rows,
        roles: ROLE_TIER_DEFAULTS
            .iter()
            .map(|(role, _)| role.as_str())
            .collect(),
        role_tier_defaults: ROLE_TIER_DEFAULTS
            .iter()
            .map(|(role, tier)| (role.as_str(), tier.as_str()))
            .collect(),
    })
    .into_response()
}

fn tier_selector(
    presets: &BTreeMap<String, SmartAutoPolicy>,
    tier: SmartAutoTier,
) -> Result<HubSelector, AppError> {
    let id = tier_selector_id(tier);
    HubSelector::parse(&id, presets)?
        .ok_or_else(|| AppError::Internal(format!("{id} is not a hub selector")))
}

/// The `hub/smart-auto-<tier>` id of a tier.
fn tier_selector_id(tier: SmartAutoTier) -> String {
    format!("{}/{}", HubSelector::NAMESPACE, tier.selector())
}

/// Drops an empty query value, so `?worker=` behaves as if the parameter were absent.
fn non_empty(value: Option<String>) -> Option<String> {
    value.filter(|raw| !raw.is_empty())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::configs::HubConfig;
    use crate::schemas::work_role::WorkRole;
    use crate::services::fake_gh_cli::FakeGhCli;
    use crate::services::gh_cli::GhOutcome;
    use crate::services::memory_credential_store::MemoryCredentialStore;
    use crate::services::model_health::FailureKind;
    use crate::services::store::Store;
    use axum::http::StatusCode;
    use serde_json::{Value, json};
    use std::sync::Arc;
    use tempfile::TempDir;

    const ALPHA: &str = "alpha/claude-sonnet-4.5";
    const BETA: &str = "beta/gpt-4.1";

    #[derive(Clone, Copy)]
    enum Persistence {
        Off,
        Sqlite,
        Json,
    }

    struct Hub {
        state: AppState,
        _dir: TempDir,
    }

    fn hub(persistence: Persistence, extra: &[(&str, &str)]) -> Hub {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("stats.db");
        let path = path.to_str().unwrap();
        let store = match persistence {
            Persistence::Off => None,
            Persistence::Sqlite => Some(Store::open("sqlite", Some(path)).unwrap()),
            Persistence::Json => Some(Store::open("json", Some(path)).unwrap()),
        };
        let vars: HashMap<String, String> = [("LLM_HUB_STORE_PATH", path)]
            .iter()
            .chain(extra)
            .map(|(key, value)| ((*key).to_string(), (*value).to_string()))
            .collect();
        let state = AppState::with_seams(
            HubConfig::from_map(&vars).unwrap(),
            store,
            Arc::new(MemoryCredentialStore::default()),
            Arc::new(FakeGhCli::new(GhOutcome::NotInstalled)),
        )
        .unwrap();
        Hub { state, _dir: dir }
    }

    fn with_models(persistence: Persistence, ids: &[&str]) -> Hub {
        let hub = hub(persistence, &[]);
        hub.state.set_model_descriptors(
            ids.iter()
                .map(|id| ModelDescriptor {
                    endpoints: PIN_ENDPOINTS.iter().map(|e| (*e).to_owned()).collect(),
                    ..ModelDescriptor::for_test(id)
                })
                .collect(),
        );
        hub
    }

    async fn json_of(response: Response) -> (StatusCode, Value) {
        let status = response.status();
        let bytes = axum::body::to_bytes(response.into_body(), usize::MAX)
            .await
            .unwrap();
        (status, serde_json::from_slice(&bytes).unwrap())
    }

    fn worker_key(state: &AppState, raw: &str) -> String {
        state.selection_log().scope_key(ScopeKind::Worker, raw)
    }

    fn event(
        worker: Option<&str>,
        stage: SelectionStage,
        selection_state: SelectionState,
        model: &str,
    ) -> SelectionEvent {
        SelectionEvent {
            worker: worker.map(str::to_owned),
            ..SelectionEvent::new(stage, selection_state, model)
        }
    }

    async fn get_current(state: &AppState, worker: Option<&str>, session: Option<&str>) -> Value {
        let query = CurrentQuery {
            worker: worker.map(str::to_owned),
            session: session.map(str::to_owned),
        };
        let (status, body) = json_of(current(State(state.clone()), Query(query)).await).await;
        assert_eq!(status, StatusCode::OK);
        body
    }

    async fn get_history(state: &AppState, params: HistoryParams) -> (StatusCode, Value) {
        json_of(history(State(state.clone()), Query(params)).await).await
    }

    fn models_of(body: &Value) -> Vec<&str> {
        body["events"]
            .as_array()
            .unwrap()
            .iter()
            .map(|event| event["model"].as_str().unwrap())
            .collect()
    }

    async fn post_outcome(state: &AppState, body: Value) -> (StatusCode, Value) {
        let body: OutcomeBody = serde_json::from_value(body).unwrap();
        json_of(report_outcome(State(state.clone()), Json(body)).await).await
    }

    async fn post_resolve(state: &AppState, body: Value) -> (StatusCode, Value) {
        let body: ResolveBody = serde_json::from_value(body).unwrap();
        json_of(resolve(State(state.clone()), Json(body)).await).await
    }

    async fn get_attempt(state: &AppState, attempt: Option<&str>) -> (StatusCode, Value) {
        let params = AttemptParams {
            attempt: attempt.map(str::to_owned),
        };
        json_of(attempt_text(State(state.clone()), Query(params)).await).await
    }

    #[test]
    fn scope_key_prefix_is_8_to_64_lowercase_hex() {
        assert!(is_scope_key_prefix("0123abcd"));
        assert!(is_scope_key_prefix(&"f".repeat(64)));
        assert!(!is_scope_key_prefix("0123abc"));
        assert!(!is_scope_key_prefix(&"f".repeat(65)));
        assert!(!is_scope_key_prefix("0123ABCD"));
        assert!(!is_scope_key_prefix("0123abcg"));
    }

    #[tokio::test]
    async fn current_without_a_worker_or_session_is_a_bad_request() {
        let hub = hub(Persistence::Off, &[]);
        let query = CurrentQuery {
            worker: Some(String::new()),
            session: None,
        };

        let (status, _) = json_of(current(State(hub.state), Query(query)).await).await;

        assert_eq!(status, StatusCode::BAD_REQUEST);
    }

    #[tokio::test]
    async fn current_finds_a_worker_by_raw_session_or_by_key_prefix() {
        let hub = hub(Persistence::Off, &[]);
        let key = worker_key(&hub.state, "session-1");
        hub.state.selection_log().record(event(
            Some(&key),
            SelectionStage::Solver,
            SelectionState::Selected,
            ALPHA,
        ));

        let by_session = get_current(&hub.state, None, Some("session-1")).await;
        let by_prefix = get_current(&hub.state, Some(&key[..12]), None).await;
        let unknown = get_current(&hub.state, Some("someone-else"), None).await;

        assert_eq!(by_session["selection"]["main"]["model"], ALPHA);
        assert_eq!(by_prefix["selection"]["main"]["model"], ALPHA);
        assert!(unknown["selection"].is_null());
    }

    #[tokio::test]
    async fn history_filters_by_worker_and_workflow_and_pages_after_since_seq() {
        let hub = hub(Persistence::Off, &[]);
        let log = hub.state.selection_log();
        let one = worker_key(&hub.state, "w1");
        let two = worker_key(&hub.state, "w2");
        log.record(event(
            Some(&one),
            SelectionStage::Solver,
            SelectionState::Selected,
            "m1",
        ));
        log.record(SelectionEvent {
            workflow: Some(log.scope_key(ScopeKind::Workflow, "flow")),
            ..event(
                Some(&two),
                SelectionStage::Solver,
                SelectionState::Selected,
                "m2",
            )
        });
        log.record(event(
            Some(&one),
            SelectionStage::Solver,
            SelectionState::Completed,
            "m3",
        ));

        let (status, worker) = get_history(
            &hub.state,
            HistoryParams {
                worker: Some("w1".into()),
                since_ts_ms: Some(0),
                ..HistoryParams::default()
            },
        )
        .await;
        let (_, workflow) = get_history(
            &hub.state,
            HistoryParams {
                workflow: Some("flow".into()),
                ..HistoryParams::default()
            },
        )
        .await;
        let (_, paged) = get_history(
            &hub.state,
            HistoryParams {
                since_seq: Some(1),
                limit: Some(1),
                ..HistoryParams::default()
            },
        )
        .await;

        assert_eq!(status, StatusCode::OK);
        assert_eq!(models_of(&worker), ["m1", "m3"]);
        assert!(worker["events"][1].get("outcome").is_none());
        assert_eq!(models_of(&workflow), ["m2"]);
        assert_eq!(models_of(&paged), ["m2"]);
    }

    #[tokio::test]
    async fn history_attaches_stored_outcomes_to_completed_solver_events_only() {
        let hub = hub(Persistence::Sqlite, &[]);
        let completed = SelectionEvent {
            attempt: Some("attempt-1".into()),
            ..event(
                None,
                SelectionStage::Solver,
                SelectionState::Completed,
                "m1",
            )
        };
        let selected = SelectionEvent {
            attempt: Some("attempt-1".into()),
            ..event(None, SelectionStage::Solver, SelectionState::Selected, "m1")
        };
        hub.state.selection_log().record(selected);
        hub.state.selection_log().record(completed);
        let outcome = AttemptOutcome {
            code: OutcomeCode::TestsPassed,
            source: OutcomeSource::Client,
        };
        hub.state
            .store()
            .unwrap()
            .save_attempt_outcome("attempt-1", None, &outcome, now_ms())
            .unwrap();

        let (status, body) = get_history(&hub.state, HistoryParams::default()).await;

        assert_eq!(status, StatusCode::OK);
        assert!(body["events"][0].get("outcome").is_none());
        assert_eq!(
            body["events"][1]["outcome"],
            json!({"code": "tests_passed", "source": "client"})
        );
    }

    #[tokio::test]
    async fn history_since_a_time_reads_the_saved_events_past_the_ring() {
        let hub = hub(Persistence::Sqlite, &[]);
        let now = now_ms();
        let saved = SelectionEvent {
            seq: 7,
            ts_ms: now,
            ..event(
                None,
                SelectionStage::Solver,
                SelectionState::Selected,
                "saved",
            )
        };
        hub.state
            .store()
            .unwrap()
            .save_selection_events(&[saved], now, -1)
            .unwrap();

        let (_, from_ring) = get_history(&hub.state, HistoryParams::default()).await;
        let (status, from_store) = get_history(
            &hub.state,
            HistoryParams {
                since_ts_ms: Some(now - 1_000),
                ..HistoryParams::default()
            },
        )
        .await;

        assert!(models_of(&from_ring).is_empty());
        assert_eq!(status, StatusCode::OK);
        assert_eq!(models_of(&from_store), ["saved"]);
    }

    #[tokio::test]
    async fn an_outcome_naming_no_attempt_session_or_worker_is_a_bad_request() {
        let hub = hub(Persistence::Sqlite, &[]);

        let (status, _) = post_outcome(&hub.state, json!({"code": "tests_passed"})).await;

        assert_eq!(status, StatusCode::BAD_REQUEST);
    }

    #[tokio::test]
    async fn an_outcome_for_a_worker_without_a_completed_attempt_is_not_found() {
        let hub = hub(Persistence::Sqlite, &[]);

        let (status, _) =
            post_outcome(&hub.state, json!({"worker": "abc", "code": "tests_failed"})).await;

        assert_eq!(status, StatusCode::NOT_FOUND);
    }

    #[tokio::test]
    async fn an_outcome_without_persistence_is_a_bad_request() {
        let hub = hub(Persistence::Off, &[]);

        let (status, body) = post_outcome(
            &hub.state,
            json!({"attempt": "attempt-1", "code": "user_accepted"}),
        )
        .await;

        assert_eq!(status, StatusCode::BAD_REQUEST);
        assert!(body.to_string().contains("persistence is off"));
    }

    #[tokio::test]
    async fn a_session_outcome_lands_on_its_last_completed_attempt() {
        let hub = hub(Persistence::Sqlite, &[]);
        let worker = worker_key(&hub.state, "session-1");
        hub.state
            .attempt_tracker()
            .note_completed(&worker, "attempt-9");

        let (status, body) = post_outcome(
            &hub.state,
            json!({"session_id": "session-1", "code": "user_corrected"}),
        )
        .await;

        assert_eq!(status, StatusCode::OK);
        assert_eq!(
            body,
            json!({"attempt": "attempt-9", "outcome": {"code": "user_corrected", "source": "client"}})
        );
        let stored = hub
            .state
            .store()
            .unwrap()
            .load_attempt_outcomes(&["attempt-9".to_owned()])
            .unwrap();
        assert_eq!(stored["attempt-9"].code, OutcomeCode::UserCorrected);
        assert_eq!(stored["attempt-9"].source, OutcomeSource::Client);
    }

    #[tokio::test]
    async fn decisions_are_newest_first_and_the_limit_is_clamped() {
        let hub = hub(Persistence::Off, &[]);
        for ts_ms in 1..=3 {
            hub.state.smart_auto_decisions().record(SmartAutoDecision {
                ts_ms,
                session: None,
                tier: "balance",
                role: WorkRole::Conversation,
                effort: None,
                model: ALPHA.into(),
                reason: "fresh",
                why: SelectionWhy::default(),
            });
        }
        let fetch = |limit| {
            let state = hub.state.clone();
            async move { json_of(decisions(State(state), Query(DecisionsQuery { limit })).await).await }
        };

        let (status, two) = fetch(Some(2)).await;
        let (_, clamped) = fetch(Some(0)).await;
        let (_, default) = fetch(None).await;

        let stamps = |body: &Value| -> Vec<u64> {
            body["decisions"]
                .as_array()
                .unwrap()
                .iter()
                .map(|d| d["ts_ms"].as_u64().unwrap())
                .collect()
        };
        assert_eq!(status, StatusCode::OK);
        assert_eq!(stamps(&two), [3, 2]);
        assert_eq!(stamps(&clamped), [3]);
        assert_eq!(stamps(&default), [3, 2, 1]);
    }

    #[tokio::test]
    async fn health_reports_a_cooled_model() {
        let hub = hub(Persistence::Off, &[]);
        hub.state.model_health().record_failure(
            ALPHA,
            RESOLVE_ENDPOINT,
            FailureKind::Timeout,
            Instant::now(),
        );

        let (status, body) = json_of(health(State(hub.state)).await).await;

        assert_eq!(status, StatusCode::OK);
        assert_eq!(body["models"][0]["model"], ALPHA);
        assert_eq!(body["models"][0]["failures"], 1);
        assert!(body["models"][0]["cooled_for_ms"].as_u64().unwrap() > 0);
        assert!(body["costs"].is_object());
    }

    #[tokio::test]
    async fn saved_rules_are_read_back_and_change_each_models_effect() {
        let hub = with_models(Persistence::Off, &[ALPHA, BETA]);

        let (_, before) = json_of(get_rules(State(hub.state.clone())).await).await;
        let (put_status, saved) = json_of(
            put_rules(
                State(hub.state.clone()),
                Bytes::from_static(br#"{"exclude":["beta/*"]}"#),
            )
            .await,
        )
        .await;
        let (_, after) = json_of(get_rules(State(hub.state.clone())).await).await;
        let (_, effect) = json_of(rules_effect(State(hub.state.clone())).await).await;

        assert_eq!(before["source"], "none");
        assert_eq!(put_status, StatusCode::OK);
        assert_eq!(saved["source"], "file");
        assert_eq!(saved["readonly"], false);
        assert_eq!(after["rules"]["exclude"], json!(["beta/*"]));
        let eligible: BTreeMap<&str, bool> = effect["rows"]
            .as_array()
            .unwrap()
            .iter()
            .map(|row| {
                (
                    row["qualified_id"].as_str().unwrap(),
                    row["eligible"].as_bool().unwrap(),
                )
            })
            .collect();
        assert_eq!(eligible, BTreeMap::from([(ALPHA, true), (BETA, false)]));
    }

    struct FreezeRules;

    impl crate::ext::Extension for FreezeRules {
        fn name(&self) -> &'static str {
            "freeze-rules"
        }

        fn on_config_change(&self, ctx: &crate::ext::ConfigChangeCtx<'_>) -> crate::ext::Flow {
            if ctx.kind() == ConfigKind::SmartAutoRules {
                crate::ext::Flow::reject(423, "Smart Auto rules are frozen")
            } else {
                crate::ext::Flow::Continue
            }
        }
    }

    #[tokio::test]
    async fn an_extension_can_stop_a_smart_auto_rules_save() {
        let hub = with_models(Persistence::Off, &[ALPHA, BETA]);
        let mut all = crate::ext::builtin::builtins();
        all.push(std::sync::Arc::new(FreezeRules));
        hub.state.set_extensions(crate::ext::ExtensionSet::new(all));

        let response = put_rules(
            State(hub.state.clone()),
            Bytes::from_static(br#"{"exclude":["beta/*"]}"#),
        )
        .await;
        let (_, rules) = json_of(get_rules(State(hub.state.clone())).await).await;

        assert_eq!(response.status().as_u16(), 423);
        assert_eq!(rules["rules"]["exclude"], serde_json::json!([]));
    }

    #[tokio::test]
    async fn invalid_rules_are_a_bad_request() {
        let hub = hub(Persistence::Off, &[]);

        let response = put_rules(State(hub.state), Bytes::from_static(b"not json")).await;

        assert_eq!(response.status(), StatusCode::BAD_REQUEST);
    }

    #[tokio::test]
    async fn env_rules_are_read_only() {
        let hub = hub(
            Persistence::Off,
            &[("LLM_HUB_SMART_AUTO_RULES", r#"{"exclude":["*preview*"]}"#)],
        );

        let (_, rules) = json_of(get_rules(State(hub.state.clone())).await).await;
        let response = put_rules(State(hub.state), Bytes::from_static(b"{}")).await;

        assert_eq!(rules["source"], "env");
        assert_eq!(rules["readonly"], true);
        assert!(response.status().is_client_error());
    }

    #[tokio::test]
    async fn attempt_text_needs_an_attempt() {
        let hub = hub(Persistence::Sqlite, &[]);

        let (status, _) = get_attempt(&hub.state, Some("")).await;

        assert_eq!(status, StatusCode::BAD_REQUEST);
    }

    #[tokio::test]
    async fn attempt_text_without_persistence_is_null() {
        let hub = hub(Persistence::Off, &[]);

        let (status, body) = get_attempt(&hub.state, Some("attempt-1")).await;

        assert_eq!(status, StatusCode::OK);
        assert_eq!(
            body,
            json!({"attempt": "attempt-1", "prompt": null, "answer": null})
        );
    }

    #[tokio::test]
    async fn attempt_text_returns_the_saved_prompt_and_answer() {
        let hub = hub(Persistence::Sqlite, &[]);
        let store = hub.state.store().unwrap();
        store
            .save_selection_prompt("attempt-1", now_ms(), "the prompt")
            .unwrap();
        store
            .save_selection_answer("attempt-1", now_ms(), "the answer")
            .unwrap();

        let (_, saved) = get_attempt(&hub.state, Some("attempt-1")).await;
        let (status, unknown) = get_attempt(&hub.state, Some("attempt-2")).await;

        assert_eq!(saved["prompt"], "the prompt");
        assert_eq!(saved["answer"], "the answer");
        assert_eq!(status, StatusCode::OK);
        assert!(unknown["prompt"].is_null() && unknown["answer"].is_null());
    }

    #[tokio::test]
    async fn attempt_text_on_the_json_store_is_an_internal_error() {
        let hub = hub(Persistence::Json, &[]);

        let (status, body) = get_attempt(&hub.state, Some("attempt-1")).await;

        assert_eq!(status, StatusCode::INTERNAL_SERVER_ERROR);
        assert!(body.to_string().contains("selection text cannot be read"));
    }

    #[tokio::test]
    async fn resolve_refuses_an_unknown_tier_or_role() {
        let hub = with_models(Persistence::Off, &[ALPHA]);

        let (tier, _) = post_resolve(&hub.state, json!({"tier": "turbo"})).await;
        let (role, _) = post_resolve(&hub.state, json!({"role": "juggling"})).await;

        assert_eq!(tier, StatusCode::BAD_REQUEST);
        assert_eq!(role, StatusCode::BAD_REQUEST);
    }

    #[tokio::test]
    async fn resolve_without_an_eligible_model_is_a_conflict() {
        let hub = with_models(Persistence::Off, &[ALPHA]);

        let (empty, _) = post_resolve(&hub.state, json!({"exclude": ["alpha/*"]})).await;
        let (filtered, body) = post_resolve(&hub.state, json!({"profiles": "beta"})).await;

        assert_eq!(empty, StatusCode::CONFLICT);
        assert_eq!(filtered, StatusCode::CONFLICT);
        assert!(body.to_string().contains("smart-auto-balance"));
    }

    #[tokio::test]
    async fn resolve_picks_a_model_and_lists_the_others_as_alternatives() {
        let hub = with_models(Persistence::Off, &[ALPHA, BETA]);

        let (status, body) = post_resolve(
            &hub.state,
            json!({"tier": "efficiency", "role": "validation", "text": "run the tests", "include": ["", "*"]}),
        )
        .await;

        assert_eq!(status, StatusCode::OK);
        assert_eq!(body["tier"], "efficiency");
        assert_eq!(body["role"], "validation");
        assert_eq!(body["pinned"], false);
        let model = body["model"].as_str().unwrap();
        let other = if model == ALPHA { BETA } else { ALPHA };
        assert_eq!(body["alternatives"], json!([other]));
    }

    #[tokio::test]
    async fn resolve_keeps_to_the_profiles_filter_and_skips_a_cooled_model() {
        let hub = with_models(Persistence::Off, &[ALPHA, BETA]);

        let (_, only_beta) = post_resolve(&hub.state, json!({"profiles": "beta"})).await;
        hub.state.model_health().record_failure(
            BETA,
            RESOLVE_ENDPOINT,
            FailureKind::Timeout,
            Instant::now(),
        );
        let (_, cooled_beta) = post_resolve(&hub.state, json!({})).await;
        let (_, only_cooled) = post_resolve(&hub.state, json!({"profiles": "beta"})).await;

        assert_eq!(only_beta["model"], BETA);
        assert_eq!(cooled_beta["model"], ALPHA);
        assert_eq!(only_cooled["model"], BETA);
    }

    #[tokio::test]
    async fn resolve_reports_a_pin_kept_on_another_client_route() {
        let hub = with_models(Persistence::Off, &[ALPHA, BETA]);
        let mut body = ResolveBody {
            worker: Some("worker-1".into()),
            ..ResolveBody::default()
        };
        let caller = resolve_caller(&mut body).unwrap();
        let selector = tier_selector(
            &hub.state.config().smart_auto_presets,
            SmartAutoTier::default(),
        )
        .unwrap();
        let policy = hub_policy(&hub.state.smart_auto_rules().current(), &selector, &caller);
        let signals = TurnSignals {
            estimated_prompt_tokens: 0,
            tool_continuation: false,
            user_turns: 0,
            last_user_text: None,
            has_tools: false,
            has_image: false,
            session_id: None,
            prompt_cache_key: None,
            content_key: None,
        };
        let key = AffinityKey::new(&signals, &caller, &policy, "/v1/chat/completions").unwrap();
        let (_, fresh) = post_resolve(&hub.state, json!({"worker": "worker-1"})).await;
        let fresh_model = fresh["model"].as_str().unwrap();
        let pinned_model = if fresh_model == ALPHA { BETA } else { ALPHA };
        let role = role_from_name(fresh["role"].as_str().unwrap()).unwrap();
        assert!(hub.state.smart_auto_affinity().commit(
            &key,
            pinned_model,
            role,
            false,
            0,
            Instant::now(),
        ));

        let (status, pinned) = post_resolve(&hub.state, json!({"worker": "worker-1"})).await;

        assert_eq!(status, StatusCode::OK);
        assert_eq!(pinned["model"], pinned_model);
        assert_eq!(pinned["reason"], "pinned");
        assert_eq!(pinned["pinned"], true);
    }

    #[tokio::test]
    async fn tiers_list_every_tier_pool_and_the_role_defaults() {
        let hub = with_models(Persistence::Off, &[ALPHA, BETA]);

        let (status, body) = json_of(tiers(State(hub.state)).await).await;

        assert_eq!(status, StatusCode::OK);
        assert_eq!(body["default_tier"], "balance");
        let names: Vec<&str> = body["tiers"]
            .as_array()
            .unwrap()
            .iter()
            .map(|row| row["name"].as_str().unwrap())
            .collect();
        assert_eq!(names, ["efficiency", "balance", "intelligence", "fast"]);
        assert_eq!(body["tiers"][1]["selector"], "hub/smart-auto-balance");
        assert_eq!(body["tiers"][1]["pool"], json!([ALPHA, BETA]));
        assert_eq!(body["roles"].as_array().unwrap().len(), 5);
        assert_eq!(body["role_tier_defaults"]["planning"], "intelligence");
    }
}
