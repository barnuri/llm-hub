use std::path::PathBuf;
use std::sync::Mutex;
use std::time::{SystemTime, UNIX_EPOCH};

use crate::schemas::api_key_record::ApiKeyRecord;
use crate::schemas::usage_report::UsageReport;
use crate::schemas::usage_row::UsageRow;
use rusqlite::Connection;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::services::stats::{RequestOutcome, StatsSnapshot, snapshot_from_outcomes};

const DEFAULT_SQLITE_PATH: &str = "llm-hub.db";
const DEFAULT_JSON_PATH: &str = "llm-hub-stats.json";
const USAGE_RECENT_LIMIT: usize = 200;
const ERROR_STATUS_MIN: u16 = 400;

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
        };
        match self {
            Store::Sqlite(conn) => {
                let guard = conn.lock().map_err(|_| "sqlite lock poisoned")?;
                guard
                    .execute(
                        "INSERT INTO requests (
                            ts_ms, model, profile, status, latency_ms, tokens_in, tokens_out,
                            ttft_ms, cache_read_tokens, cache_write_tokens
                         ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10)",
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
                            db_i64(row.cache_write_tokens)
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
                                ttft_ms, cache_read_tokens, cache_write_tokens
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
                            ttft_ms, cache_read_tokens, cache_write_tokens
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

    fn filtered_outcomes(&self, filter: &StatsFilter) -> Result<Vec<RequestOutcome>, String> {
        match self {
            Store::Sqlite(conn) => {
                let guard = conn.lock().map_err(|_| "sqlite lock poisoned")?;
                let mut sql = String::from(
                    "SELECT model, profile, status, latency_ms, tokens_in, tokens_out,
                            ttft_ms, cache_read_tokens, cache_write_tokens, ts_ms
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
    fn sqlite_errors__only_status_400_plus() {
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

    #[test]
    fn unknown_store_kind_errors() {
        assert!(Store::open("postgres", None).is_err());
    }
}
