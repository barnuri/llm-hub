use axum::Json;
use axum::body::Bytes;
use axum::extract::{Query, State};
use axum::response::{IntoResponse, Response};
use std::collections::HashMap;

use serde::{Deserialize, Serialize};

use crate::consts::{
    AUTO_DECISIONS_DEFAULT_LIMIT, AUTO_DECISIONS_MAX_LIMIT, SELECTION_HISTORY_DEFAULT_LIMIT,
    SELECTION_HISTORY_MAX_LIMIT,
};
use crate::dependencies::state::AppState;
use crate::routes::proxy::hub_policy;
use crate::schemas::affinity_key::AffinityKey;
use crate::schemas::app_error::AppError;
use crate::schemas::attempt_outcome::{AttemptOutcome, OutcomeCode, OutcomeSource};
use crate::schemas::current_selection::CurrentSelection;
use crate::schemas::effort_level::EffortLevel;
use crate::schemas::history_query::HistoryQuery;
use crate::schemas::hub_selector::HubSelector;
use crate::schemas::model_context::strip_1m_suffix;
use crate::schemas::model_descriptor::ModelDescriptor;
use crate::schemas::role_decision::RoleDecision;
use crate::schemas::role_reason::RoleReason;
use crate::schemas::route_reason::RouteReason;
use crate::schemas::scope_kind::ScopeKind;
use crate::schemas::selection_event::SelectionEvent;
use crate::schemas::selection_stage::SelectionStage;
use crate::schemas::selection_state::SelectionState;
use crate::schemas::smart_auto_decision::SmartAutoDecision;
use crate::schemas::smart_auto_headers::SmartAutoHeaders;
use crate::schemas::smart_auto_policy::SmartAutoPolicy;
use crate::schemas::smart_auto_role_tiers::{ROLE_TIER_DEFAULTS, role_from_name};
use crate::schemas::smart_auto_rules::SmartAutoRules;
use crate::schemas::smart_auto_tier::SmartAutoTier;
use crate::schemas::turn_signals::TurnSignals;
use crate::services::model_health::HealthRow;
use crate::services::selection_log::SelectionLog;
use crate::services::smart_auto_live::SmartAutoLive;
use crate::services::smart_auto_pool::SmartAutoPool;
use crate::services::smart_auto_router::SmartAutoRouter;
use crate::services::smart_auto_rules::{LoadedRules, RuleEffect, RulesSource, rule_effects};
use crate::services::store::now_ms;
use std::collections::{BTreeMap, BTreeSet};
use std::time::Instant;

#[derive(Debug, Deserialize, Default)]
pub struct CurrentQuery {
    pub worker: Option<String>,
    pub session: Option<String>,
}

#[derive(Debug, Deserialize, Default)]
pub struct HistoryParams {
    pub workflow: Option<String>,
    pub worker: Option<String>,
    pub since_seq: Option<u64>,
    /// Read the saved history from this time on, instead of the in-memory ring.
    pub since_ts_ms: Option<u64>,
    pub limit: Option<usize>,
}

#[derive(Debug, Deserialize, Default)]
pub struct DecisionsQuery {
    pub limit: Option<usize>,
}

#[derive(Debug, Deserialize, Default)]
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

#[derive(Debug, Deserialize)]
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
pub async fn health(State(state): State<AppState>) -> Response {
    Json(HealthBody {
        models: state.model_health().report(std::time::Instant::now()),
        costs: state
            .model_costs()
            .iter()
            .map(|(id, score)| (id.clone(), *score))
            .collect(),
    })
    .into_response()
}

/// `GET /api/smart-auto/rules`: the active allow, exclude and include rules, their source and whether they are read-only.
pub async fn get_rules(State(state): State<AppState>) -> Response {
    rules_response(&state.smart_auto_rules().current())
}

/// `PUT /api/smart-auto/rules`: validates the body, saves it atomically and returns the saved rules.
pub async fn put_rules(State(state): State<AppState>, body: Bytes) -> Response {
    match state.smart_auto_rules().replace(&body) {
        Ok(loaded) => rules_response(&loaded),
        Err(error) => error.into_response(),
    }
}

/// `GET /api/smart-auto/rules/effect`: each model's `eligible`, `reason` and `matched` glob under the current rules.
/// Reasons are checked in this order: `not_chat`, `auto_alias`, `hub_namespace`, `allowed_override`, `excluded_by_global`, `excluded_by_profile`, `not_in_profile_include`, `not_in_global_include`, `eligible`.
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
#[derive(Debug, Deserialize, Default)]
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

/// The client route a dry run resolves for; affinity keys include it.
const RESOLVE_ENDPOINT: &str = "/v1/messages";
/// How many other eligible models a dry run lists.
const RESOLVE_ALTERNATIVES: usize = 4;

/// `POST /api/smart-auto/resolve`: the model a `hub/smart-auto-<tier>` turn would get, without
/// calling an upstream or recording anything. With `worker` or `session`, a kept pin is reported.
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
    let snapshot = state.model_descriptors();
    let mut pool = SmartAutoPool::eligible(&snapshot, &policy, RESOLVE_ENDPOINT, 0);
    let now = Instant::now();
    let live = SmartAutoLive::collect(state, RESOLVE_ENDPOINT, &pool, now);
    let sits_out = |descriptor: &&ModelDescriptor| {
        live.unavailable
            .contains(strip_1m_suffix(&descriptor.qualified_id))
    };
    if pool.iter().any(|descriptor| !sits_out(descriptor)) {
        pool.retain(|descriptor| !sits_out(descriptor));
    }
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
    let key = AffinityKey::new(&signals, &caller, &policy, RESOLVE_ENDPOINT);
    let anchor = key.as_ref().map(|key| (state.smart_auto_affinity(), key));
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
            RouteReason::Pinned | RouteReason::ToolContinuationReplay
        ),
        alternatives,
    })
}

/// `GET /api/smart-auto/tiers`: each tier's selector and eligible pool under the current rules,
/// the role names and the advisory tier per role.
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
            selector: format!("{}/{}", HubSelector::NAMESPACE, tier.selector()),
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
    let id = format!("{}/{}", HubSelector::NAMESPACE, tier.selector());
    HubSelector::parse(&id, presets)?
        .ok_or_else(|| AppError::Internal(format!("{id} is not a hub selector")))
}

/// Drops an empty query value, so `?worker=` behaves as if the parameter were absent.
fn non_empty(value: Option<String>) -> Option<String> {
    value.filter(|raw| !raw.is_empty())
}

#[cfg(test)]
mod tests {
    use super::is_scope_key_prefix;

    #[test]
    fn scope_key_prefix_is_8_to_64_lowercase_hex() {
        assert!(is_scope_key_prefix("0123abcd"));
        assert!(is_scope_key_prefix(&"f".repeat(64)));
        assert!(!is_scope_key_prefix("0123abc"));
        assert!(!is_scope_key_prefix(&"f".repeat(65)));
        assert!(!is_scope_key_prefix("0123ABCD"));
        assert!(!is_scope_key_prefix("0123abcg"));
    }
}
