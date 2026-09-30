use serde::Serialize;

use super::failure_reason::FailureReason;
use super::insight_leaders::InsightLeaders;
use super::model_insight::ModelInsight;
use super::model_pick::ModelPick;

/// Self-contained insights for an agent choosing a model: plain-language
/// findings first, then every pick, per-model metrics and failure reasons.
#[derive(Serialize, Debug, Clone, PartialEq)]
pub struct AgentReport {
    pub generated_at: String,
    pub range: String,
    pub profile: Option<String>,
    pub min_requests: u64,
    pub scoring: String,
    pub caveats: Vec<String>,
    pub summary: Vec<String>,
    pub picks: Vec<ModelPick>,
    pub leaders: InsightLeaders,
    pub failure_reasons: Vec<FailureReason>,
    pub models: Vec<ModelInsight>,
}
