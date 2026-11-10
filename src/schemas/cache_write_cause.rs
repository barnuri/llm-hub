use serde::Serialize;

/// Why a request wrote to the prompt cache. `NoSessionId` marks requests without a session key.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum CacheWriteCause {
    FirstInSession,
    ModelSwitch,
    IdleOverTtl,
    Warm,
    NoSessionId,
}
