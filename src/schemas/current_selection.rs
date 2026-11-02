use serde::Serialize;

use crate::schemas::selection_event::SelectionEvent;
use crate::schemas::selection_state::SelectionState;

/// The current selection of one worker: its solver, the stages still running, and how fresh the view is.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct CurrentSelection {
    /// The worker's latest solver event, whatever its state.
    pub main: Option<SelectionEvent>,
    /// The stages dispatched and not yet finished, oldest first.
    pub active: Vec<SelectionEvent>,
    /// The state of the worker's latest event of any stage.
    pub state: SelectionState,
    /// Milliseconds since the latest event was recorded.
    pub age_ms: u64,
    /// True when the latest event is older than the stale threshold.
    pub stale: bool,
}
