use serde::Serialize;

/// Tool-result text in a session's newest prompt, for one tool family. Estimated tokens are bytes divided by four.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct SessionToolResult {
    pub family: String,
    pub bytes: u64,
    pub estimated_tokens: u64,
}
