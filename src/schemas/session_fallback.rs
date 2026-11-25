use serde::Serialize;

/// Requests with no session id, grouped by the stored fallback key (client, stream key and a 30 minute gap).
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct SessionFallback {
    pub key: String,
    pub client: Option<String>,
    pub requests: u64,
    pub first_ts_ms: u64,
    pub last_ts_ms: u64,
    pub cache_write_tokens: u64,
    pub cache_write_usd: f64,
    pub cost_usd: f64,
}
