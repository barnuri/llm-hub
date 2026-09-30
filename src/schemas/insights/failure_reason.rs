use serde::Serialize;

/// One failure reason across every model in the window.
#[derive(Serialize, Debug, Clone, PartialEq, Eq)]
pub struct FailureReason {
    pub reason: String,
    pub count: u64,
    pub models: Vec<String>,
    pub last_seen: String,
}
