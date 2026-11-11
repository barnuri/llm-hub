use serde::Serialize;

use crate::schemas::keep_warm_summary::KeepWarmSummary;
use crate::schemas::prompt_stats::PromptStats;
use crate::schemas::session_routed_by_spend::SessionRoutedBySpend;

/// One session in the list. Requests count every row. Tokens, prompt sizes and cost count rows with status below 400.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct SessionSummary {
    pub worker: String,
    pub client: Option<String>,
    pub project: Option<String>,
    pub requests: u64,
    pub user_turns: u64,
    pub first_ts_ms: u64,
    pub last_ts_ms: u64,
    pub duration_ms: u64,
    pub models: Vec<String>,
    /// Distinct request streams (system prompt and tool set) in the session, such as the main conversation and each subagent type.
    pub streams: u64,
    pub prompt: Option<PromptStats>,
    pub cost_usd: f64,
    pub input_usd: f64,
    pub output_usd: f64,
    pub cache_read_usd: f64,
    pub cache_write_usd: f64,
    pub long_tier_usd: f64,
    pub long_tier_share: f64,
    pub compactions: u64,
    pub idle_cache_write_usd: f64,
    /// Successful requests and cost per `routed_by`, most expensive first.
    pub routed_by_spend: Vec<SessionRoutedBySpend>,
    /// Keep-warm pings and the cache writes their reads avoided, flattened into the summary.
    #[serde(flatten)]
    pub keep_warm: KeepWarmSummary,
}
