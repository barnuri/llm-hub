use serde_json::Value;

/// A finished stage call: the chat completion and the bytes it used.
#[derive(Debug, Clone, PartialEq)]
pub struct StageReply {
    pub completion: Value,
    pub bytes: u64,
}
