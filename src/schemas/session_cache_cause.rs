use serde::Serialize;

use crate::schemas::cache_write_cause::CacheWriteCause;

/// Cache-write requests and their cost for one cause.
#[derive(Debug, Clone, Copy, PartialEq, Serialize)]
pub struct SessionCacheCause {
    pub cause: CacheWriteCause,
    pub requests: u64,
    pub cache_write_tokens: u64,
    pub cache_write_usd: f64,
}
