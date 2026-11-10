use serde::Serialize;

/// A request whose prompt dropped sharply from the previous request of the same session.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct SessionCompaction {
    pub ts_ms: u64,
    pub model: String,
    pub previous_prompt_tokens: u64,
    pub prompt_tokens: u64,
    pub drop_percent: u64,
}
