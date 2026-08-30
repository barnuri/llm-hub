use std::path::PathBuf;
use std::sync::Mutex;
use std::time::{SystemTime, UNIX_EPOCH};

use crate::schemas::api_key_record::ApiKeyRecord;
use crate::schemas::usage_report::UsageReport;
use crate::schemas::usage_row::UsageRow;
use rusqlite::Connection;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::services::stats::RequestOutcome;

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
}

#[derive(Debug, Serialize, Deserialize, Default, Clone)]
struct Totals {
    requests: u64,
    errors: u64,
    tokens_in: u64,
    tokens_out: u64,
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
                tokens_out INTEGER NOT NULL
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
            tokens_in: outcome.tokens_in,
            tokens_out: outcome.tokens_out,
        };
        match self {
            Store::Sqlite(conn) => {
                let guard = conn.lock().map_err(|_| "sqlite lock poisoned")?;
                guard
                    .execute(
                        "INSERT INTO requests (ts_ms, model, profile, status, latency_ms, tokens_in, tokens_out)
                         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
                        rusqlite::params![
                            db_i64(row.ts_ms),
                            row.model,
                            row.profile,
                            i64::from(row.status),
                            db_i64(row.latency_ms),
                            db_i64(row.tokens_in),
                            db_i64(row.tokens_out)
                        ],
                    )
                    .map_err(|e| e.to_string())?;
                Ok(())
            }
            Store::Json(store) => {
                let mut guard = store.lock().map_err(|_| "json store lock poisoned")?;
                guard.state.totals.requests += 1;
                if row.status >= 400 {
                    guard.state.totals.errors += 1;
                }
                guard.state.totals.tokens_in += row.tokens_in;
                guard.state.totals.tokens_out += row.tokens_out;
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
                        "SELECT ts_ms, model, profile, status, latency_ms, tokens_in, tokens_out
                         FROM requests ORDER BY id DESC LIMIT ?1",
                    )
                    .map_err(|e| e.to_string())?;
                let limit = i64::try_from(USAGE_RECENT_LIMIT).unwrap_or(i64::MAX);
                let recent = stmt
                    .query_map([limit], |r| {
                        Ok(UsageRow {
                            ts_ms: db_u64(r.get::<_, i64>(0)?),
                            model: r.get(1)?,
                            profile: r.get(2)?,
                            status: u16::try_from(r.get::<_, i64>(3)?).unwrap_or(0),
                            latency_ms: db_u64(r.get::<_, i64>(4)?),
                            tokens_in: db_u64(r.get::<_, i64>(5)?),
                            tokens_out: db_u64(r.get::<_, i64>(6)?),
                        })
                    })
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

#[cfg(test)]
mod tests {
    use super::*;

    fn sample_outcome() -> RequestOutcome {
        RequestOutcome {
            profile: "openai".into(),
            model_key: "openai/gpt-4o".into(),
            status: 200,
            latency_ms: 42,
            tokens_in: 5,
            tokens_out: 7,
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
    }

    #[test]
    fn unknown_store_kind_errors() {
        assert!(Store::open("postgres", None).is_err());
    }
}
