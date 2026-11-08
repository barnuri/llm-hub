use serde::{Deserialize, Serialize};

/// The input that anchored a Smart Auto affinity key. Only this code is stored, never the value.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
#[repr(u8)]
pub enum AffinityScope {
    /// The caller's Claude Code session id.
    SessionHeader = 1,
    /// The body's `prompt_cache_key`.
    PromptCacheKey = 2,
    /// The hash of the conversation's system prompt and first message.
    ContentHash = 3,
    /// The caller's worker id.
    WorkerHeader = 4,
}
