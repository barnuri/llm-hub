use serde::Deserialize;
use utoipa::ToSchema;

/// The model a manual insights analysis runs on, as `<profile>/<model>`.
#[derive(Debug, Clone, Deserialize, ToSchema)]
pub struct InsightsAnalysisInput {
    pub model: String,
}
