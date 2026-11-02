use axum::Json;
use axum::body::Bytes;
use axum::extract::{Query, State};
use axum::response::{IntoResponse, Response};
use serde::{Deserialize, Serialize};

use crate::consts::{
    AUTO_DECISIONS_DEFAULT_LIMIT, AUTO_DECISIONS_MAX_LIMIT, SELECTION_HISTORY_DEFAULT_LIMIT,
    SELECTION_HISTORY_MAX_LIMIT,
};
use crate::dependencies::state::AppState;
use crate::schemas::app_error::AppError;
use crate::schemas::current_selection::CurrentSelection;
use crate::schemas::history_query::HistoryQuery;
use crate::schemas::scope_kind::ScopeKind;
use crate::schemas::selection_event::SelectionEvent;
use crate::schemas::smart_auto_decision::SmartAutoDecision;
use crate::schemas::smart_auto_rules::SmartAutoRules;
use crate::services::smart_auto_rules::{LoadedRules, RuleEffect, RulesSource, rule_effects};
use crate::services::store::now_ms;

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
    pub limit: Option<usize>,
}

#[derive(Debug, Deserialize, Default)]
pub struct DecisionsQuery {
    pub limit: Option<usize>,
}

#[derive(Serialize)]
struct CurrentBody {
    /// `null` until the worker has a selection.
    selection: Option<CurrentSelection>,
}

#[derive(Serialize)]
struct HistoryBody {
    events: Vec<SelectionEvent>,
}

#[derive(Serialize)]
struct DecisionsBody {
    decisions: Vec<SmartAutoDecision>,
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
    Json(HistoryBody {
        events: log.history(&query),
    })
    .into_response()
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
