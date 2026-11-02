use axum::Json;
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

/// `GET /api/smart-auto/current?worker=<id>` or `?session=<id>`: the worker's main model, its active stages and freshness.
pub async fn current(State(state): State<AppState>, Query(query): Query<CurrentQuery>) -> Response {
    let Some(raw) = non_empty(query.worker).or_else(|| non_empty(query.session)) else {
        return AppError::BadRequest("worker or session is required".into()).into_response();
    };
    let log = state.selection_log();
    let selection = log.current(&log.scope_key(ScopeKind::Worker, &raw), now_ms());
    Json(CurrentBody { selection }).into_response()
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

/// Drops an empty query value, so `?worker=` behaves as if the parameter were absent.
fn non_empty(value: Option<String>) -> Option<String> {
    value.filter(|raw| !raw.is_empty())
}
