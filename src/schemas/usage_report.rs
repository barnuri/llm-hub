use serde::Serialize;

use super::usage_row::UsageRow;

#[derive(Serialize)]
pub struct UsageReport {
    pub total_requests: u64,
    pub total_errors: u64,
    pub total_tokens_in: u64,
    pub total_tokens_out: u64,
    pub recent: Vec<UsageRow>,
}
