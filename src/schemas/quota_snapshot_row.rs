use serde::Serialize;

/// One stored Copilot quota reading. `used_usd` and `limit_usd` are present only for AI-credit quotas.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct QuotaSnapshotRow {
    pub ts_ms: u64,
    pub profile: String,
    pub quota: String,
    pub used: f64,
    pub entitlement: f64,
    pub used_usd: Option<f64>,
    pub limit_usd: Option<f64>,
}
