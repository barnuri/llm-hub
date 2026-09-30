use serde::Serialize;

/// The leading model id per category; `None` when no model qualifies.
#[derive(Serialize, Debug, Clone, Default, PartialEq, Eq)]
pub struct InsightLeaders {
    pub best_overall: Option<String>,
    pub most_stable: Option<String>,
    pub fastest_first_token: Option<String>,
    pub fastest_decode: Option<String>,
    pub fastest_prefill: Option<String>,
    pub most_used: Option<String>,
}
