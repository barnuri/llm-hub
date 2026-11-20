use serde::Serialize;

/// The leading models per category, best first, at most three each. Empty when no model qualifies.
#[derive(Serialize, Debug, Clone, Default, PartialEq, Eq)]
pub struct InsightLeaders {
    pub best_overall: Vec<String>,
    pub most_stable: Vec<String>,
    /// By the lower bound of the verified pass rate.
    pub best_verified: Vec<String>,
    pub fastest_first_token: Vec<String>,
    pub fastest_decode: Vec<String>,
    pub fastest_prefill: Vec<String>,
    pub most_used: Vec<String>,
}
