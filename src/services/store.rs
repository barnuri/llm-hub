use std::collections::{BTreeMap, HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::time::{SystemTime, UNIX_EPOCH};

use crate::schemas::api_key_record::ApiKeyRecord;
use crate::schemas::attempt_outcome::{AttemptOutcome, OutcomeCode, OutcomeSource};
use crate::schemas::cost_parts::CostParts;
use crate::schemas::errors_report::ErrorEntry;
use crate::schemas::prompt_shape::PromptShape;
use crate::schemas::selection_event::SelectionEvent;
use crate::schemas::usage_report::UsageReport;
use crate::schemas::usage_row::UsageRow;
use rusqlite::{Connection, OptionalExtension};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use sha2::{Digest, Sha256};

use crate::consts::{ERROR_STATUS_MIN, KEEP_WARM_CLIENT};
use crate::services::harness::is_keep_warm;
use crate::services::model_limits::{LimitSource, ModelLimit};
use crate::services::pricing::{PricingBook, apply_outcome_costs, outcome_cost_parts};
use crate::services::stats::{RequestOutcome, StatsSnapshot, snapshot_from_outcomes};

const DEFAULT_SQLITE_PATH: &str = "llm-hub.db";
const DEFAULT_JSON_PATH: &str = "llm-hub-stats.json";
const USAGE_RECENT_LIMIT: usize = 200;
/// Selection events kept by count; the oldest are dropped first.
/// Event count kept besides the age limit. A negative value means no count limit, so only the age limit applies.
pub const SELECTION_RETENTION_EVENTS: i64 = -1;
/// Selection events older than this are dropped on the next save.
const SELECTION_RETENTION_MS: u64 = 60 * MS_PER_DAY;
const MS_PER_DAY: u64 = 86_400_000;
/// The `install_settings` name that holds the salt of the scope keys.
const INSTALL_SALT_NAME: &str = "selection_salt";

/// The directory that holds the store file. Files kept beside the store, such as the Smart Auto rules, live here.
#[must_use]
pub fn store_dir(store_path: Option<&str>) -> PathBuf {
    match Path::new(store_path.unwrap_or(DEFAULT_SQLITE_PATH)).parent() {
        Some(dir) if !dir.as_os_str().is_empty() => dir.to_path_buf(),
        _ => PathBuf::from("."),
    }
}

/// Durable store behind `LLM_HUB_PERSISTENT=true`. Sqlite keeps a full request
/// log; Json keeps a rolling recent window flushed on every write batch.
#[derive(Debug)]
pub enum Store {
    Sqlite(Mutex<Connection>),
    Json(Box<Mutex<JsonStore>>),
}

#[derive(Debug)]
pub struct JsonStore {
    path: PathBuf,
    state: JsonState,
}

#[derive(Debug, Serialize, Deserialize, Default)]
struct JsonState {
    totals: Totals,
    recent: Vec<UsageRow>,
    api_keys: Vec<ApiKeyRecord>,
    /// Older files have no catalogs; without the default the whole file would fail to parse.
    #[serde(default)]
    model_catalogs: HashMap<String, Vec<Value>>,
    /// Profile -> upstream model id -> prompt limit. Absent from older files.
    #[serde(default)]
    model_limits: HashMap<String, HashMap<String, ModelLimit>>,
    /// Profile -> stored body fields its upstream rejects. Absent from older files.
    #[serde(default)]
    unsupported_body_fields: HashMap<String, Vec<String>>,
    /// Profile -> upstream models that refused image input. Absent from older files.
    #[serde(default)]
    vision_refusals: HashMap<String, Vec<String>>,
    /// Smart Auto selection events, oldest first. Absent from older files.
    #[serde(default)]
    selection_events: Vec<SelectionEvent>,
    /// The salt behind the selection scope keys. Absent from older files.
    #[serde(default)]
    install_salt: Option<String>,
}

#[derive(Debug, Serialize, Deserialize, Default, Clone)]
struct Totals {
    requests: u64,
    errors: u64,
    tokens_in: u64,
    tokens_out: u64,
    cache_read_tokens: u64,
    cache_write_tokens: u64,
}

/// Optional filters for store-backed stats aggregation.
#[derive(Debug, Clone, Default)]
pub struct StatsFilter {
    pub since_ms: Option<u64>,
    pub profile: Option<String>,
    pub model: Option<String>,
    /// Keeps requests whose `routed_by` starts with this text, for example `smart-auto`.
    pub routed_by_prefix: Option<String>,
    pub range_label: String,
}

/// One `(tier, role)` group of Smart Auto requests: its cost and the verified outcomes of its attempts.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct TierRoleRow {
    pub tier: String,
    pub role: String,
    pub requests: u64,
    pub cost_usd: f64,
    pub completed_attempts: u64,
    /// `None` when the group has no completed attempt.
    pub cost_per_completed_usd: Option<f64>,
    /// Verified outcome code to the number of the group's attempts that carry it.
    pub outcomes: BTreeMap<String, u64>,
}

#[derive(Default)]
struct TierRoleAcc {
    requests: u64,
    cost: CostParts,
    attempts: HashSet<String>,
}

/// The tier of a `smart-auto:<tier>` route, or `None` for any other route.
fn smart_auto_tier(routed_by: &str) -> Option<&str> {
    routed_by
        .strip_prefix("smart-auto:")
        .filter(|tier| !tier.is_empty())
}

/// Groups Smart Auto request rows by tier and role. A missing role groups as `unknown`.
/// `completed` holds the attempts with a completed solver event; `verdicts` maps an attempt to its outcome code.
pub(crate) fn group_tier_role(
    requests: &[RequestOutcome],
    cost_of: impl Fn(&RequestOutcome) -> CostParts,
    completed: &HashSet<String>,
    verdicts: &HashMap<String, String>,
) -> Vec<TierRoleRow> {
    let mut groups: BTreeMap<(&str, &str), TierRoleAcc> = BTreeMap::new();
    for request in requests {
        let Some(tier) = request.routed_by.as_deref().and_then(smart_auto_tier) else {
            continue;
        };
        let role = request.role.as_deref().unwrap_or("unknown");
        let group = groups.entry((tier, role)).or_default();
        group.requests += 1;
        group.cost += cost_of(request);
        if let Some(attempt) = &request.attempt {
            group.attempts.insert(attempt.clone());
        }
    }
    groups
        .into_iter()
        .map(|((tier, role), group)| {
            let completed_attempts = group
                .attempts
                .iter()
                .filter(|attempt| completed.contains(*attempt))
                .count();
            let completed_attempts = u64::try_from(completed_attempts).unwrap_or(u64::MAX);
            let mut outcomes = BTreeMap::new();
            for attempt in &group.attempts {
                if let Some(code) = verdicts.get(attempt) {
                    *outcomes.entry(code.clone()).or_insert(0) += 1;
                }
            }
            let cost_usd = group.cost.total();
            #[allow(clippy::cast_precision_loss)] // attempt counts stay far below 2^52
            let completed_count = completed_attempts as f64;
            TierRoleRow {
                tier: tier.into(),
                role: role.into(),
                requests: group.requests,
                cost_usd,
                completed_attempts,
                cost_per_completed_usd: (completed_attempts > 0)
                    .then(|| cost_usd / completed_count),
                outcomes,
            }
        })
        .collect()
}

impl Store {
    /// SQL condition that keeps user requests and drops keep-warm pings (NULL-safe).
    fn not_keep_warm_sql() -> String {
        format!("client IS NOT '{KEEP_WARM_CLIENT}'")
    }

    /// Runs `ddl` unless `existing` already has the column `name`.
    fn add_missing_column(
        conn: &Connection,
        existing: &[String],
        name: &str,
        ddl: &str,
    ) -> Result<(), String> {
        if existing.iter().any(|col| col == name) {
            return Ok(());
        }
        conn.execute(ddl, [])
            .map_err(|e| format!("sqlite migrate add {name}: {e}"))?;
        Ok(())
    }

    /// The JSON text of a count map. Serializing a map of strings to counts cannot fail; the fallback is an empty object.
    fn json_text<T: Serialize>(value: &T) -> String {
        serde_json::to_string(value).unwrap_or_else(|_| "{}".to_owned())
    }

    /// kind: "sqlite" | "json". Unknown kinds error at startup.
    pub fn open(kind: &str, path: Option<&str>) -> Result<Store, String> {
        match kind {
            "sqlite" => Self::open_sqlite(path.unwrap_or(DEFAULT_SQLITE_PATH)),
            "json" => Ok(Self::open_json(path.unwrap_or(DEFAULT_JSON_PATH))),
            other => Err(format!(
                "unknown LLM_HUB_STORE: {other} (expected sqlite or json)"
            )),
        }
    }

