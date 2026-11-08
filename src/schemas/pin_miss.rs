use serde::{Deserialize, Serialize};

/// Why a Smart Auto turn found no pin to reuse, so it routed fresh.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PinMiss {
    /// The request names no affinity key.
    NoKey,
    /// The key has no pin: a new conversation, or a pin dropped by an earlier miss.
    NoPin,
    /// The pin sat idle past its limit and was removed.
    IdleExpired,
    /// The pinned model is no longer in the eligible pool, so the pin was dropped.
    LeftPool,
}
