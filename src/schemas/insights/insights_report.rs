use serde::Serialize;

use super::insight_leaders::InsightLeaders;
use super::model_insight::ModelInsight;

/// `/api/insights` body: ranked models first (best overall first), then the rest.
#[derive(Serialize, Debug, Clone, PartialEq)]
pub struct InsightsReport {
    pub range: String,
    pub min_requests: u64,
    pub generated_ms: u64,
    pub leaders: InsightLeaders,
    pub models: Vec<ModelInsight>,
}
