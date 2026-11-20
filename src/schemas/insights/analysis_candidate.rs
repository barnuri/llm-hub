use serde::Serialize;

/// A model offered for the manual insights analysis, with its estimated cost per run.
#[derive(Serialize, Debug, Clone, PartialEq)]
pub struct AnalysisCandidate {
    pub model: String,
    pub quality_score: Option<f64>,
    pub free: bool,
    /// Estimated cost of one analysis run in USD, from the price book. Zero for a free model.
    pub estimated_cost_usd: f64,
    /// The highest-quality model whose run costs no more than the fair limit.
    pub recommended: bool,
    pub reason: String,
}