    #[allow(clippy::too_many_lines)] // linear schema setup and migrations
    fn open_sqlite(path: &str) -> Result<Store, String> {
        let conn =
            Connection::open(path).map_err(|e| format!("cannot open sqlite at {path}: {e}"))?;
        conn.execute_batch(
            "CREATE TABLE IF NOT EXISTS requests (
                id INTEGER PRIMARY KEY AUTOINCREMENT,
                ts_ms INTEGER NOT NULL,
                model TEXT NOT NULL,
                profile TEXT NOT NULL,
                status INTEGER NOT NULL,
                latency_ms INTEGER NOT NULL,
                tokens_in INTEGER NOT NULL,
                tokens_out INTEGER NOT NULL,
                ttft_ms INTEGER,
                cache_read_tokens INTEGER NOT NULL DEFAULT 0,
                cache_write_tokens INTEGER NOT NULL DEFAULT 0,
                worker TEXT
            );
            CREATE INDEX IF NOT EXISTS idx_requests_ts ON requests(ts_ms);
            CREATE TABLE IF NOT EXISTS api_keys (
                name TEXT PRIMARY KEY,
                key_hash TEXT NOT NULL,
                masked TEXT NOT NULL,
                enabled INTEGER NOT NULL DEFAULT 1,
                created_ms INTEGER NOT NULL
            );
            CREATE TABLE IF NOT EXISTS model_catalogs (
                profile TEXT PRIMARY KEY,
                entries TEXT NOT NULL,
                updated_ms INTEGER NOT NULL
            );
            CREATE TABLE IF NOT EXISTS model_limits (
                profile TEXT NOT NULL,
                model TEXT NOT NULL,
                prompt_limit INTEGER NOT NULL,
                source TEXT NOT NULL,
                learned_at INTEGER NOT NULL,
                PRIMARY KEY (profile, model),
                CONSTRAINT ck_model_limits_source CHECK (source IN ('seeded', 'learned'))
            );
            CREATE TABLE IF NOT EXISTS unsupported_body_fields (
                profile TEXT NOT NULL,
                field TEXT NOT NULL,
                learned_at INTEGER NOT NULL,
                PRIMARY KEY (profile, field)
            );
            CREATE TABLE IF NOT EXISTS vision_refusals (
                profile TEXT NOT NULL,
                model TEXT NOT NULL,
                source TEXT NOT NULL,
                learned_at INTEGER NOT NULL,
                PRIMARY KEY (profile, model),
                CONSTRAINT ck_vision_refusals_source CHECK (source IN ('learned'))
            );
            CREATE TABLE IF NOT EXISTS selection_events (
                seq INTEGER PRIMARY KEY,
                ts_ms INTEGER NOT NULL,
                event TEXT NOT NULL
            );
            CREATE INDEX IF NOT EXISTS idx_selection_events_ts ON selection_events(ts_ms);
            CREATE TABLE IF NOT EXISTS selection_texts (
                attempt TEXT PRIMARY KEY,
                ts_ms INTEGER NOT NULL,
                prompt TEXT NOT NULL DEFAULT '',
                answer TEXT NOT NULL DEFAULT ''
            );
            CREATE INDEX IF NOT EXISTS idx_selection_texts_ts ON selection_texts(ts_ms);
            CREATE TABLE IF NOT EXISTS attempt_outcomes (
                attempt TEXT PRIMARY KEY,
                worker TEXT,
                code TEXT NOT NULL,
                source TEXT NOT NULL,
                ts_ms INTEGER NOT NULL,
                CONSTRAINT ck_attempt_outcomes_code CHECK (code IN ('tests_passed', 'tests_failed', 'review_passed', 'review_rejected', 'user_accepted', 'user_corrected')),
                CONSTRAINT ck_attempt_outcomes_source CHECK (source IN ('heuristic', 'review', 'client'))
            );
            CREATE INDEX IF NOT EXISTS idx_attempt_outcomes_ts ON attempt_outcomes(ts_ms);
            CREATE TABLE IF NOT EXISTS install_settings (
                name TEXT PRIMARY KEY,
                value TEXT NOT NULL
            );
            CREATE TABLE IF NOT EXISTS request_shapes (
                request_id INTEGER PRIMARY KEY,
                system_chars INTEGER NOT NULL,
                messages INTEGER NOT NULL,
                turn_start INTEGER NOT NULL,
                tool_count INTEGER NOT NULL,
                tool_bytes INTEGER NOT NULL,
                mcp_tool_count INTEGER NOT NULL,
                mcp_tool_bytes INTEGER NOT NULL,
                deferred_listing_chars INTEGER,
                skills_listing_chars INTEGER,
                claude_md_chars INTEGER,
                image_count INTEGER NOT NULL,
                project TEXT
            );
            CREATE TABLE IF NOT EXISTS quota_snapshots (
                ts_ms INTEGER NOT NULL,
                profile TEXT NOT NULL,
                quota TEXT NOT NULL,
                used REAL NOT NULL,
                entitlement REAL NOT NULL,
                used_usd REAL,
                limit_usd REAL,
                PRIMARY KEY (ts_ms, profile, quota)
            );",
        )
        .map_err(|e| format!("sqlite schema init failed: {e}"))?;
        migrate_requests_columns(&conn)?;
        migrate_request_shapes_columns(&conn)?;
        Ok(Store::Sqlite(Mutex::new(conn)))
    }

    fn open_json(path: &str) -> Store {
        let path_buf = PathBuf::from(path);
        let state = match std::fs::read(&path_buf) {
            Ok(bytes) => serde_json::from_slice(&bytes).unwrap_or_default(),
            Err(_) => JsonState::default(),
        };
        Store::Json(Box::new(Mutex::new(JsonStore {
            path: path_buf,
            state,
        })))
    }

    /// Writes the request row and returns its id. The json store keeps no row ids and returns `None`.
    pub fn record(&self, outcome: &RequestOutcome) -> Result<Option<i64>, String> {
        let row = UsageRow {
            ts_ms: now_ms(),
            model: outcome.model_key.clone(),
            profile: outcome.profile.clone(),
            status: outcome.status,
            latency_ms: outcome.latency_ms,
            ttft_ms: outcome.ttft_ms,
            tokens_in: outcome.tokens_in,
            tokens_out: outcome.tokens_out,
            cache_read_tokens: outcome.cache_read_tokens,
            cache_write_tokens: outcome.cache_write_tokens,
            cost_usd: 0.0,
            error: outcome.error.clone(),
            client: outcome.client.clone(),
            worker: outcome.worker.clone(),
        };
        match self {
            Store::Sqlite(conn) => {
                let guard = conn.lock().map_err(|_| "sqlite lock poisoned")?;
                guard
                    .execute(
                        "INSERT INTO requests (
                            ts_ms, model, profile, status, latency_ms, tokens_in, tokens_out,
                            ttft_ms, cache_read_tokens, cache_write_tokens, error, client, worker,
                            routed_by, attempt, role
                         ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15, ?16)",
                        rusqlite::params![
                            db_i64(row.ts_ms),
                            row.model,
                            row.profile,
                            i64::from(row.status),
                            db_i64(row.latency_ms),
                            db_i64(row.tokens_in),
                            db_i64(row.tokens_out),
                            row.ttft_ms.map(db_i64),
                            db_i64(row.cache_read_tokens),
                            db_i64(row.cache_write_tokens),
                            row.error,
                            row.client,
                            row.worker,
                            outcome.routed_by.as_deref(),
                            outcome.attempt.as_deref(),
                            outcome.role.as_deref()
                        ],
                    )
                    .map_err(|e| e.to_string())?;
                Ok(Some(guard.last_insert_rowid()))
            }
            Store::Json(store) => {
                let mut guard = store.lock().map_err(|_| "json store lock poisoned")?;
                if !is_keep_warm_row(&row) {
                    guard.state.totals.requests += 1;
                    if row.status >= ERROR_STATUS_MIN {
                        guard.state.totals.errors += 1;
                    }
                }
                guard.state.totals.tokens_in += row.tokens_in;
                guard.state.totals.tokens_out += row.tokens_out;
                guard.state.totals.cache_read_tokens += row.cache_read_tokens;
                guard.state.totals.cache_write_tokens += row.cache_write_tokens;
                guard.state.recent.push(row);
                let overflow = guard.state.recent.len().saturating_sub(USAGE_RECENT_LIMIT);
                if overflow > 0 {
                    guard.state.recent.drain(..overflow);
                }
                guard.flush().map(|()| None)
            }
        }
    }

    /// Writes the prompt shape of the request row `request_id`. Only the sqlite store keeps shapes;
    /// the json store skips them.
    pub fn record_shape(&self, request_id: i64, shape: &PromptShape) -> Result<(), String> {
        let Store::Sqlite(conn) = self else {
            return Ok(());
        };
        let guard = conn.lock().map_err(|_| "sqlite lock poisoned")?;
        guard
            .execute(
                "INSERT INTO request_shapes (
                    request_id, system_chars, messages, turn_start, tool_count, tool_bytes,
                    mcp_tool_count, mcp_tool_bytes, deferred_listing_chars, skills_listing_chars,
                    claude_md_chars, image_count, project, stream_key, tool_result_bytes,
                    skill_body_chars
                 ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15, ?16)",
                rusqlite::params![
                    request_id,
                    db_i64(shape.system_chars),
                    db_i64(shape.messages),
                    i64::from(shape.turn_start),
                    db_i64(shape.tool_count),
                    db_i64(shape.tool_bytes),
                    db_i64(shape.mcp_tool_count),
                    db_i64(shape.mcp_tool_bytes),
                    shape.deferred_listing_chars.map(db_i64),
                    shape.skills_listing_chars.map(db_i64),
                    shape.claude_md_chars.map(db_i64),
                    db_i64(shape.image_count),
                    shape.project.as_deref(),
                    shape.stream_key.as_deref(),
                    Self::json_text(&shape.tool_result_bytes),
                    Self::json_text(&shape.skill_body_chars)
                ],
            )
            .map_err(|e| e.to_string())?;
        Ok(())
    }

    pub fn usage(&self) -> Result<UsageReport, String> {
        match self {
            Store::Sqlite(conn) => {
                let guard = conn.lock().map_err(|_| "sqlite lock poisoned")?;
                let (total_requests, total_errors, total_tokens_in, total_tokens_out) = guard
                    .query_row(
                        &format!(
                            "SELECT COALESCE(SUM(CASE WHEN {NOT_KEEP_WARM} THEN 1 ELSE 0 END), 0),
                                    COALESCE(SUM(CASE WHEN {NOT_KEEP_WARM} AND status >= 400 THEN 1 ELSE 0 END), 0),
                                    COALESCE(SUM(tokens_in), 0), COALESCE(SUM(tokens_out), 0)
                             FROM requests",
                            NOT_KEEP_WARM = Self::not_keep_warm_sql()
                        ),
                        [],
                        |r| {
                            Ok((
                                db_u64(r.get::<_, i64>(0)?),
                                db_u64(r.get::<_, i64>(1)?),
                                db_u64(r.get::<_, i64>(2)?),
                                db_u64(r.get::<_, i64>(3)?),
                            ))
                        },
                    )
                    .map_err(|e| e.to_string())?;
                let mut stmt = guard
                    .prepare(&format!(
                        "SELECT ts_ms, model, profile, status, latency_ms, tokens_in, tokens_out,
                                ttft_ms, cache_read_tokens, cache_write_tokens, error, client, worker
                         FROM requests WHERE {} ORDER BY id DESC LIMIT ?1",
                        Self::not_keep_warm_sql()
                    ))
                    .map_err(|e| e.to_string())?;
                let limit = i64::try_from(USAGE_RECENT_LIMIT).unwrap_or(i64::MAX);
                let recent = stmt
                    .query_map([limit], map_usage_row)
                    .map_err(|e| e.to_string())?
                    .filter_map(Result::ok)
                    .collect();
                Ok(UsageReport {
                    total_requests,
                    total_errors,
                    total_tokens_in,
                    total_tokens_out,
                    recent,
                })
            }
            Store::Json(store) => {
                let guard = store.lock().map_err(|_| "json store lock poisoned")?;
                let totals = guard.state.totals.clone();
                Ok(UsageReport {
                    total_requests: totals.requests,
                    total_errors: totals.errors,
                    total_tokens_in: totals.tokens_in,
                    total_tokens_out: totals.tokens_out,
                    recent: guard
                        .state
                        .recent
                        .iter()
                        .rev()
                        .filter(|row| !is_keep_warm_row(row))
                        .cloned()
                        .collect(),
                })
            }
        }
    }

    /// Failed requests (`status >= 400`) newest first, optional `since_ms` window.
    pub fn errors(&self, since_ms: Option<u64>) -> Result<(u64, Vec<ErrorEntry>), String> {
        match self {
            Store::Sqlite(conn) => {
                let guard = conn.lock().map_err(|_| "sqlite lock poisoned")?;
                let mut count_sql = format!(
                    "SELECT COUNT(*) FROM requests WHERE status >= {ERROR_STATUS_MIN} AND {}",
                    Self::not_keep_warm_sql()
                );
                let mut params: Vec<rusqlite::types::Value> = Vec::new();
                if let Some(since) = since_ms {
                    count_sql.push_str(" AND ts_ms >= ?");
                    params.push(rusqlite::types::Value::Integer(db_i64(since)));
                }
                let total_errors = guard
                    .query_row(&count_sql, rusqlite::params_from_iter(params.iter()), |r| {
                        Ok(db_u64(r.get::<_, i64>(0)?))
                    })
                    .map_err(|e| e.to_string())?;
                let mut list_sql = format!(
                    "SELECT ts_ms, model, profile, status, latency_ms, tokens_in, tokens_out,
                            ttft_ms, cache_read_tokens, cache_write_tokens, error, client, worker,
                            id, COALESCE(resolved_ms, (SELECT e.resolved_ms FROM error_resolutions e
                                WHERE e.reason = COALESCE(requests.error, '') AND e.resolved_ms >= requests.ts_ms))
                     FROM requests WHERE status >= {ERROR_STATUS_MIN} AND {}",
                    Self::not_keep_warm_sql()
                );
                if since_ms.is_some() {
                    list_sql.push_str(" AND ts_ms >= ?");
                }
                list_sql.push_str(" ORDER BY id DESC LIMIT ?");
                let mut list_params: Vec<rusqlite::types::Value> = Vec::new();
                if let Some(since) = since_ms {
                    list_params.push(rusqlite::types::Value::Integer(db_i64(since)));
                }
                list_params.push(rusqlite::types::Value::Integer(
                    i64::try_from(USAGE_RECENT_LIMIT).unwrap_or(i64::MAX),
                ));
                let mut stmt = guard.prepare(&list_sql).map_err(|e| e.to_string())?;
                let recent = stmt
                    .query_map(rusqlite::params_from_iter(list_params), |r| {
                        Ok(ErrorEntry {
                            id: Some(r.get(13)?),
                            resolved_ms: r.get::<_, Option<i64>>(14)?.map(db_u64),
                            row: map_usage_row(r)?,
                        })
                    })
                    .map_err(|e| e.to_string())?
                    .filter_map(Result::ok)
                    .collect();
                Ok((total_errors, recent))
            }
            Store::Json(store) => {
                let guard = store.lock().map_err(|_| "json store lock poisoned")?;
                let recent: Vec<ErrorEntry> = guard
                    .state
                    .recent
                    .iter()
                    .rev()
                    .filter(|row| {
                        row.status >= ERROR_STATUS_MIN
                            && !is_keep_warm_row(row)
                            && since_ms.is_none_or(|since| row.ts_ms >= since)
                    })
                    .cloned()
                    .map(|row| ErrorEntry {
                        id: None,
                        resolved_ms: None,
                        row,
                    })
                    .collect();
                let total_errors = if since_ms.is_none() {
                    guard.state.totals.errors
                } else {
                    u64::try_from(recent.len()).unwrap_or(u64::MAX)
                };
                Ok((total_errors, recent))
            }
        }
    }

    /// Count of failed rows in the window that are marked solved.
    pub fn solved_error_count(&self, since_ms: Option<u64>) -> Result<u64, String> {
        match self {
            Store::Sqlite(conn) => {
                let guard = conn.lock().map_err(|_| "sqlite lock poisoned")?;
                let mut sql = format!(
                    "SELECT COUNT(*) FROM requests WHERE status >= {ERROR_STATUS_MIN}
                     AND (resolved_ms IS NOT NULL OR EXISTS (SELECT 1 FROM error_resolutions e
                         WHERE e.reason = COALESCE(requests.error, '') AND e.resolved_ms >= requests.ts_ms))
                     AND {}",
                    Self::not_keep_warm_sql()
                );
                let mut params: Vec<rusqlite::types::Value> = Vec::new();
                if let Some(since) = since_ms {
                    sql.push_str(" AND ts_ms >= ?");
                    params.push(rusqlite::types::Value::Integer(db_i64(since)));
                }
                guard
                    .query_row(&sql, rusqlite::params_from_iter(params.iter()), |r| {
                        Ok(db_u64(r.get::<_, i64>(0)?))
                    })
                    .map_err(|e| e.to_string())
            }
            Store::Json(_) => Ok(0),
        }
    }

    /// Sets or clears `resolved_ms` on failed rows by id. Returns rows updated.
    /// Only the `SQLite` store keeps row ids, so the JSON store refuses.
    pub fn set_errors_resolved(
        &self,
        ids: &[i64],
        resolved_ms: Option<u64>,
    ) -> Result<u64, String> {
        match self {
            Store::Sqlite(conn) => {
                let guard = conn.lock().map_err(|_| "sqlite lock poisoned")?;
                let value = resolved_ms.map(|ms| rusqlite::types::Value::Integer(db_i64(ms)));
                let mut updated = 0_u64;
                let mut stmt = guard
                    .prepare(&format!(
                        "UPDATE requests SET resolved_ms = ?1
                         WHERE id = ?2 AND status >= {ERROR_STATUS_MIN}"
                    ))
                    .map_err(|e| e.to_string())?;
                for id in ids {
                    let changed = stmt
                        .execute(rusqlite::params![value.clone(), id])
                        .map_err(|e| e.to_string())?;
                    updated += u64::try_from(changed).unwrap_or(0);
                    match resolved_ms {
                        Some(ms) => {
                            guard
                                .execute(
                                    "INSERT INTO error_resolutions (reason, resolved_ms)
                                     SELECT COALESCE(error, ''), ?1 FROM requests
                                     WHERE id = ?2 AND status >= 400
                                     ON CONFLICT(reason) DO UPDATE SET resolved_ms = excluded.resolved_ms",
                                    rusqlite::params![db_i64(ms), id],
                                )
                                .map_err(|e| e.to_string())?;
                        }
                        None => {
                            guard
                                .execute(
                                    "DELETE FROM error_resolutions WHERE reason IN
                                     (SELECT COALESCE(error, '') FROM requests WHERE id = ?1 AND status >= 400)",
                                    rusqlite::params![id],
                                )
                                .map_err(|e| e.to_string())?;
                        }
                    }
                }
                Ok(updated)
            }
            Store::Json(_) => Err("error resolution needs the SQLite store".into()),
        }
    }

    pub(crate) fn model_request_counts(
        &self,
        since_ms: u64,
    ) -> Result<HashMap<String, u64>, String> {
        match self {
            Store::Sqlite(conn) => {
                let guard = conn.lock().map_err(|_| "sqlite lock poisoned")?;
                let mut stmt = guard
                    .prepare(&format!(
                        "SELECT model, COUNT(*) FROM requests WHERE ts_ms >= ? AND {} GROUP BY model",
                        Self::not_keep_warm_sql()
                    ))
                    .map_err(|error| error.to_string())?;
                let rows = stmt
                    .query_map([db_i64(since_ms)], |row| {
                        Ok((row.get::<_, String>(0)?, db_u64(row.get::<_, i64>(1)?)))
                    })
                    .map_err(|error| error.to_string())?;
                rows.collect::<Result<HashMap<_, _>, _>>()
                    .map_err(|error| error.to_string())
            }
            Store::Json(store) => {
                let guard = store.lock().map_err(|_| "json store lock poisoned")?;
                let mut counts = HashMap::new();
                for row in guard
                    .state
                    .recent
                    .iter()
                    .filter(|row| row.ts_ms >= since_ms && !is_keep_warm_row(row))
                {
                    *counts.entry(row.model.clone()).or_insert(0) += 1;
                }
                Ok(counts)
            }
        }
    }

    /// Aggregate filtered request history into the same shape as live stats.
    pub fn stats(&self, filter: &StatsFilter, book: &PricingBook) -> Result<StatsSnapshot, String> {
        let outcomes = self.filtered_outcomes(filter)?;
        let mut snapshot = snapshot_from_outcomes(&outcomes, &filter.range_label, true);
        if let Some((label, bucket_ms)) =
            crate::services::stats::series_bucket_for_range(&filter.range_label)
        {
            let until_ms = now_ms();
            let since_ms = filter
                .since_ms
                .unwrap_or_else(|| outcomes.first().map_or(until_ms, |row| row.ts_ms));
            snapshot.series_bucket = label.into();
            snapshot.series = crate::services::stats::series_from_outcomes(
                &outcomes, bucket_ms, since_ms, until_ms,
            );
        }
        apply_outcome_costs(&mut snapshot, &outcomes, book);
        Ok(snapshot)
    }

    /// Smart Auto requests grouped by tier and role, with their cost and verified attempt outcomes.
    /// The json store keeps no attempts, so it returns no rows.
    pub fn by_tier_role(
        &self,
        filter: &StatsFilter,
        book: &PricingBook,
    ) -> Result<Vec<TierRoleRow>, String> {
        match self {
            Store::Sqlite(conn) => {
                let requests = self.filtered_outcomes(filter)?;
                let since_ms = db_i64(filter.since_ms.unwrap_or(0));
                let guard = conn.lock().map_err(|_| "sqlite lock poisoned")?;
                let completed: HashSet<String> = {
                    let mut stmt = guard
                        .prepare(
                            "SELECT DISTINCT json_extract(event, '$.attempt') FROM selection_events
                             WHERE ts_ms >= ?1
                               AND json_extract(event, '$.state') = 'completed'
                               AND json_extract(event, '$.stage') = 'solver'
                               AND json_extract(event, '$.attempt') IS NOT NULL",
                        )
                        .map_err(|e| e.to_string())?;
                    let rows = stmt
                        .query_map([since_ms], |r| r.get::<_, String>(0))
                        .map_err(|e| e.to_string())?;
                    rows.filter_map(Result::ok).collect()
                };
                let verdicts: HashMap<String, String> = {
                    let mut stmt = guard
                        .prepare("SELECT attempt, code FROM attempt_outcomes WHERE ts_ms >= ?1")
                        .map_err(|e| e.to_string())?;
                    let rows = stmt
                        .query_map([since_ms], |r| {
                            Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?))
                        })
                        .map_err(|e| e.to_string())?;
                    rows.filter_map(Result::ok).collect()
                };
                Ok(group_tier_role(
                    &requests,
                    |request| outcome_cost_parts(book, request),
                    &completed,
                    &verdicts,
                ))
            }
            Store::Json(_) => Ok(Vec::new()),
        }
    }

    /// Saves an attempt's outcome. A lower-precedence source never replaces a higher one; an equal one does.
    /// The json store keeps no outcomes.
    pub fn save_attempt_outcome(
        &self,
        attempt: &str,
        worker: Option<&str>,
        outcome: &AttemptOutcome,
        ts_ms: u64,
    ) -> Result<(), String> {
        let Store::Sqlite(conn) = self else {
            return Ok(());
        };
        let guard = conn.lock().map_err(|_| "sqlite lock poisoned")?;
        guard
            .execute(
                "INSERT INTO attempt_outcomes (attempt, worker, code, source, ts_ms)
                 VALUES (?1, ?2, ?3, ?4, ?5)
                 ON CONFLICT(attempt) DO UPDATE SET
                    worker = COALESCE(excluded.worker, attempt_outcomes.worker),
                    code = excluded.code,
                    source = excluded.source,
                    ts_ms = excluded.ts_ms
                 WHERE (CASE excluded.source WHEN 'client' THEN 2 WHEN 'review' THEN 1 ELSE 0 END)
                    >= (CASE attempt_outcomes.source WHEN 'client' THEN 2 WHEN 'review' THEN 1 ELSE 0 END)",
                rusqlite::params![
                    attempt,
                    worker,
                    outcome.code.as_str(),
                    outcome.source.as_str(),
                    db_i64(ts_ms)
                ],
            )
            .map_err(|e| e.to_string())?;
        Ok(())
    }

    /// Stored outcomes for `attempts`, keyed by attempt. Unknown attempts are absent.
    pub fn load_attempt_outcomes(
        &self,
        attempts: &[String],
    ) -> Result<HashMap<String, AttemptOutcome>, String> {
        let Store::Sqlite(conn) = self else {
            return Ok(HashMap::new());
        };
        if attempts.is_empty() {
            return Ok(HashMap::new());
        }
        let guard = conn.lock().map_err(|_| "sqlite lock poisoned")?;
        let mut stmt = guard
            .prepare_cached("SELECT code, source FROM attempt_outcomes WHERE attempt = ?1")
            .map_err(|e| e.to_string())?;
        let mut found = HashMap::new();
        for attempt in attempts {
            let row = stmt
                .query_row([attempt], |r| {
                    Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?))
                })
                .optional()
                .map_err(|e| e.to_string())?;
            if let Some((code, source)) = row
                && let (Some(code), Some(source)) =
                    (OutcomeCode::parse(&code), OutcomeSource::parse(&source))
            {
                found.insert(attempt.clone(), AttemptOutcome { code, source });
            }
        }
        Ok(found)
    }

    /// Request outcomes matching `filter`, oldest first.
    pub fn filtered_outcomes(&self, filter: &StatsFilter) -> Result<Vec<RequestOutcome>, String> {
        match self {
            Store::Sqlite(conn) => {
                let guard = conn.lock().map_err(|_| "sqlite lock poisoned")?;
                let mut sql = String::from(
                    "SELECT model, profile, status, latency_ms, tokens_in, tokens_out,
                            ttft_ms, cache_read_tokens, cache_write_tokens, ts_ms, error, client, worker,
                            routed_by, attempt, role
                     FROM requests WHERE 1=1",
                );
                let mut params: Vec<rusqlite::types::Value> = Vec::new();
                if let Some(since) = filter.since_ms {
                    sql.push_str(" AND ts_ms >= ?");
                    params.push(rusqlite::types::Value::Integer(db_i64(since)));
                }
                if let Some(profile) = &filter.profile {
                    sql.push_str(" AND profile = ?");
                    params.push(rusqlite::types::Value::Text(profile.clone()));
                }
                if let Some(model) = &filter.model {
                    sql.push_str(" AND model = ?");
                    params.push(rusqlite::types::Value::Text(model.clone()));
                }
                if let Some(prefix) = &filter.routed_by_prefix {
                    sql.push_str(" AND routed_by LIKE ?");
                    params.push(rusqlite::types::Value::Text(format!("{prefix}%")));
                }
                sql.push_str(" ORDER BY id");
                let mut stmt = guard.prepare(&sql).map_err(|e| e.to_string())?;
                let rows = stmt
                    .query_map(rusqlite::params_from_iter(params), |r| {
                        Ok(RequestOutcome {
                            model_key: r.get(0)?,
                            profile: r.get(1)?,
                            status: u16::try_from(r.get::<_, i64>(2)?).unwrap_or(0),
                            latency_ms: db_u64(r.get::<_, i64>(3)?),
                            tokens_in: db_u64(r.get::<_, i64>(4)?),
                            tokens_out: db_u64(r.get::<_, i64>(5)?),
                            ttft_ms: r.get::<_, Option<i64>>(6)?.map(db_u64),
                            cache_read_tokens: db_u64(r.get::<_, i64>(7).unwrap_or(0)),
                            cache_write_tokens: db_u64(r.get::<_, i64>(8).unwrap_or(0)),
                            ts_ms: db_u64(r.get::<_, i64>(9).unwrap_or(0)),
                            error: r.get::<_, Option<String>>(10).unwrap_or(None),
                            client: r.get::<_, Option<String>>(11).unwrap_or(None),
                            worker: r.get::<_, Option<String>>(12).unwrap_or(None),
                            routed_by: r.get::<_, Option<String>>(13).unwrap_or(None),
                            attempt: r.get::<_, Option<String>>(14).unwrap_or(None),
                            role: r.get::<_, Option<String>>(15).unwrap_or(None),
                        })
                    })
                    .map_err(|e| e.to_string())?
                    .filter_map(Result::ok)
                    .collect();
                Ok(rows)
            }
            Store::Json(store) => {
                let guard = store.lock().map_err(|_| "json store lock poisoned")?;
                let rows = guard
                    .state
                    .recent
                    .iter()
                    .filter(|row| {
                        filter.since_ms.is_none_or(|since| row.ts_ms >= since)
                            && filter
                                .profile
                                .as_ref()
                                .is_none_or(|profile| row.profile == *profile)
                            && filter
                                .model
                                .as_ref()
                                .is_none_or(|model| row.model == *model)
                            && filter.routed_by_prefix.is_none()
                    })
                    .map(|row| RequestOutcome {
                        profile: row.profile.clone(),
                        model_key: row.model.clone(),
                        status: row.status,
                        latency_ms: row.latency_ms,
                        ttft_ms: row.ttft_ms,
                        tokens_in: row.tokens_in,
                        tokens_out: row.tokens_out,
                        cache_read_tokens: row.cache_read_tokens,
                        cache_write_tokens: row.cache_write_tokens,
                        ts_ms: row.ts_ms,
                        error: row.error.clone(),
                        client: row.client.clone(),
                        worker: row.worker.clone(),
                        routed_by: None,
                        attempt: None,
                        role: None,
                    })
                    .collect();
                Ok(rows)
            }
        }
    }

    pub fn list_api_keys(&self) -> Result<Vec<ApiKeyRecord>, String> {
        match self {
            Store::Sqlite(conn) => {
                let guard = conn.lock().map_err(|_| "sqlite lock poisoned")?;
                let mut stmt = guard
                    .prepare("SELECT name, key_hash, masked, enabled, created_ms FROM api_keys ORDER BY created_ms")
                    .map_err(|e| e.to_string())?;
                let keys = stmt
                    .query_map([], |r| {
                        Ok(ApiKeyRecord {
                            name: r.get(0)?,
                            key_hash: r.get(1)?,
                            masked: r.get(2)?,
                            enabled: r.get::<_, i64>(3)? != 0,
                            created_ms: db_u64(r.get::<_, i64>(4)?),
                        })
                    })
                    .map_err(|e| e.to_string())?
                    .filter_map(Result::ok)
                    .collect();
                Ok(keys)
            }
            Store::Json(store) => {
                let guard = store.lock().map_err(|_| "json store lock poisoned")?;
                Ok(guard.state.api_keys.clone())
            }
        }
    }

    pub fn insert_api_key(&self, record: &ApiKeyRecord) -> Result<(), String> {
        match self {
            Store::Sqlite(conn) => {
                let guard = conn.lock().map_err(|_| "sqlite lock poisoned")?;
                guard
                    .execute(
                        "INSERT INTO api_keys (name, key_hash, masked, enabled, created_ms) VALUES (?1, ?2, ?3, ?4, ?5)",
                        rusqlite::params![
                            record.name,
                            record.key_hash,
                            record.masked,
                            i64::from(record.enabled),
                            db_i64(record.created_ms)
                        ],
                    )
                    .map_err(|e| format!("insert api key failed (duplicate name?): {e}"))?;
                Ok(())
            }
            Store::Json(store) => {
                let mut guard = store.lock().map_err(|_| "json store lock poisoned")?;
                if guard.state.api_keys.iter().any(|k| k.name == record.name) {
                    return Err(format!("api key name already exists: {}", record.name));
                }
                guard.state.api_keys.push(record.clone());
                guard.flush()
            }
        }
    }

    pub fn delete_api_key(&self, name: &str) -> Result<(), String> {
        match self {
            Store::Sqlite(conn) => {
                let guard = conn.lock().map_err(|_| "sqlite lock poisoned")?;
                guard
                    .execute("DELETE FROM api_keys WHERE name = ?1", [name])
                    .map_err(|e| e.to_string())?;
                Ok(())
            }
            Store::Json(store) => {
                let mut guard = store.lock().map_err(|_| "json store lock poisoned")?;
                guard.state.api_keys.retain(|k| k.name != name);
                guard.flush()
            }
        }
    }
    /// Profile name -> the raw upstream `/models` entries last saved for it.
    pub fn load_model_catalogs(&self) -> Result<HashMap<String, Vec<Value>>, String> {
        match self {
            Store::Sqlite(conn) => {
                let guard = conn.lock().map_err(|_| "sqlite lock poisoned")?;
                let mut stmt = guard
                    .prepare("SELECT profile, entries FROM model_catalogs")
                    .map_err(|e| e.to_string())?;
                let rows: Vec<(String, String)> = stmt
                    .query_map([], |r| Ok((r.get(0)?, r.get(1)?)))
                    .map_err(|e| e.to_string())?
                    .filter_map(Result::ok)
                    .collect();
                Ok(rows
                    .into_iter()
                    .filter_map(|(profile, entries)| {
                        let entries = serde_json::from_str(&entries)
                            .inspect_err(|e| {
                                tracing::warn!(profile, "stored model catalog is unreadable: {e}");
                            })
                            .ok()?;
                        Some((profile, entries))
                    })
                    .collect())
            }
            Store::Json(store) => {
                let guard = store.lock().map_err(|_| "json store lock poisoned")?;
                Ok(guard.state.model_catalogs.clone())
            }
        }
    }

    /// Replaces the saved catalog of `profile`.
    pub fn save_model_catalog(&self, profile: &str, entries: &[Value]) -> Result<(), String> {
        match self {
            Store::Sqlite(conn) => {
                let json = serde_json::to_string(entries).map_err(|e| e.to_string())?;
                let guard = conn.lock().map_err(|_| "sqlite lock poisoned")?;
                guard
                    .execute(
                        "INSERT INTO model_catalogs (profile, entries, updated_ms) VALUES (?1, ?2, ?3)
                         ON CONFLICT(profile) DO UPDATE SET
                             entries = excluded.entries, updated_ms = excluded.updated_ms",
                        rusqlite::params![profile, json, db_i64(now_ms())],
                    )
                    .map_err(|e| e.to_string())?;
                Ok(())
            }
            Store::Json(store) => {
                let mut guard = store.lock().map_err(|_| "json store lock poisoned")?;
                guard
                    .state
                    .model_catalogs
                    .insert(profile.to_string(), entries.to_vec());
                guard.flush()
            }
        }
    }

    /// Saves `seeds` for `profile`. A model whose limit was learned keeps it.
    pub fn seed_model_limits(
        &self,
        profile: &str,
        seeds: &[(&str, u64)],
        now_ms: u64,
    ) -> Result<(), String> {
        match self {
            Store::Sqlite(conn) => {
                let guard = conn.lock().map_err(|_| "sqlite lock poisoned")?;
                for (model, limit) in seeds {
                    guard
                        .execute(
                            "INSERT INTO model_limits (profile, model, prompt_limit, source, learned_at)
                             VALUES (?1, ?2, ?3, ?4, ?5)
                             ON CONFLICT(profile, model) DO UPDATE SET
                                 prompt_limit = excluded.prompt_limit, learned_at = excluded.learned_at
                             WHERE model_limits.source = excluded.source
                                 AND model_limits.prompt_limit <> excluded.prompt_limit",
                            rusqlite::params![
                                profile,
                                model,
                                db_i64(*limit),
                                LimitSource::Seeded.as_str(),
                                db_i64(now_ms)
                            ],
                        )
                        .map_err(|e| e.to_string())?;
                }
                Ok(())
            }
            Store::Json(store) => {
                let mut guard = store.lock().map_err(|_| "json store lock poisoned")?;
                let limits = guard
                    .state
                    .model_limits
                    .entry(profile.to_string())
                    .or_default();
                let mut changed = false;
                for (model, limit) in seeds {
                    let stale = limits.get(*model).is_none_or(|current| {
                        current.source == LimitSource::Seeded && current.limit != *limit
                    });
                    if stale {
                        let entry = ModelLimit {
                            limit: *limit,
                            source: LimitSource::Seeded,
                            learned_at: now_ms,
                        };
                        limits.insert((*model).to_string(), entry);
                        changed = true;
                    }
                }
                if changed { guard.flush() } else { Ok(()) }
            }
        }
    }

    /// Profile name -> upstream model id -> the prompt limit saved for it.
    pub fn load_model_limits(
        &self,
    ) -> Result<HashMap<String, HashMap<String, ModelLimit>>, String> {
        match self {
            Store::Sqlite(conn) => {
                let guard = conn.lock().map_err(|_| "sqlite lock poisoned")?;
                let mut stmt = guard
                    .prepare(
                        "SELECT profile, model, prompt_limit, source, learned_at FROM model_limits",
                    )
                    .map_err(|e| e.to_string())?;
                let rows: Vec<(String, String, i64, String, i64)> = stmt
                    .query_map([], |r| {
                        Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?))
                    })
                    .map_err(|e| e.to_string())?
                    .filter_map(Result::ok)
                    .collect();
                let mut limits: HashMap<String, HashMap<String, ModelLimit>> = HashMap::new();
                for (profile, model, limit, source, learned_at) in rows {
                    let Some(source) = LimitSource::parse(&source) else {
                        tracing::warn!(
                            profile,
                            model,
                            "stored prompt limit has an unknown source: {source}"
                        );
                        continue;
                    };
                    let entry = ModelLimit {
                        limit: db_u64(limit),
                        source,
                        learned_at: db_u64(learned_at),
                    };
                    limits.entry(profile).or_default().insert(model, entry);
                }
                Ok(limits)
            }
            Store::Json(store) => {
                let guard = store.lock().map_err(|_| "json store lock poisoned")?;
                Ok(guard.state.model_limits.clone())
            }
        }
    }

    /// Replaces the saved prompt limit of `model` on `profile`.
    pub fn save_model_limit(
        &self,
        profile: &str,
        model: &str,
        limit: &ModelLimit,
    ) -> Result<(), String> {
        match self {
            Store::Sqlite(conn) => {
                let guard = conn.lock().map_err(|_| "sqlite lock poisoned")?;
                guard
                    .execute(
                        "INSERT INTO model_limits (profile, model, prompt_limit, source, learned_at)
                         VALUES (?1, ?2, ?3, ?4, ?5)
                         ON CONFLICT(profile, model) DO UPDATE SET
                             prompt_limit = excluded.prompt_limit, source = excluded.source,
                             learned_at = excluded.learned_at",
                        rusqlite::params![
                            profile,
                            model,
                            db_i64(limit.limit),
                            limit.source.as_str(),
                            db_i64(limit.learned_at)
                        ],
                    )
                    .map_err(|e| e.to_string())?;
                Ok(())
            }
            Store::Json(store) => {
                let mut guard = store.lock().map_err(|_| "json store lock poisoned")?;
                guard
                    .state
                    .model_limits
                    .entry(profile.to_string())
                    .or_default()
                    .insert(model.to_string(), *limit);
                guard.flush()
            }
        }
    }

    /// Profile name -> the stored body fields its upstream rejects.
    pub fn load_unsupported_body_fields(&self) -> Result<HashMap<String, Vec<String>>, String> {
        match self {
            Store::Sqlite(conn) => {
                let guard = conn.lock().map_err(|_| "sqlite lock poisoned")?;
                let mut stmt = guard
                    .prepare(
                        "SELECT profile, field FROM unsupported_body_fields ORDER BY learned_at",
                    )
                    .map_err(|e| e.to_string())?;
                let rows: Vec<(String, String)> = stmt
                    .query_map([], |r| Ok((r.get(0)?, r.get(1)?)))
                    .map_err(|e| e.to_string())?
                    .filter_map(Result::ok)
                    .collect();
                let mut fields: HashMap<String, Vec<String>> = HashMap::new();
                for (profile, field) in rows {
                    fields.entry(profile).or_default().push(field);
                }
                Ok(fields)
            }
            Store::Json(store) => {
                let guard = store.lock().map_err(|_| "json store lock poisoned")?;
                Ok(guard.state.unsupported_body_fields.clone())
            }
        }
    }

    /// Adds `field` to the rejected body fields of `profile`. Saving a known field is a no-op.
    pub fn save_unsupported_body_field(
        &self,
        profile: &str,
        field: &str,
        now_ms: u64,
    ) -> Result<(), String> {
        match self {
            Store::Sqlite(conn) => {
                let guard = conn.lock().map_err(|_| "sqlite lock poisoned")?;
                guard
                    .execute(
                        "INSERT INTO unsupported_body_fields (profile, field, learned_at)
                         VALUES (?1, ?2, ?3)
                         ON CONFLICT(profile, field) DO NOTHING",
                        rusqlite::params![profile, field, db_i64(now_ms)],
                    )
                    .map_err(|e| e.to_string())?;
                Ok(())
            }
            Store::Json(store) => {
                let mut guard = store.lock().map_err(|_| "json store lock poisoned")?;
                let fields = guard
                    .state
                    .unsupported_body_fields
                    .entry(profile.to_string())
                    .or_default();
                if fields.iter().any(|known| known == field) {
                    return Ok(());
                }
                fields.push(field.to_string());
                guard.flush()
            }
        }
    }

    /// Every `(profile, model)` that refused image input, as learned and saved.
    pub fn load_vision_refusals(&self) -> Result<Vec<(String, String)>, String> {
        match self {
            Store::Sqlite(conn) => {
                let guard = conn.lock().map_err(|_| "sqlite lock poisoned")?;
                let mut stmt = guard
                    .prepare("SELECT profile, model FROM vision_refusals ORDER BY learned_at")
                    .map_err(|e| e.to_string())?;
                let rows = stmt
                    .query_map([], |r| Ok((r.get(0)?, r.get(1)?)))
                    .map_err(|e| e.to_string())?
                    .filter_map(Result::ok)
                    .collect();
                Ok(rows)
            }
            Store::Json(store) => {
                let guard = store.lock().map_err(|_| "json store lock poisoned")?;
                Ok(guard
                    .state
                    .vision_refusals
                    .iter()
                    .flat_map(|(profile, models)| {
                        models.iter().map(|m| (profile.clone(), m.clone()))
                    })
                    .collect())
            }
        }
    }

    /// Saves that `model` on `profile` refused image input. Saving a known pair is a no-op.
    pub fn save_vision_refusal(
        &self,
        profile: &str,
        model: &str,
        now_ms: u64,
    ) -> Result<(), String> {
        match self {
            Store::Sqlite(conn) => {
                let guard = conn.lock().map_err(|_| "sqlite lock poisoned")?;
                guard
                    .execute(
                        "INSERT INTO vision_refusals (profile, model, source, learned_at)
                         VALUES (?1, ?2, 'learned', ?3)
                         ON CONFLICT(profile, model) DO NOTHING",
                        rusqlite::params![profile, model, db_i64(now_ms)],
                    )
                    .map_err(|e| e.to_string())?;
                Ok(())
            }
            Store::Json(store) => {
                let mut guard = store.lock().map_err(|_| "json store lock poisoned")?;
                let models = guard
                    .state
                    .vision_refusals
                    .entry(profile.to_string())
                    .or_default();
                if models.iter().any(|known| known == model) {
                    return Ok(());
                }
                models.push(model.to_string());
                guard.flush()
            }
        }
    }

    /// The salt of the selection scope keys. Stores `fresh` when none is saved yet, and returns the saved salt.
    pub fn install_salt(&self, fresh: &str) -> Result<String, String> {
        match self {
            Store::Sqlite(conn) => {
                let guard = conn.lock().map_err(|_| "sqlite lock poisoned")?;
                guard
                    .execute(
                        "INSERT INTO install_settings (name, value) VALUES (?1, ?2)
                         ON CONFLICT(name) DO NOTHING",
                        rusqlite::params![INSTALL_SALT_NAME, fresh],
                    )
                    .map_err(|e| e.to_string())?;
                guard
                    .query_row(
                        "SELECT value FROM install_settings WHERE name = ?1",
                        rusqlite::params![INSTALL_SALT_NAME],
                        |r| r.get(0),
                    )
                    .map_err(|e| e.to_string())
            }
            Store::Json(store) => {
                let mut guard = store.lock().map_err(|_| "json store lock poisoned")?;
                if let Some(saved) = &guard.state.install_salt {
                    return Ok(saved.clone());
                }
                guard.state.install_salt = Some(fresh.to_string());
                guard.flush()?;
                Ok(fresh.to_string())
            }
        }
    }

    /// Saves `events`, then drops events older than the retention age. A `max_events` of zero or more also keeps only the newest that many.
    pub fn save_selection_events(
        &self,
        events: &[SelectionEvent],
        now_ms: u64,
        max_events: i64,
    ) -> Result<(), String> {
        let cutoff = now_ms.saturating_sub(SELECTION_RETENTION_MS);
        match self {
            Store::Sqlite(conn) => {
                let mut guard = conn.lock().map_err(|_| "sqlite lock poisoned")?;
                let tx = guard.transaction().map_err(|e| e.to_string())?;
                {
                    let mut insert = tx
                        .prepare(
                            "INSERT OR REPLACE INTO selection_events (seq, ts_ms, event)
                             VALUES (?1, ?2, ?3)",
                        )
                        .map_err(|e| e.to_string())?;
                    for event in events {
                        let json = serde_json::to_string(event).map_err(|e| e.to_string())?;
                        insert
                            .execute(rusqlite::params![
                                db_i64(event.seq),
                                db_i64(event.ts_ms),
                                json
                            ])
                            .map_err(|e| e.to_string())?;
                    }
                }
                tx.execute(
                    "DELETE FROM selection_events WHERE ts_ms < ?1",
                    rusqlite::params![db_i64(cutoff)],
                )
                .map_err(|e| e.to_string())?;
                if let Ok(keep) = usize::try_from(max_events) {
                    tx.execute(
                        "DELETE FROM selection_events WHERE seq <= COALESCE(
                             (SELECT seq FROM selection_events ORDER BY seq DESC LIMIT 1 OFFSET ?1), 0)",
                        rusqlite::params![i64::try_from(keep).unwrap_or(i64::MAX)],
                    )
                    .map_err(|e| e.to_string())?;
                }
                tx.commit().map_err(|e| e.to_string())
            }
            Store::Json(store) => {
                let mut guard = store.lock().map_err(|_| "json store lock poisoned")?;
                let stored = &mut guard.state.selection_events;
                stored.extend_from_slice(events);
                stored.retain(|event| event.ts_ms >= cutoff);
                let excess =
                    usize::try_from(max_events).map_or(0, |keep| stored.len().saturating_sub(keep));
                if excess > 0 {
                    stored.drain(..excess);
                }
                guard.flush()
            }
        }
    }

    /// Saves the prompt text of one dispatch attempt. Texts older than the retention age are dropped.
    /// Only the sqlite store keeps texts; the json store returns an error.
    pub fn save_selection_prompt(
        &self,
        attempt: &str,
        now_ms: u64,
        prompt: &str,
    ) -> Result<(), String> {
        self.save_selection_text(
            "INSERT INTO selection_texts (attempt, ts_ms, prompt) VALUES (?1, ?2, ?3)
             ON CONFLICT(attempt) DO UPDATE SET prompt = excluded.prompt, ts_ms = excluded.ts_ms",
            attempt,
            now_ms,
            prompt,
        )
    }

    /// Saves the answer text of one dispatch attempt. Texts older than the retention age are dropped.
    /// Only the sqlite store keeps texts; the json store returns an error.
    pub fn save_selection_answer(
        &self,
        attempt: &str,
        now_ms: u64,
        answer: &str,
    ) -> Result<(), String> {
        self.save_selection_text(
            "INSERT INTO selection_texts (attempt, ts_ms, answer) VALUES (?1, ?2, ?3)
             ON CONFLICT(attempt) DO UPDATE SET answer = excluded.answer, ts_ms = excluded.ts_ms",
            attempt,
            now_ms,
            answer,
        )
    }

    fn save_selection_text(
        &self,
        upsert: &str,
        attempt: &str,
        now_ms: u64,
        text: &str,
    ) -> Result<(), String> {
        let Store::Sqlite(conn) = self else {
            return Err("selection texts need the sqlite store".to_owned());
        };
        let cutoff = now_ms.saturating_sub(SELECTION_RETENTION_MS);
        let mut guard = conn.lock().map_err(|_| "sqlite lock poisoned")?;
        let tx = guard.transaction().map_err(|e| e.to_string())?;
        tx.execute(upsert, rusqlite::params![attempt, db_i64(now_ms), text])
            .map_err(|e| e.to_string())?;
        tx.execute(
            "DELETE FROM selection_texts WHERE ts_ms < ?1",
            rusqlite::params![db_i64(cutoff)],
        )
        .map_err(|e| e.to_string())?;
        tx.commit().map_err(|e| e.to_string())
    }

    /// The stored `(prompt, answer)` of one dispatch attempt, or `None` when none was kept.
    pub fn load_selection_text(&self, attempt: &str) -> Result<Option<(String, String)>, String> {
        let Store::Sqlite(conn) = self else {
            return Err("selection texts need the sqlite store".to_owned());
        };
        let guard = conn.lock().map_err(|_| "sqlite lock poisoned")?;
        let mut stmt = guard
            .prepare("SELECT prompt, answer FROM selection_texts WHERE attempt = ?1")
            .map_err(|e| e.to_string())?;
        let mut rows = stmt
            .query_map(rusqlite::params![attempt], |r| {
                Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?))
            })
            .map_err(|e| e.to_string())?;
        rows.next().transpose().map_err(|e| e.to_string())
    }

    /// Up to `limit` selection events saved at or after `since_ts_ms` with a seq above `after_seq`, oldest first.
    pub fn load_selection_events_since(
        &self,
        since_ts_ms: u64,
        after_seq: u64,
        limit: usize,
    ) -> Result<Vec<SelectionEvent>, String> {
        match self {
            Store::Sqlite(conn) => {
                let guard = conn.lock().map_err(|_| "sqlite lock poisoned")?;
                let mut stmt = guard
                    .prepare(
                        "SELECT event FROM selection_events
                         WHERE ts_ms >= ?1 AND seq > ?2 ORDER BY seq LIMIT ?3",
                    )
                    .map_err(|e| e.to_string())?;
                let rows: Vec<String> = stmt
                    .query_map(
                        rusqlite::params![
                            db_i64(since_ts_ms),
                            db_i64(after_seq),
                            i64::try_from(limit).unwrap_or(i64::MAX)
                        ],
                        |r| r.get::<_, String>(0),
                    )
                    .map_err(|e| e.to_string())?
                    .filter_map(Result::ok)
                    .collect();
                Ok(rows
                    .iter()
                    .filter_map(|json| {
                        serde_json::from_str::<SelectionEvent>(json)
                            .inspect_err(|e| {
                                tracing::warn!("stored selection event is unreadable: {e}");
                            })
                            .ok()
                    })
                    .collect())
            }
            Store::Json(store) => {
                let guard = store.lock().map_err(|_| "json store lock poisoned")?;
                Ok(guard
                    .state
                    .selection_events
                    .iter()
                    .filter(|event| event.ts_ms >= since_ts_ms && event.seq > after_seq)
                    .take(limit)
                    .cloned()
                    .collect())
            }
        }
    }

    /// The newest `limit` selection events, oldest first.
    pub fn load_selection_events(&self, limit: usize) -> Result<Vec<SelectionEvent>, String> {
        match self {
            Store::Sqlite(conn) => {
                let guard = conn.lock().map_err(|_| "sqlite lock poisoned")?;
                let mut stmt = guard
                    .prepare(
                        "SELECT event FROM (
                             SELECT seq, event FROM selection_events ORDER BY seq DESC LIMIT ?1
                         ) ORDER BY seq",
                    )
                    .map_err(|e| e.to_string())?;
                let rows: Vec<String> = stmt
                    .query_map(
                        rusqlite::params![i64::try_from(limit).unwrap_or(i64::MAX)],
                        |r| r.get::<_, String>(0),
                    )
                    .map_err(|e| e.to_string())?
                    .filter_map(Result::ok)
                    .collect();
                Ok(rows
                    .iter()
                    .filter_map(|json| {
                        serde_json::from_str::<SelectionEvent>(json)
                            .inspect_err(|e| {
                                tracing::warn!("stored selection event is unreadable: {e}");
                            })
                            .ok()
                    })
                    .collect())
            }
            Store::Json(store) => {
                let guard = store.lock().map_err(|_| "json store lock poisoned")?;
                let stored = &guard.state.selection_events;
                let skip = stored.len().saturating_sub(limit);
                Ok(stored[skip..].to_vec())
            }
        }
    }
}

