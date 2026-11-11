use serde::Serialize;

use super::usage_row::UsageRow;

/// One failed request. `id` is `None` for the JSON store, which cannot mark rows solved.
#[derive(Serialize)]
pub struct ErrorEntry {
    pub id: Option<i64>,
    /// Unix ms when a person marked the error solved. `None` while open.
    pub resolved_ms: Option<u64>,
    #[serde(flatten)]
    pub row: UsageRow,
}

#[derive(Serialize)]
pub struct ErrorsReport {
    pub range: String,
    /// All failed calls in the window, solved included.
    pub total_errors: u64,
    /// Failed calls in the window not yet marked solved.
    pub open_errors: u64,
    pub recent: Vec<ErrorEntry>,
}
