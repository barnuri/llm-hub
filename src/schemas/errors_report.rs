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
    /// Every failure in the window grouped by normalized signature, largest first.
    pub groups: Vec<ErrorGroup>,
}

/// A name and how many failures it had in a group.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct NameCount {
    pub name: String,
    pub count: u64,
}

/// Failures that share one normalized signature.
#[derive(Debug, Clone, Serialize)]
pub struct ErrorGroup {
    /// Key for `POST /api/errors/resolve-group`.
    pub signature: String,
    pub count: u64,
    pub open: u64,
    pub resolved: u64,
    pub first_ts_ms: u64,
    pub last_ts_ms: u64,
    /// Top profiles and models by failure count; the `*_count` fields give the full number.
    pub profiles: Vec<NameCount>,
    pub models: Vec<NameCount>,
    pub profile_count: usize,
    pub model_count: usize,
    /// Raw reason of the newest failure in the group.
    pub sample: Option<String>,
    pub sample_status: u16,
}
