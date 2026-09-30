use serde::Serialize;

use super::pick_by::PickBy;
use super::pick_candidate::PickCandidate;

/// The best model for one criterion, plus the runners-up in order.
#[derive(Serialize, Debug, Clone, PartialEq)]
pub struct ModelPick {
    pub by: PickBy,
    pub model: String,
    pub value: f64,
    pub reason: String,
    pub alternatives: Vec<PickCandidate>,
}
