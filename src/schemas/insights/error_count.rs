use serde::Serialize;

/// One failure reason and how often it was recorded.
#[derive(Serialize, Debug, Clone, PartialEq, Eq)]
pub struct ErrorCount {
    pub reason: String,
    pub count: u64,
}
