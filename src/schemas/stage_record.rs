use crate::schemas::selection_stage::SelectionStage;
use crate::schemas::selection_state::SelectionState;

/// One stage's place in a review, as the selection log records it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StageRecord {
    pub stage: SelectionStage,
    /// Qualified id of the model that ran the stage.
    pub model: String,
    pub state: SelectionState,
    /// Why the stage was rejected, skipped or cancelled.
    pub reason: Option<&'static str>,
}
