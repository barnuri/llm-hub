use serde::Serialize;

use super::usage_row::UsageRow;

#[derive(Serialize)]
pub struct ErrorsReport {
    pub range: String,
    pub total_errors: u64,
    pub recent: Vec<UsageRow>,
}