impl JsonStore {
    fn flush(&self) -> Result<(), String> {
        let bytes = serde_json::to_vec_pretty(&self.state).map_err(|e| e.to_string())?;
        let tmp = self.path.with_extension("tmp");
        std::fs::write(&tmp, bytes).map_err(|e| e.to_string())?;
        std::fs::rename(&tmp, &self.path).map_err(|e| e.to_string())
    }
}

pub fn hash_key(key: &str) -> String {
    crate::utils::hex::to_hex(&Sha256::digest(key.as_bytes()))
}

pub fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| u64::try_from(d.as_millis()).unwrap_or(u64::MAX))
}

/// True when the row is a keep-warm ping, which counts toward cost but not request totals.
fn is_keep_warm_row(row: &UsageRow) -> bool {
    is_keep_warm(row.client.as_deref())
}

/// `SQLite` stores only `i64`; values here are counters/timestamps, so clamping
/// at the type bounds is the correct lossy edge behavior.
fn db_i64(v: u64) -> i64 {
    i64::try_from(v).unwrap_or(i64::MAX)
}

fn db_u64(v: i64) -> u64 {
    u64::try_from(v).unwrap_or(0)
}

fn table_columns(conn: &Connection, table: &str) -> Result<Vec<String>, String> {
    let mut stmt = conn
        .prepare(&format!("PRAGMA table_info({table})"))
        .map_err(|e| e.to_string())?;
    stmt.query_map([], |r| r.get::<_, String>(1))
        .map_err(|e| e.to_string())
        .map(|rows| rows.filter_map(Result::ok).collect())
}

