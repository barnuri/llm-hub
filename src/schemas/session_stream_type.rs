use serde::Serialize;

/// Successful requests and cost for one stream label, such as `main` or `subagent:explore`.
/// Rows with no recorded label group as `unknown`.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct SessionStreamType {
    pub label: String,
    pub requests: u64,
    /// Distinct stream keys with this label.
    pub streams: u64,
    pub cost_usd: f64,
    pub cache_write_usd: f64,
}
