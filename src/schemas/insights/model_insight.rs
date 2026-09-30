use serde::Serialize;

use super::error_count::ErrorCount;
use super::harness_insight::HarnessInsight;
use super::model_health::ModelHealth;

/// Speed, stability and health of one model over the insights window.
///
/// Scores are `None` for models that are not ranked (too few calls, or
/// currently failing).
#[derive(Serialize, Debug, Clone, PartialEq)]
pub struct ModelInsight {
    pub model: String,
    pub profile: String,
    pub context_window: Option<u64>,
    pub use_cases: Vec<String>,
    pub summary: Option<String>,
    pub requests: u64,
    pub errors: u64,
    pub success_rate_pct: f64,
    pub consecutive_failures: u64,
    pub health: ModelHealth,
    pub ttft_p50_ms: Option<u64>,
    pub ttft_p95_ms: Option<u64>,
    pub latency_p50_ms: Option<u64>,
    pub decode_tokens_per_sec_p50: Option<f64>,
    pub prefill_tokens_per_sec_p50: Option<f64>,
    pub cache_hit_rate_pct: f64,
    pub tokens_in: u64,
    pub tokens_out: u64,
    pub top_errors: Vec<ErrorCount>,
    pub last_success_ms: Option<u64>,
    pub last_error_ms: Option<u64>,
    pub ranked: bool,
    pub stability_score: Option<f64>,
    pub speed_score: Option<f64>,
    pub overall_score: Option<f64>,
    pub by_harness: Vec<HarnessInsight>,
    pub best_harness: Option<String>,
}
