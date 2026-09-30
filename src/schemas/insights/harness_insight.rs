use serde::Serialize;

/// How one client harness fared, for one model or across all of them.
#[derive(Serialize, Debug, Clone, PartialEq)]
pub struct HarnessInsight {
    pub harness: String,
    pub requests: u64,
    pub success_rate_pct: f64,
    pub ttft_p50_ms: Option<u64>,
    pub decode_tokens_per_sec_p50: Option<f64>,
}
