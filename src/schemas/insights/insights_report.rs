use serde::Serialize;

use super::harness_insight::HarnessInsight;
use super::insight_leaders::InsightLeaders;
use super::model_insight::ModelInsight;
use super::sessions_insight::SessionsInsight;
use super::use_case_pick::UseCasePick;

/// `/api/insights` body: ranked models first (best overall first), then the rest.
#[derive(Serialize, Debug, Clone, PartialEq)]
pub struct InsightsReport {
    pub range: String,
    pub min_requests: u64,
    pub generated_ms: u64,
    pub leaders: InsightLeaders,
    pub best_for: Vec<UseCasePick>,
    /// Up to three best-ranked free models, best first.
    pub best_free: Vec<String>,
    /// Up to three best-value ranked models, best first.
    pub best_value: Vec<String>,
    pub sessions: SessionsInsight,
    pub harnesses: Vec<HarnessInsight>,
    pub models: Vec<ModelInsight>,
}
