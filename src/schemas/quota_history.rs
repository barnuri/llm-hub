use serde::Serialize;

use crate::schemas::quota_daily_delta::QuotaDailyDelta;
use crate::schemas::quota_snapshot_row::QuotaSnapshotRow;

/// The stored Copilot quota readings of one range, and the daily burn computed from them.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct QuotaHistory {
    pub range: String,
    pub rows: Vec<QuotaSnapshotRow>,
    pub daily: Vec<QuotaDailyDelta>,
}