fn migrate_requests_columns(conn: &Connection) -> Result<(), String> {
    // A solved reason stays solved only for failures at or before its solve time.
    conn.execute_batch(
        "CREATE TABLE IF NOT EXISTS error_resolutions (
            reason TEXT PRIMARY KEY,
            resolved_ms INTEGER NOT NULL
        );",
    )
    .map_err(|e| e.to_string())?;
    let existing = table_columns(conn, "requests")?;
    let add = |name: &str, ddl: &str| Store::add_missing_column(conn, &existing, name, ddl);
    add("ttft_ms", "ALTER TABLE requests ADD COLUMN ttft_ms INTEGER")?;
    add(
        "cache_read_tokens",
        "ALTER TABLE requests ADD COLUMN cache_read_tokens INTEGER NOT NULL DEFAULT 0",
    )?;
    add(
        "cache_write_tokens",
        "ALTER TABLE requests ADD COLUMN cache_write_tokens INTEGER NOT NULL DEFAULT 0",
    )?;
    add("error", "ALTER TABLE requests ADD COLUMN error TEXT")?;
    add("client", "ALTER TABLE requests ADD COLUMN client TEXT")?;
    add("worker", "ALTER TABLE requests ADD COLUMN worker TEXT")?;
    add(
        "routed_by",
        "ALTER TABLE requests ADD COLUMN routed_by TEXT",
    )?;
    add("attempt", "ALTER TABLE requests ADD COLUMN attempt TEXT")?;
    add(
        "role",
        "ALTER TABLE requests ADD COLUMN role TEXT CONSTRAINT ck_requests_role CHECK (role IS NULL OR role IN ('planning', 'investigation', 'implementation', 'validation', 'conversation'))",
    )?;
    add(
        "resolved_ms",
        "ALTER TABLE requests ADD COLUMN resolved_ms INTEGER",
    )?;
    conn.execute(
        "CREATE INDEX IF NOT EXISTS idx_requests_worker ON requests(worker)",
        [],
    )
    .map_err(|e| format!("sqlite migrate idx_requests_worker: {e}"))?;
    conn.execute(
        "CREATE INDEX IF NOT EXISTS idx_requests_attempt ON requests(attempt)",
        [],
    )
    .map_err(|e| format!("sqlite migrate idx_requests_attempt: {e}"))?;
    Ok(())
}

