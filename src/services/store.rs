use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Mutex;
use std::time::{SystemTime, UNIX_EPOCH};

use crate::schemas::api_key_record::ApiKeyRecord;
use crate::schemas::usage_report::UsageReport;
use crate::schemas::usage_row::UsageRow;
use rusqlite::Connection;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use sha2::{Digest, Sha256};

use crate::consts::ERROR_STATUS_MIN;
use crate::services::model_limits::{LimitSource, ModelLimit};
use crate::services::stats::{RequestOutcome, StatsSnapshot, snapshot_from_outcomes};

const DEFAULT_SQLITE_PATH: &str = "llm-hub.db";
const DEFAULT_JSON_PATH: &str = "llm-hub-stats.json";
const USAGE_RECENT_LIMIT: usize = 200;

/// Durable store behind `LLM_HUB_PERSISTENT=true`. Sqlite keeps a full request
/// log; Json keeps a rolling recent window flushed on every write batch.
#[derive(Debug)]
pub enum Store {
    Sqlite(Mutex<Connection>),
    Json(Mutex<JsonStore>),
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
    pub range_label: String,
}

impl Store {
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
                cache_write_tokens INTEGER NOT NULL DEFAULT 0
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
            );",
        )
        .map_err(|e| format!("sqlite schema init failed: {e}"))?;
        migrate_requests_columns(&conn)?;
        Ok(Store::Sqlite(Mutex::new(conn)))
    }

    fn open_json(path: &str) -> Store {
        let path_buf = PathBuf::from(path);
        let state = match std::fs::read(&path_buf) {
            Ok(bytes) => serde_json::from_slice(&bytes).unwrap_or_default(),
            Err(_) => JsonState::default(),
        };
        Store::Json(Mutex::new(JsonStore {
            path: path_buf,
            state,
        }))
    }

    pub fn record(&self, outcome: &RequestOutcome) -> Result<(), String> {
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
        };
        match self {
            Store::Sqlite(conn) => {
                let guard = conn.lock().map_err(|_| "sqlite lock poisoned")?;
                guard
                    .execute(
                        "INSERT INTO requests (
                            ts_ms, model, profile, status, latency_ms, tokens_in, tokens_out,
                            ttft_ms, cache_read_tokens, cache_write_tokens, error, client
                         ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12)",
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
                            row.client
                        ],
                    )
                    .map_err(|e| e.to_string())?;
                Ok(())
            }
            Store::Json(store) => {
                let mut guard = store.lock().map_err(|_| "json store lock poisoned")?;
                guard.state.totals.requests += 1;
                if row.status >= ERROR_STATUS_MIN {
                    guard.state.totals.errors += 1;
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
                guard.flush()
            }
        }
    }

    pub fn usage(&self) -> Result<UsageReport, String> {
        match self {
            Store::Sqlite(conn) => {
                let guard = conn.lock().map_err(|_| "sqlite lock poisoned")?;
                let (total_requests, total_errors, total_tokens_in, total_tokens_out) = guard
                    .query_row(
                        "SELECT COUNT(*),
                                COALESCE(SUM(CASE WHEN status >= 400 THEN 1 ELSE 0 END), 0),
                                COALESCE(SUM(tokens_in), 0), COALESCE(SUM(tokens_out), 0)
                         FROM requests",
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
                    .prepare(
                        "SELECT ts_ms, model, profile, status, latency_ms, tokens_in, tokens_out,
                                ttft_ms, cache_read_tokens, cache_write_tokens, error, client
                         FROM requests ORDER BY id DESC LIMIT ?1",
                    )
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
                    recent: guard.state.recent.iter().rev().cloned().collect(),
                })
            }
        }
    }

    /// Failed requests (`status >= 400`) newest first, optional `since_ms` window.
    pub fn errors(&self, since_ms: Option<u64>) -> Result<(u64, Vec<UsageRow>), String> {
        match self {
            Store::Sqlite(conn) => {
                let guard = conn.lock().map_err(|_| "sqlite lock poisoned")?;
                let mut count_sql =
                    format!("SELECT COUNT(*) FROM requests WHERE status >= {ERROR_STATUS_MIN}");
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
                            ttft_ms, cache_read_tokens, cache_write_tokens, error, client
                     FROM requests WHERE status >= {ERROR_STATUS_MIN}"
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
                    .query_map(rusqlite::params_from_iter(list_params), map_usage_row)
                    .map_err(|e| e.to_string())?
                    .filter_map(Result::ok)
                    .collect();
                Ok((total_errors, recent))
            }
            Store::Json(store) => {
                let guard = store.lock().map_err(|_| "json store lock poisoned")?;
                let recent: Vec<UsageRow> = guard
                    .state
                    .recent
                    .iter()
                    .rev()
                    .filter(|row| {
                        row.status >= ERROR_STATUS_MIN
                            && since_ms.is_none_or(|since| row.ts_ms >= since)
                    })
                    .cloned()
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

    pub(crate) fn model_request_counts(
        &self,
        since_ms: u64,
    ) -> Result<HashMap<String, u64>, String> {
        match self {
            Store::Sqlite(conn) => {
                let guard = conn.lock().map_err(|_| "sqlite lock poisoned")?;
                let mut stmt = guard
                    .prepare("SELECT model, COUNT(*) FROM requests WHERE ts_ms >= ? GROUP BY model")
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
                    .filter(|row| row.ts_ms >= since_ms)
                {
                    *counts.entry(row.model.clone()).or_insert(0) += 1;
                }
                Ok(counts)
            }
        }
    }

    /// Aggregate filtered request history into the same shape as live stats.
    pub fn stats(&self, filter: &StatsFilter) -> Result<StatsSnapshot, String> {
        let outcomes = self.filtered_outcomes(filter)?;
        let mut snapshot = snapshot_from_outcomes(&outcomes, &filter.range_label, true);
        if let Some((label, bucket_ms)) =
            crate::services::stats::series_bucket_for_range(&filter.range_label)
        {
            let until_ms = now_ms();
            let since_ms = filter.since_ms.unwrap_or(0);
            snapshot.series_bucket = label.into();
            snapshot.series = crate::services::stats::series_from_outcomes(
                &outcomes, bucket_ms, since_ms, until_ms,
            );
        }
        Ok(snapshot)
    }

    /// Request outcomes matching `filter`, oldest first.
    pub fn filtered_outcomes(&self, filter: &StatsFilter) -> Result<Vec<RequestOutcome>, String> {
        match self {
            Store::Sqlite(conn) => {
                let guard = conn.lock().map_err(|_| "sqlite lock poisoned")?;
                let mut sql = String::from(
                    "SELECT model, profile, status, latency_ms, tokens_in, tokens_out,
                            ttft_ms, cache_read_tokens, cache_write_tokens, ts_ms, error, client
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

/// `SQLite` stores only `i64`; values here are counters/timestamps, so clamping
/// at the type bounds is the correct lossy edge behavior.
fn db_i64(v: u64) -> i64 {
    i64::try_from(v).unwrap_or(i64::MAX)
}

fn db_u64(v: i64) -> u64 {
    u64::try_from(v).unwrap_or(0)
}

fn migrate_requests_columns(conn: &Connection) -> Result<(), String> {
    let existing = {
        let mut stmt = conn
            .prepare("PRAGMA table_info(requests)")
            .map_err(|e| e.to_string())?;
        let names: Vec<String> = stmt
            .query_map([], |r| r.get::<_, String>(1))
            .map_err(|e| e.to_string())?
            .filter_map(Result::ok)
            .collect();
        names
    };
    let add = |name: &str, ddl: &str| -> Result<(), String> {
        if existing.iter().any(|col| col == name) {
            return Ok(());
        }
        conn.execute(ddl, [])
            .map_err(|e| format!("sqlite migrate add {name}: {e}"))?;
        Ok(())
    };
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
            recent[0].error.as_deref(),
            Some("exceeds the available context size")
        );
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
            .stats(&StatsFilter {
                since_ms: None,
                profile: Some("openai".into()),
                model: None,
                range_label: "all".into(),
            })
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
        assert_eq!(rows[0].status, 502);
        assert_eq!(rows[0].model, "openai/fail");
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

    #[test]
    fn unknown_store_kind_errors() {
        assert!(Store::open("postgres", None).is_err());
    }
}
