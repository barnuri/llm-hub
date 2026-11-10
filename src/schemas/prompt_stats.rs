use serde::Serialize;

/// Prompt-size distribution of a session, in tokens, over its requests with status below 400.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
pub struct PromptStats {
    pub min: u64,
    pub median: u64,
    pub p90: u64,
    pub max: u64,
    /// Timestamp of the first request that reached `max`.
    pub peak_ts_ms: u64,
}