fn migrate_request_shapes_columns(conn: &Connection) -> Result<(), String> {
    let existing = table_columns(conn, "request_shapes")?;
    let add = |name: &str, ddl: &str| Store::add_missing_column(conn, &existing, name, ddl);
    add(
        "stream_key",
        "ALTER TABLE request_shapes ADD COLUMN stream_key TEXT",
    )?;
    add(
        "tool_result_bytes",
        "ALTER TABLE request_shapes ADD COLUMN tool_result_bytes TEXT",
    )?;
    add(
        "skill_body_chars",
        "ALTER TABLE request_shapes ADD COLUMN skill_body_chars TEXT",
    )?;
    Ok(())
}

fn map_usage_row(r: &rusqlite::Row<'_>) -> rusqlite::Result<UsageRow> {
    Ok(UsageRow {
        ts_ms: db_u64(r.get::<_, i64>(0)?),
        model: r.get(1)?,
        profile: r.get(2)?,
        status: u16::try_from(r.get::<_, i64>(3)?).unwrap_or(0),
        latency_ms: db_u64(r.get::<_, i64>(4)?),
        tokens_in: db_u64(r.get::<_, i64>(5)?),
        tokens_out: db_u64(r.get::<_, i64>(6)?),
        ttft_ms: r.get::<_, Option<i64>>(7)?.map(db_u64),
        cache_read_tokens: db_u64(r.get::<_, i64>(8).unwrap_or(0)),
        cache_write_tokens: db_u64(r.get::<_, i64>(9).unwrap_or(0)),
        cost_usd: 0.0,
        error: r.get::<_, Option<String>>(10).unwrap_or(None),
        client: r.get::<_, Option<String>>(11).unwrap_or(None),
        worker: r.get::<_, Option<String>>(12).unwrap_or(None),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample_outcome() -> RequestOutcome {
        RequestOutcome {
            profile: "openai".into(),
            model_key: "openai/gpt-4o".into(),
            status: 200,
            latency_ms: 42,
            ttft_ms: Some(12),
            tokens_in: 5,
            tokens_out: 7,
            cache_read_tokens: 3,
            cache_write_tokens: 1,
            ts_ms: 0,
            error: None,
            client: None,
            worker: None,
            routed_by: None,
            attempt: None,
            role: None,
        }
    }

    #[test]
    fn model_counts_include_only_requests_within_window() {
        for kind in ["sqlite", "json"] {
            let dir = tempfile::tempdir().unwrap();
            let path = dir.path().join("counts");
            let store = Store::open(kind, Some(path.to_str().unwrap())).unwrap();
            let mut outcome = sample_outcome();
            store.record(&outcome).unwrap();
            store.record(&outcome).unwrap();
            outcome.model_key = "other/model".into();
            store.record(&outcome).unwrap();
            assert_eq!(
                store.model_request_counts(0).unwrap(),
                HashMap::from([("openai/gpt-4o".into(), 2), ("other/model".into(), 1)])
            );
            let an_hour = 3_600_000;
            assert!(
                store
                    .model_request_counts(now_ms() + an_hour)
                    .unwrap()
                    .is_empty()
            );
        }
    }

    #[test]
    fn keep_warm_pings_are_not_counted_as_user_requests() {
        for kind in ["sqlite", "json"] {
            let dir = tempfile::tempdir().unwrap();
            let path = dir.path().join("pings");
            let store = Store::open(kind, Some(path.to_str().unwrap())).unwrap();
            store.record(&sample_outcome()).unwrap();
            let mut ping = sample_outcome();
            ping.client = Some(KEEP_WARM_CLIENT.into());
            store.record(&ping).unwrap();
            ping.status = 400;
            store.record(&ping).unwrap();

            let usage = store.usage().unwrap();
            assert_eq!(usage.total_requests, 1, "{kind}");
            assert_eq!(usage.total_errors, 0, "{kind}");
            assert_eq!(usage.recent.len(), 1, "{kind}");
            assert_eq!(store.errors(None).unwrap().0, 0, "{kind}");
            assert_eq!(
                store.model_request_counts(0).unwrap(),
                HashMap::from([("openai/gpt-4o".into(), 1)]),
                "{kind}"
            );
        }
    }

    #[test]
    fn sqlite_round_trip() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("t.db");
        let store = Store::open("sqlite", Some(path.to_str().unwrap())).unwrap();
        store.record(&sample_outcome()).unwrap();
        let usage = store.usage().unwrap();
        assert_eq!(usage.total_requests, 1);
        assert_eq!(usage.recent.len(), 1);
        assert_eq!(usage.recent[0].tokens_out, 7);
        assert_eq!(usage.recent[0].cache_read_tokens, 3);
        assert_eq!(usage.recent[0].ttft_ms, Some(12));
    }

    #[test]
    fn sqlite_database_from_before_worker_gains_the_column_and_keeps_rows() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("old.db");
        {
            let conn = rusqlite::Connection::open(&path).unwrap();
            conn.execute_batch(
                "CREATE TABLE requests (
                    id INTEGER PRIMARY KEY AUTOINCREMENT,
                    ts_ms INTEGER NOT NULL,
                    model TEXT NOT NULL,
                    profile TEXT NOT NULL,
                    status INTEGER NOT NULL,
                    latency_ms INTEGER NOT NULL,
                    tokens_in INTEGER NOT NULL,
                    tokens_out INTEGER NOT NULL,
                    ttft_ms INTEGER,
                    cache_read_tokens INTEGER NOT NULL DEFAULT 0,
                    cache_write_tokens INTEGER NOT NULL DEFAULT 0,
                    error TEXT,
                    client TEXT
                );
                INSERT INTO requests (ts_ms, model, profile, status, latency_ms, tokens_in, tokens_out)
                VALUES (1, 'openai/gpt-4o', 'openai', 200, 42, 5, 7);",
            )
            .unwrap();
        }

        let store = Store::open("sqlite", Some(path.to_str().unwrap())).unwrap();
        {
            let conn = rusqlite::Connection::open(&path).unwrap();
            let has_worker = conn
                .prepare("PRAGMA table_info(requests)")
                .unwrap()
                .query_map([], |r| r.get::<_, String>(1))
                .unwrap()
                .filter_map(Result::ok)
                .any(|name| name == "worker");
            assert!(has_worker);
        }
        let usage = store.usage().unwrap();
        assert_eq!(usage.total_requests, 1);
        assert_eq!(usage.recent[0].model, "openai/gpt-4o");
        assert_eq!(usage.recent[0].worker, None);

        let mut outcome = sample_outcome();
        outcome.worker = Some("salted-worker".into());
        store.record(&outcome).unwrap();
        let usage = store.usage().unwrap();
        assert_eq!(usage.recent[0].worker.as_deref(), Some("salted-worker"));
        assert_eq!(usage.recent[1].worker, None);
    }

    #[test]
    fn worker_key_round_trips_through_each_store() {
        for kind in ["sqlite", "json"] {
            let dir = tempfile::tempdir().unwrap();
            let path = dir.path().join("worker");
            let store = Store::open(kind, Some(path.to_str().unwrap())).unwrap();
            let mut outcome = sample_outcome();
            outcome.worker = Some("salted-worker".into());
            store.record(&outcome).unwrap();

            assert_eq!(
                store.usage().unwrap().recent[0].worker.as_deref(),
                Some("salted-worker")
            );
            let filter = StatsFilter {
                since_ms: None,
                profile: None,
                model: None,
                routed_by_prefix: None,
                range_label: String::new(),
            };
            let read = store.filtered_outcomes(&filter).unwrap();
            assert_eq!(read[0].worker.as_deref(), Some("salted-worker"));
        }
    }

    #[test]
    fn old_database_without_worker_gains_worker_index_and_shape_table() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("old.db");
        {
            let conn = rusqlite::Connection::open(&path).unwrap();
            conn.execute_batch(
                "CREATE TABLE requests (
                    id INTEGER PRIMARY KEY AUTOINCREMENT,
                    ts_ms INTEGER NOT NULL,
                    model TEXT NOT NULL,
                    profile TEXT NOT NULL,
                    status INTEGER NOT NULL,
                    latency_ms INTEGER NOT NULL,
                    tokens_in INTEGER NOT NULL,
                    tokens_out INTEGER NOT NULL
                );",
            )
            .unwrap();
        }
        let _store = Store::open("sqlite", Some(path.to_str().unwrap())).unwrap();
        let conn = rusqlite::Connection::open(&path).unwrap();
        let count = |sql: &str| -> i64 { conn.query_row(sql, [], |r| r.get(0)).unwrap() };
        assert_eq!(
            count(
                "SELECT COUNT(*) FROM sqlite_master WHERE type = 'index' AND name = 'idx_requests_worker'"
            ),
            1
        );
        assert_eq!(
            count(
                "SELECT COUNT(*) FROM sqlite_master WHERE type = 'table' AND name = 'request_shapes'"
            ),
            1
        );
    }

    #[test]
    fn record_on_sqlite_returns_row_id() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("ids.db");
        let store = Store::open("sqlite", Some(path.to_str().unwrap())).unwrap();
        assert_eq!(store.record(&sample_outcome()).unwrap(), Some(1));
        assert_eq!(store.record(&sample_outcome()).unwrap(), Some(2));
    }

    #[test]
    fn record_on_json_store_returns_none() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("ids.json");
        let store = Store::open("json", Some(path.to_str().unwrap())).unwrap();
        assert_eq!(store.record(&sample_outcome()).unwrap(), None);
    }

    #[test]
    fn record_shape_then_read_round_trips_counts() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("shapes.db");
        let store = Store::open("sqlite", Some(path.to_str().unwrap())).unwrap();
        let id = store.record(&sample_outcome()).unwrap().unwrap();
        let shape = PromptShape {
            system_chars: 1200,
            messages: 7,
            turn_start: true,
            tool_count: 3,
            tool_bytes: 900,
            mcp_tool_count: 1,
            mcp_tool_bytes: 300,
            deferred_listing_chars: Some(450),
            skills_listing_chars: None,
            claude_md_chars: Some(2048),
            image_count: 2,
            project: Some("sample-project".into()),
            stream_key: Some("0123456789abcdef".into()),
            tool_result_bytes: std::collections::BTreeMap::from([("Read".into(), 480_u64)]),
            skill_body_chars: std::collections::BTreeMap::from([("pdf".into(), 800_u64)]),
        };
        store.record_shape(id, &shape).unwrap();
        let conn = rusqlite::Connection::open(&path).unwrap();
        let read = conn
            .query_row(
                "SELECT system_chars, messages, turn_start, tool_count, tool_bytes,
                        mcp_tool_count, mcp_tool_bytes, deferred_listing_chars,
                        skills_listing_chars, claude_md_chars, image_count, project, stream_key,
                        tool_result_bytes, skill_body_chars
                 FROM request_shapes WHERE request_id = ?1",
                [id],
                |r| {
                    Ok(PromptShape {
                        system_chars: db_u64(r.get(0)?),
                        messages: db_u64(r.get(1)?),
                        turn_start: r.get::<_, i64>(2)? == 1,
                        tool_count: db_u64(r.get(3)?),
                        tool_bytes: db_u64(r.get(4)?),
                        mcp_tool_count: db_u64(r.get(5)?),
                        mcp_tool_bytes: db_u64(r.get(6)?),
                        deferred_listing_chars: r.get::<_, Option<i64>>(7)?.map(db_u64),
                        skills_listing_chars: r.get::<_, Option<i64>>(8)?.map(db_u64),
                        claude_md_chars: r.get::<_, Option<i64>>(9)?.map(db_u64),
                        image_count: db_u64(r.get(10)?),
                        project: r.get(11)?,
                        stream_key: r.get(12)?,
                        tool_result_bytes: serde_json::from_str(&r.get::<_, String>(13)?).unwrap(),
                        skill_body_chars: serde_json::from_str(&r.get::<_, String>(14)?).unwrap(),
                    })
                },
            )
            .unwrap();
        assert_eq!(read, shape);
    }

    #[test]
    fn routed_by_round_trips_and_unrecorded_rows_read_none() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("routed.db");
        let store = Store::open("sqlite", Some(path.to_str().unwrap())).unwrap();
        let mut routed = sample_outcome();
        routed.routed_by = Some("smart-auto:balance".into());
        store.record(&routed).unwrap();
        store.record(&sample_outcome()).unwrap();
        let outcomes = store.filtered_outcomes(&StatsFilter::default()).unwrap();
        assert_eq!(outcomes[0].routed_by.as_deref(), Some("smart-auto:balance"));
        assert_eq!(outcomes[1].routed_by, None);
    }

    #[test]
    fn attempt_and_role_round_trip_on_sqlite() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("attempt.db");
        let store = Store::open("sqlite", Some(path.to_str().unwrap())).unwrap();
        let mut routed = sample_outcome();
        routed.attempt = Some("attempt-key".into());
        routed.role = Some("investigation".into());
        store.record(&routed).unwrap();
        store.record(&sample_outcome()).unwrap();
        let outcomes = store.filtered_outcomes(&StatsFilter::default()).unwrap();
        assert_eq!(outcomes[0].attempt.as_deref(), Some("attempt-key"));
        assert_eq!(outcomes[0].role.as_deref(), Some("investigation"));
        assert_eq!(outcomes[1].attempt, None);
        assert_eq!(outcomes[1].role, None);
    }

    #[test]
    fn role_check_rejects_values_outside_the_work_roles() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("role-check.db");
        let store = Store::open("sqlite", Some(path.to_str().unwrap())).unwrap();
        let mut bad = sample_outcome();
        bad.role = Some("janitor".into());
        assert!(store.record(&bad).is_err());
    }

    #[test]
    fn routed_by_prefix_keeps_only_matching_routes() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("prefix.db");
        let store = Store::open("sqlite", Some(path.to_str().unwrap())).unwrap();
        let mut smart = sample_outcome();
        smart.routed_by = Some("smart-auto:balance".into());
        let mut pinned = sample_outcome();
        pinned.routed_by = Some("pinned".into());
        store.record(&smart).unwrap();
        store.record(&pinned).unwrap();
        store.record(&sample_outcome()).unwrap();
        let filter = StatsFilter {
            routed_by_prefix: Some("smart-auto".into()),
            ..StatsFilter::default()
        };

        let outcomes = store.filtered_outcomes(&filter).unwrap();

        assert_eq!(outcomes.len(), 1);
        assert_eq!(outcomes[0].routed_by.as_deref(), Some("smart-auto:balance"));
    }

    #[test]
    fn sqlite_database_from_before_routed_by_gains_the_column_and_keeps_rows() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("before-routed.db");
        {
            let conn = rusqlite::Connection::open(&path).unwrap();
            conn.execute_batch(
                "CREATE TABLE requests (
                    id INTEGER PRIMARY KEY AUTOINCREMENT,
                    ts_ms INTEGER NOT NULL,
                    model TEXT NOT NULL,
                    profile TEXT NOT NULL,
                    status INTEGER NOT NULL,
                    latency_ms INTEGER NOT NULL,
                    tokens_in INTEGER NOT NULL,
                    tokens_out INTEGER NOT NULL,
                    ttft_ms INTEGER,
                    error TEXT,
                    client TEXT,
                    worker TEXT
                );
                INSERT INTO requests (ts_ms, model, profile, status, latency_ms, tokens_in, tokens_out)
                VALUES (1, 'openai/gpt-4o', 'openai', 200, 42, 5, 7);",
            )
            .unwrap();
        }
        let store = Store::open("sqlite", Some(path.to_str().unwrap())).unwrap();
        let outcomes = store.filtered_outcomes(&StatsFilter::default()).unwrap();
        assert_eq!(outcomes.len(), 1);
        assert_eq!(outcomes[0].routed_by, None);
    }

    #[test]
    fn request_shapes_table_older_than_stream_key_gains_the_column() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("old-shapes.db");
        {
            let conn = rusqlite::Connection::open(&path).unwrap();
            conn.execute_batch(
                "CREATE TABLE requests (
                    id INTEGER PRIMARY KEY AUTOINCREMENT, ts_ms INTEGER NOT NULL,
                    model TEXT NOT NULL, profile TEXT NOT NULL, status INTEGER NOT NULL,
                    latency_ms INTEGER NOT NULL, tokens_in INTEGER NOT NULL,
                    tokens_out INTEGER NOT NULL, ttft_ms INTEGER,
                    cache_read_tokens INTEGER NOT NULL DEFAULT 0,
                    cache_write_tokens INTEGER NOT NULL DEFAULT 0, worker TEXT
                 );
                 CREATE TABLE request_shapes (
                    request_id INTEGER PRIMARY KEY, system_chars INTEGER NOT NULL,
                    messages INTEGER NOT NULL, turn_start INTEGER NOT NULL,
                    tool_count INTEGER NOT NULL, tool_bytes INTEGER NOT NULL,
                    mcp_tool_count INTEGER NOT NULL, mcp_tool_bytes INTEGER NOT NULL,
                    deferred_listing_chars INTEGER, skills_listing_chars INTEGER,
                    claude_md_chars INTEGER, image_count INTEGER NOT NULL, project TEXT
                 );",
            )
            .unwrap();
        }
        let _store = Store::open("sqlite", Some(path.to_str().unwrap())).unwrap();
        let conn = rusqlite::Connection::open(&path).unwrap();
        let has_stream_key = conn
            .prepare("PRAGMA table_info(request_shapes)")
            .unwrap()
            .query_map([], |r| r.get::<_, String>(1))
            .unwrap()
            .filter_map(Result::ok)
            .any(|name| name == "stream_key");
        assert!(has_stream_key);
    }

    #[test]
    fn sqlite_errors_keep_the_failure_reason() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("t.db");
        let store = Store::open("sqlite", Some(path.to_str().unwrap())).unwrap();
        let mut failed = sample_outcome();
        failed.status = 400;
        failed.error = Some("exceeds the available context size".into());
        store.record(&failed).unwrap();
        let (total, recent) = store.errors(None).unwrap();
        assert_eq!(total, 1);
        assert_eq!(
            recent[0].row.error.as_deref(),
            Some("exceeds the available context size")
        );
    }

    #[test]
    fn two_haiku_requests_one_above_threshold_cost_is_sum_of_each_request() {
        use crate::configs::{LongContextTier, PriceCard, TokenRates};
        use std::collections::HashSet;
        use std::sync::Arc;

        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("t.db");
        let store = Store::open("sqlite", Some(path.to_str().unwrap())).unwrap();
        for tokens_in in [50_000, 150_000] {
            let mut outcome = sample_outcome();
            outcome.profile = "copilot".into();
            outcome.model_key = "copilot/demo-long-model".into();
            outcome.tokens_in = tokens_in;
            outcome.tokens_out = 0;
            outcome.cache_read_tokens = 0;
            outcome.cache_write_tokens = 0;
            store.record(&outcome).unwrap();
        }
        let card = PriceCard {
            base: TokenRates {
                input_per_1m: 0.1,
                ..TokenRates::default()
            },
            long_context: Some(LongContextTier {
                threshold_tokens: 100_000,
                rates: TokenRates {
                    input_per_1m: 0.5,
                    ..TokenRates::default()
                },
            }),
        };
        let book = PricingBook {
            copilot_profiles: HashSet::from(["copilot".to_string()]),
            copilot_published: Arc::new(HashMap::from([("demo-long-model".to_string(), card)])),
            ..PricingBook::default()
        };
        let snap = store
            .stats(
                &StatsFilter {
                    since_ms: None,
                    profile: None,
                    model: None,
                    routed_by_prefix: None,
                    range_label: "all".into(),
                },
                &book,
            )
            .unwrap();
        let expected = (50_000.0 * 0.1 + 150_000.0 * 0.5) / 1_000_000.0;
        assert!((snap.overview.cost_usd - expected).abs() < 1e-12);
    }

    #[test]
    fn sqlite_stats_filter_by_profile() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("t.db");
        let store = Store::open("sqlite", Some(path.to_str().unwrap())).unwrap();
        store.record(&sample_outcome()).unwrap();
        let mut other = sample_outcome();
        other.profile = "anthropic".into();
        other.model_key = "anthropic/claude".into();
        store.record(&other).unwrap();
        let snap = store
            .stats(
                &StatsFilter {
                    since_ms: None,
                    profile: Some("openai".into()),
                    model: None,
                    routed_by_prefix: None,
                    range_label: "all".into(),
                },
                &PricingBook::default(),
            )
            .unwrap();
        assert_eq!(snap.overview.requests, 1);
        assert_eq!(snap.profiles[0].key, "openai");
        assert!(snap.overview.tokens_per_sec_avg > 0.0);
    }

    #[test]
    fn sqlite_errors_only_status_400_plus() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("t.db");
        let store = Store::open("sqlite", Some(path.to_str().unwrap())).unwrap();
        store.record(&sample_outcome()).unwrap();
        let mut failed = sample_outcome();
        failed.status = 502;
        failed.model_key = "openai/fail".into();
        store.record(&failed).unwrap();
        let (total, rows) = store.errors(None).unwrap();
        assert_eq!(total, 1);
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].row.status, 502);
        assert_eq!(rows[0].row.model, "openai/fail");
        assert_eq!(rows[0].resolved_ms, None);
    }

    #[test]
    fn sqlite_errors_mark_solved_and_reopen() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("t.db");
        let store = Store::open("sqlite", Some(path.to_str().unwrap())).unwrap();
        store.record(&sample_outcome()).unwrap();
        let mut failed = sample_outcome();
        failed.status = 502;
        store.record(&failed).unwrap();
        let (_, rows) = store.errors(None).unwrap();
        let id = rows[0].id.expect("sqlite rows carry ids");

        assert_eq!(
            store
                .set_errors_resolved(&[id], Some(1_700_000_000_000))
                .unwrap(),
            1
        );
        let (_, rows) = store.errors(None).unwrap();
        assert_eq!(rows[0].resolved_ms, Some(1_700_000_000_000));

        // A successful request id is not an error, so it cannot be marked.
        let ok_id: i64 = {
            let Store::Sqlite(conn) = &store else {
                unreachable!()
            };
            conn.lock()
                .unwrap()
                .query_row(
                    "SELECT id FROM requests WHERE status < 400 LIMIT 1",
                    [],
                    |r| r.get(0),
                )
                .unwrap()
        };
        assert_eq!(store.set_errors_resolved(&[ok_id], Some(1)).unwrap(), 0);

        assert_eq!(store.set_errors_resolved(&[id], None).unwrap(), 1);
        let (_, rows) = store.errors(None).unwrap();
        assert_eq!(rows[0].resolved_ms, None);
    }

    #[test]
    fn sqlite_recurring_error_reopens_after_solve() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("t.db");
        let store = Store::open("sqlite", Some(path.to_str().unwrap())).unwrap();
        let mut failed = sample_outcome();
        failed.status = 502;
        failed.error = Some("boom".into());
        store.record(&failed).unwrap();
        let (_, rows) = store.errors(None).unwrap();
        let first = rows[0].id.expect("sqlite rows carry ids");
        store.set_errors_resolved(&[first], Some(now_ms())).unwrap();

        // The same reason recurs after the solve: the newer row is open, the old one stays solved.
        std::thread::sleep(std::time::Duration::from_millis(5));
        store.record(&failed).unwrap();
        let (_, rows) = store.errors(None).unwrap();
        assert_eq!(rows.len(), 2);
        assert_eq!(rows[0].resolved_ms, None, "newer occurrence must be open");
        assert!(
            rows[1].resolved_ms.is_some(),
            "older occurrence stays solved"
        );
        assert_eq!(store.solved_error_count(None).unwrap(), 1);

        // Solving the newer occurrence covers the whole reason again.
        let second = rows[0].id.expect("sqlite rows carry ids");
        store
            .set_errors_resolved(&[second], Some(now_ms()))
            .unwrap();
        let (_, rows) = store.errors(None).unwrap();
        assert!(rows.iter().all(|r| r.resolved_ms.is_some()));
        assert_eq!(store.solved_error_count(None).unwrap(), 2);
    }

    #[test]
    fn sqlite_api_keys_crud() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("t.db");
        let store = Store::open("sqlite", Some(path.to_str().unwrap())).unwrap();
        let record = ApiKeyRecord {
            name: "ci".into(),
            key_hash: hash_key("sk-hub-abc"),
            masked: "sk-h...-abc".into(),
            enabled: true,
            created_ms: now_ms(),
        };
        store.insert_api_key(&record).unwrap();
        assert!(store.insert_api_key(&record).is_err());
        assert_eq!(store.list_api_keys().unwrap().len(), 1);
        store.delete_api_key("ci").unwrap();
        assert!(store.list_api_keys().unwrap().is_empty());
    }

    #[test]
    fn json_store_persists_across_reopen() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("s.json");
        let path_str = path.to_str().unwrap();
        {
            let store = Store::open("json", Some(path_str)).unwrap();
            store.record(&sample_outcome()).unwrap();
        }
        let store = Store::open("json", Some(path_str)).unwrap();
        let usage = store.usage().unwrap();
        assert_eq!(usage.total_requests, 1);
        assert_eq!(usage.recent[0].cache_write_tokens, 1);
    }

    fn catalog_round_trip(kind: &str, file: &str) {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join(file);
        let path_str = path.to_str().unwrap();
        {
            let store = Store::open(kind, Some(path_str)).unwrap();
            store
                .save_model_catalog("copilot", &[serde_json::json!({"id": "old"})])
                .unwrap();
            store
                .save_model_catalog("copilot", &[serde_json::json!({"id": "claude-opus-5.5"})])
                .unwrap();
        }
        let store = Store::open(kind, Some(path_str)).unwrap();
        let catalogs = store.load_model_catalogs().unwrap();
        assert_eq!(catalogs.len(), 1);
        assert_eq!(
            catalogs["copilot"],
            [serde_json::json!({"id": "claude-opus-5.5"})]
        );
    }

    #[test]
    fn sqlite_model_catalog_survives_reopen_and_overwrites() {
        catalog_round_trip("sqlite", "t.db");
    }

    #[test]
    fn json_model_catalog_survives_reopen_and_overwrites() {
        catalog_round_trip("json", "s.json");
    }

    #[test]
    fn json_store_without_catalogs_still_loads() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("s.json");
        std::fs::write(&path, r#"{"totals":{"requests":3,"errors":0,"tokens_in":0,"tokens_out":0,"cache_read_tokens":0,"cache_write_tokens":0},"recent":[],"api_keys":[]}"#).unwrap();
        let store = Store::open("json", Some(path.to_str().unwrap())).unwrap();
        assert_eq!(store.usage().unwrap().total_requests, 3);
        assert!(store.load_model_catalogs().unwrap().is_empty());
    }

    fn limits_seed_without_replacing_learned(kind: &str, file: &str) {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join(file);
        let path_str = path.to_str().unwrap();
        let learned = ModelLimit {
            limit: 150_000,
            source: LimitSource::Learned,
            learned_at: 5,
        };
        {
            let store = Store::open(kind, Some(path_str)).unwrap();
            store
                .seed_model_limits("copilot", &[("a", 100), ("b", 200)], 1)
                .unwrap();
            store.save_model_limit("copilot", "b", &learned).unwrap();
        }
        let store = Store::open(kind, Some(path_str)).unwrap();
        store
            .seed_model_limits("copilot", &[("a", 300), ("b", 400)], 2)
            .unwrap();
        let limits = &store.load_model_limits().unwrap()["copilot"];
        assert_eq!(limits["b"], learned);
        assert_eq!(limits["a"].limit, 300);
        assert_eq!(limits["a"].source, LimitSource::Seeded);
        assert_eq!(limits["a"].learned_at, 2);
    }

    #[test]
    fn sqlite_seeded_limits_never_replace_learned_ones() {
        limits_seed_without_replacing_learned("sqlite", "t.db");
    }

    #[test]
    fn json_seeded_limits_never_replace_learned_ones() {
        limits_seed_without_replacing_learned("json", "s.json");
    }

    fn body_fields_round_trip(kind: &str, file: &str) {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join(file);
        let path_str = path.to_str().unwrap();
        {
            let store = Store::open(kind, Some(path_str)).unwrap();
            store
                .save_unsupported_body_field("copilot", "context_management", 1)
                .unwrap();
            store
                .save_unsupported_body_field("copilot", "messages.*.output_config", 2)
                .unwrap();
            store
                .save_unsupported_body_field("copilot", "context_management", 3)
                .unwrap();
        }
        let store = Store::open(kind, Some(path_str)).unwrap();
        let fields = store.load_unsupported_body_fields().unwrap();
        assert_eq!(fields.len(), 1);
        assert_eq!(
            fields["copilot"],
            ["context_management", "messages.*.output_config"]
        );
    }

    #[test]
    fn sqlite_unsupported_body_fields_survive_reopen_once_each() {
        body_fields_round_trip("sqlite", "t.db");
    }

    #[test]
    fn json_unsupported_body_fields_survive_reopen_once_each() {
        body_fields_round_trip("json", "s.json");
    }

    fn vision_refusals_round_trip(kind: &str, file: &str) {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join(file);
        let path_str = path.to_str().unwrap();
        {
            let store = Store::open(kind, Some(path_str)).unwrap();
            store.save_vision_refusal("copilot", "gpt-4o", 1).unwrap();
            store.save_vision_refusal("copilot", "gpt-4o", 2).unwrap();
            store.save_vision_refusal("other", "mini", 3).unwrap();
        }
        let store = Store::open(kind, Some(path_str)).unwrap();
        let mut refusals = store.load_vision_refusals().unwrap();
        refusals.sort();
        assert_eq!(
            refusals,
            [
                ("copilot".to_string(), "gpt-4o".to_string()),
                ("other".to_string(), "mini".to_string()),
            ]
        );
    }

    #[test]
    fn sqlite_image_refusals_survive_reopen() {
        vision_refusals_round_trip("sqlite", "t.db");
    }

    #[test]
    fn json_image_refusals_survive_reopen() {
        vision_refusals_round_trip("json", "s.json");
    }

    #[test]
    fn unknown_store_kind_errors() {
        assert!(Store::open("postgres", None).is_err());
    }

    fn selection_event(seq: u64, ts_ms: u64) -> SelectionEvent {
        use crate::schemas::selection_stage::SelectionStage;
        use crate::schemas::selection_state::SelectionState;

        SelectionEvent {
            seq,
            ts_ms,
            ..SelectionEvent::new(
                SelectionStage::Solver,
                SelectionState::Selected,
                "copilot/m",
            )
        }
    }

    fn seqs(events: &[SelectionEvent]) -> Vec<u64> {
        events.iter().map(|event| event.seq).collect()
    }

    fn selection_events_keep_the_newest_by_count(kind: &str, file: &str) {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join(file);
        let path_str = path.to_str().unwrap();
        let now = 10 * MS_PER_DAY;
        let events: Vec<SelectionEvent> = (1..=5).map(|seq| selection_event(seq, now)).collect();
        {
            let store = Store::open(kind, Some(path_str)).unwrap();
            store.save_selection_events(&events[..3], now, 3).unwrap();
            store.save_selection_events(&events[3..], now, 3).unwrap();
        }
        let store = Store::open(kind, Some(path_str)).unwrap();

        assert_eq!(seqs(&store.load_selection_events(10).unwrap()), [3, 4, 5]);
        assert_eq!(seqs(&store.load_selection_events(2).unwrap()), [4, 5]);
    }

    fn selection_events_since_filter_by_time_and_seq(kind: &str, file: &str) {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join(file);
        let store = Store::open(kind, Some(path.to_str().unwrap())).unwrap();
        let now = 10 * MS_PER_DAY;
        let events: Vec<SelectionEvent> = (1..=5).map(|seq| selection_event(seq, now)).collect();
        store.save_selection_events(&events, now, -1).unwrap();

        assert_eq!(
            seqs(&store.load_selection_events_since(0, 0, 10).unwrap()),
            [1, 2, 3, 4, 5]
        );
        assert_eq!(
            seqs(&store.load_selection_events_since(0, 3, 10).unwrap()),
            [4, 5]
        );
        assert_eq!(
            seqs(&store.load_selection_events_since(0, 0, 2).unwrap()),
            [1, 2]
        );
        assert!(
            store
                .load_selection_events_since(now + 1, 0, 10)
                .unwrap()
                .is_empty()
        );
    }

    #[test]
    fn sqlite_selection_events_since_filter_by_time_and_seq() {
        selection_events_since_filter_by_time_and_seq("sqlite", "t.db");
    }

    #[test]
    fn json_selection_events_since_filter_by_time_and_seq() {
        selection_events_since_filter_by_time_and_seq("json", "s.json");
    }

    #[test]
    fn sqlite_selection_events_keep_the_newest_by_count() {
        selection_events_keep_the_newest_by_count("sqlite", "t.db");
    }

    #[test]
    fn json_selection_events_keep_the_newest_by_count() {
        selection_events_keep_the_newest_by_count("json", "s.json");
    }

    fn selection_events_expire_by_age(kind: &str, file: &str) {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join(file);
        let store = Store::open(kind, Some(path.to_str().unwrap())).unwrap();
        let now = 100 * MS_PER_DAY;
        let expired = now - SELECTION_RETENTION_MS - 1;

        store
            .save_selection_events(
                &[selection_event(1, expired), selection_event(2, now)],
                now,
                SELECTION_RETENTION_EVENTS,
            )
            .unwrap();

        assert_eq!(seqs(&store.load_selection_events(10).unwrap()), [2]);
    }

    #[test]
    fn sqlite_selection_events_expire_by_age() {
        selection_events_expire_by_age("sqlite", "t.db");
    }

    #[test]
    fn sqlite_selection_text_round_trips_prompt_then_answer() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("t.db");
        let store = Store::open("sqlite", Some(path.to_str().unwrap())).unwrap();
        let now = 100 * MS_PER_DAY;

        store
            .save_selection_prompt("att-1", now, "question")
            .unwrap();
        assert_eq!(
            store.load_selection_text("att-1").unwrap(),
            Some(("question".to_owned(), String::new()))
        );

        store.save_selection_answer("att-1", now, "reply").unwrap();
        assert_eq!(
            store.load_selection_text("att-1").unwrap(),
            Some(("question".to_owned(), "reply".to_owned()))
        );
        assert_eq!(store.load_selection_text("missing").unwrap(), None);
    }

    #[test]
    fn sqlite_selection_text_expires_by_age() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("t.db");
        let store = Store::open("sqlite", Some(path.to_str().unwrap())).unwrap();
        let now = 100 * MS_PER_DAY;
        let expired = now - SELECTION_RETENTION_MS - 1;

        store
            .save_selection_prompt("old", expired, "stale")
            .unwrap();
        store.save_selection_prompt("new", now, "fresh").unwrap();

        assert_eq!(store.load_selection_text("old").unwrap(), None);
        assert!(store.load_selection_text("new").unwrap().is_some());
    }

    #[test]
    fn json_store_refuses_selection_text() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("s.json");
        let store = Store::open("json", Some(path.to_str().unwrap())).unwrap();

        assert!(store.save_selection_prompt("a", 1, "x").is_err());
        assert!(store.load_selection_text("a").is_err());
    }

    #[test]
    fn json_selection_events_expire_by_age() {
        selection_events_expire_by_age("json", "s.json");
    }

    #[test]
    fn sqlite_database_from_before_selection_events_gains_the_tables() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("old.db");
        {
            let conn = rusqlite::Connection::open(&path).unwrap();
            conn.execute_batch(
                "CREATE TABLE requests (
                    id INTEGER PRIMARY KEY AUTOINCREMENT,
                    ts_ms INTEGER NOT NULL,
                    model TEXT NOT NULL,
                    profile TEXT NOT NULL,
                    status INTEGER NOT NULL,
                    latency_ms INTEGER NOT NULL,
                    tokens_in INTEGER NOT NULL,
                    tokens_out INTEGER NOT NULL,
                    ttft_ms INTEGER
                );",
            )
            .unwrap();
        }

        let store = Store::open("sqlite", Some(path.to_str().unwrap())).unwrap();
        assert!(store.load_selection_events(10).unwrap().is_empty());
        store
            .save_selection_events(&[selection_event(1, 5)], 5, 10)
            .unwrap();

        assert_eq!(seqs(&store.load_selection_events(10).unwrap()), [1]);
    }

    #[test]
    fn json_store_without_selection_fields_still_loads() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("old.json");
        std::fs::write(
            &path,
            r#"{"totals":{"requests":0,"errors":0,"tokens_in":0,"tokens_out":0,"cache_read_tokens":0,"cache_write_tokens":0},"recent":[],"api_keys":[]}"#,
        )
        .unwrap();
        let store = Store::open("json", Some(path.to_str().unwrap())).unwrap();

        assert!(store.load_selection_events(10).unwrap().is_empty());
        assert_eq!(store.install_salt("fresh").unwrap(), "fresh");
    }

    fn install_salt_survives_reopen(kind: &str, file: &str) {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join(file);
        let path_str = path.to_str().unwrap();
        {
            let store = Store::open(kind, Some(path_str)).unwrap();
            assert_eq!(store.install_salt("first").unwrap(), "first");
        }
        let store = Store::open(kind, Some(path_str)).unwrap();

        assert_eq!(store.install_salt("second").unwrap(), "first");
    }

    #[test]
    fn sqlite_install_salt_is_created_once() {
        install_salt_survives_reopen("sqlite", "t.db");
    }

    #[test]
    fn json_install_salt_is_created_once() {
        install_salt_survives_reopen("json", "s.json");
    }
}

