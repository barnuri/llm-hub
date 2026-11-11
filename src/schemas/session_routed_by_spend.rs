use serde::Serialize;

/// Successful requests and their cost for one `routed_by` value. Rows written before the column group as `unknown`.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct SessionRoutedBySpend {
    pub routed_by: String,
    pub requests: u64,
    pub cost_usd: f64,
}
