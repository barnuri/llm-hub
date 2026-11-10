use serde::Serialize;

/// One request in a session's prompt-size series.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct SessionPromptPoint {
    pub ts_ms: u64,
    pub model: String,
    pub prompt_tokens: u64,
    pub cost_usd: f64,
}