#[cfg(test)]
#[allow(clippy::float_cmp, reason = "fixture costs are exact small integers")]
mod tier_role_tests {
    use super::*;
    use crate::configs::TokenRates;
    use crate::schemas::selection_stage::SelectionStage;
    use crate::schemas::selection_state::SelectionState;

    fn request(
        routed_by: Option<&str>,
        role: Option<&str>,
        attempt: Option<&str>,
    ) -> RequestOutcome {
        RequestOutcome {
            profile: "acme".into(),
            model_key: "acme/m".into(),
            status: 200,
            latency_ms: 1,
            ttft_ms: None,
            tokens_in: 1_000_000,
            tokens_out: 0,
            cache_read_tokens: 0,
            cache_write_tokens: 0,
            ts_ms: 0,
            error: None,
            client: None,
            worker: None,
            routed_by: routed_by.map(Into::into),
            attempt: attempt.map(Into::into),
            role: role.map(Into::into),
        }
    }

    /// Each request costs $1 in this test, whatever its tokens.
    fn flat_dollar(_: &RequestOutcome) -> CostParts {
        CostParts {
            input: 1.0,
            ..CostParts::default()
        }
    }

    fn set(items: &[&str]) -> HashSet<String> {
        items.iter().map(|item| (*item).to_string()).collect()
    }

