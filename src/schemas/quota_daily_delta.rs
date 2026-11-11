use serde::Serialize;

/// The quota burned on one UTC day by one profile's quota, summed from the changes between its readings.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct QuotaDailyDelta {
    /// UTC day as `YYYY-MM-DD`.
    pub day: String,
    /// Profile name.
    pub profile: String,
    /// Quota name, such as `premium_interactions`.
    pub quota: String,
    /// Units burned that day.
    pub used_delta: f64,
    /// USD burned that day, when the quota reports dollars.
    pub used_usd_delta: Option<f64>,
}