    #[test]
    fn groups_smart_auto_rows_by_tier_and_role_and_skips_other_routes() {
        let requests = [
            request(
                Some("smart-auto:balance"),
                Some("implementation"),
                Some("a1"),
            ),
            request(Some("smart-auto:balance"), None, Some("a2")),
            request(
                Some("smart-auto:balance"),
                Some("implementation"),
                Some("a3"),
            ),
            request(Some("smart-auto:intelligence"), Some("planning"), None),
            request(Some("pinned"), Some("implementation"), Some("a4")),
            request(Some("smart-auto:"), None, None),
            request(None, None, None),
        ];
        let rows = group_tier_role(&requests, flat_dollar, &set(&["a1"]), &HashMap::new());

        let keys: Vec<(&str, &str)> = rows
            .iter()
            .map(|row| (row.tier.as_str(), row.role.as_str()))
            .collect();
        assert_eq!(
            keys,
            [
                ("balance", "implementation"),
                ("balance", "unknown"),
                ("intelligence", "planning"),
            ]
        );
        assert_eq!(rows[0].requests, 2);
        assert_eq!(rows[0].cost_usd, 2.0);
        assert_eq!(rows[0].completed_attempts, 1);
        assert_eq!(rows[0].cost_per_completed_usd, Some(2.0));
        assert_eq!(rows[1].completed_attempts, 0);
        assert_eq!(rows[1].cost_per_completed_usd, None);
        assert_eq!(rows[2].requests, 1);
        assert_eq!(rows[2].completed_attempts, 0);
    }

    #[test]
    fn outcomes_count_each_attempt_once_per_group() {
        let requests = [
            request(Some("smart-auto:balance"), Some("validation"), Some("a1")),
            request(Some("smart-auto:balance"), Some("validation"), Some("a1")),
            request(Some("smart-auto:balance"), Some("validation"), Some("a2")),
            request(Some("smart-auto:balance"), Some("validation"), Some("a3")),
        ];
        let verdicts = HashMap::from([
            ("a1".to_string(), "tests_passed".to_string()),
            ("a2".to_string(), "tests_failed".to_string()),
            ("a9".to_string(), "review_passed".to_string()),
        ]);
        let rows = group_tier_role(&requests, flat_dollar, &set(&["a1", "a2", "a9"]), &verdicts);

        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].requests, 4);
        assert_eq!(rows[0].cost_usd, 4.0);
        assert_eq!(rows[0].completed_attempts, 2);
        assert_eq!(rows[0].cost_per_completed_usd, Some(2.0));
        assert_eq!(rows[0].outcomes.get("tests_passed"), Some(&1));
        assert_eq!(rows[0].outcomes.get("tests_failed"), Some(&1));
        assert_eq!(rows[0].outcomes.get("review_passed"), None);
    }

    #[test]
    fn group_row_serializes_null_cost_per_completed_without_completions() {
        let rows = group_tier_role(
            &[request(Some("smart-auto:balance"), None, None)],
            flat_dollar,
            &HashSet::new(),
            &HashMap::new(),
        );
        let json = serde_json::to_value(&rows).unwrap();
        assert!(json[0]["cost_per_completed_usd"].is_null());
        assert_eq!(json[0]["role"], "unknown");
        assert_eq!(json[0]["outcomes"], serde_json::json!({}));
    }

    #[test]
    fn attempt_outcome_precedence_keeps_the_strongest_source() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("outcomes.db");
        let store = Store::open("sqlite", Some(path.to_str().unwrap())).unwrap();
        let outcome = |code, source| AttemptOutcome { code, source };

        store
            .save_attempt_outcome(
                "a1",
                Some("w1"),
                &outcome(OutcomeCode::TestsFailed, OutcomeSource::Heuristic),
                1,
            )
            .unwrap();
        store
            .save_attempt_outcome(
                "a1",
                None,
                &outcome(OutcomeCode::TestsPassed, OutcomeSource::Heuristic),
                2,
            )
            .unwrap();
        store
            .save_attempt_outcome(
                "a1",
                None,
                &outcome(OutcomeCode::UserCorrected, OutcomeSource::Client),
                3,
            )
            .unwrap();
        store
            .save_attempt_outcome(
                "a1",
                None,
                &outcome(OutcomeCode::ReviewPassed, OutcomeSource::Review),
                4,
            )
            .unwrap();

        let loaded = store
            .load_attempt_outcomes(&["a1".to_string(), "missing".to_string()])
            .unwrap();
        assert_eq!(loaded.len(), 1);
        assert_eq!(
            loaded["a1"],
            outcome(OutcomeCode::UserCorrected, OutcomeSource::Client)
        );
        let Store::Sqlite(conn) = &store else {
            panic!("expected sqlite store");
        };
        let worker: Option<String> = conn
            .lock()
            .unwrap()
            .query_row(
                "SELECT worker FROM attempt_outcomes WHERE attempt = 'a1'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(worker.as_deref(), Some("w1"));
    }

    #[test]
    fn attempt_outcome_check_rejects_unknown_codes() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("outcomes-check.db");
        let store = Store::open("sqlite", Some(path.to_str().unwrap())).unwrap();
        let Store::Sqlite(conn) = &store else {
            panic!("expected sqlite store");
        };
        let inserted = conn.lock().unwrap().execute(
            "INSERT INTO attempt_outcomes (attempt, code, source, ts_ms) VALUES ('a', 'nope', 'client', 1)",
            [],
        );
        assert!(inserted.is_err());
    }

    #[test]
    fn json_store_keeps_no_attempt_outcomes() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("store.json");
        let store = Store::open("json", Some(path.to_str().unwrap())).unwrap();
        let outcome = AttemptOutcome {
            code: OutcomeCode::TestsPassed,
            source: OutcomeSource::Heuristic,
        };
        store.save_attempt_outcome("a1", None, &outcome, 1).unwrap();
        assert!(
            store
                .load_attempt_outcomes(&["a1".to_string()])
                .unwrap()
                .is_empty()
        );
    }

    #[test]
    fn sqlite_by_tier_role_joins_completed_solver_attempts_and_verdicts() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("tier.db");
        let store = Store::open("sqlite", Some(path.to_str().unwrap())).unwrap();
        let Store::Sqlite(conn) = &store else {
            panic!("expected sqlite store");
        };
        conn.lock()
            .unwrap()
            .execute_batch(
                "CREATE TABLE IF NOT EXISTS attempt_outcomes (
                    attempt TEXT PRIMARY KEY,
                    worker TEXT,
                    code TEXT NOT NULL,
                    source TEXT NOT NULL,
                    ts_ms INTEGER NOT NULL
                 );
                 INSERT INTO attempt_outcomes (attempt, worker, code, source, ts_ms)
                 VALUES ('a1', NULL, 'tests_passed', 'heuristic', 1),
                        ('a2', NULL, 'tests_failed', 'heuristic', 1);",
            )
            .unwrap();

        let book = PricingBook {
            global: TokenRates {
                input_per_1m: 2.0,
                ..TokenRates::default()
            },
            ..PricingBook::default()
        };
        for (routed_by, role, attempt) in [
            ("smart-auto:balance", Some("implementation"), "a1"),
            ("smart-auto:balance", Some("implementation"), "a4"),
            ("smart-auto:balance", None, "a2"),
            ("pinned", Some("implementation"), "a5"),
        ] {
            let mut row = request(Some(routed_by), role, Some(attempt));
            row.ts_ms = now_ms();
            store.record(&row).unwrap();
        }

        let now = now_ms();
        let event = |seq: u64, attempt: &str, stage, state| SelectionEvent {
            seq,
            ts_ms: now,
            attempt: Some(attempt.into()),
            ..SelectionEvent::new(stage, state, "acme/m")
        };
        store
            .save_selection_events(
                &[
                    event(1, "a1", SelectionStage::Solver, SelectionState::Completed),
                    event(2, "a2", SelectionStage::Solver, SelectionState::Completed),
                    event(3, "a4", SelectionStage::Solver, SelectionState::Selected),
                    event(4, "a4", SelectionStage::Judge, SelectionState::Completed),
                ],
                now,
                -1,
            )
            .unwrap();

        let filter = StatsFilter {
            range_label: "all".into(),
            ..StatsFilter::default()
        };
        let rows = store.by_tier_role(&filter, &book).unwrap();

        assert_eq!(rows.len(), 2);
        assert_eq!(rows[0].tier, "balance");
        assert_eq!(rows[0].role, "implementation");
        assert_eq!(rows[0].requests, 2);
        assert!((rows[0].cost_usd - 4.0).abs() < 1e-9);
        assert_eq!(rows[0].completed_attempts, 1);
        assert_eq!(rows[0].outcomes.get("tests_passed"), Some(&1));
        assert!((rows[0].cost_per_completed_usd.unwrap() - 4.0).abs() < 1e-9);
        assert_eq!(rows[1].role, "unknown");
        assert_eq!(rows[1].completed_attempts, 1);
        assert_eq!(rows[1].outcomes.get("tests_failed"), Some(&1));

        let narrowed = store
            .by_tier_role(
                &StatsFilter {
                    since_ms: Some(now + 1),
                    ..filter
                },
                &book,
            )
            .unwrap();
        assert!(narrowed.is_empty());
    }

    #[test]
    fn json_store_has_no_tier_role_rows() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("tier.json");
        let store = Store::open("json", Some(path.to_str().unwrap())).unwrap();
        let mut row = request(
            Some("smart-auto:balance"),
            Some("implementation"),
            Some("a1"),
        );
        row.ts_ms = now_ms();
        store.record(&row).unwrap();

        let rows = store
            .by_tier_role(&StatsFilter::default(), &PricingBook::default())
            .unwrap();
        assert!(rows.is_empty());
    }
}
