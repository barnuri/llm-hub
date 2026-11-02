use serde::{Deserialize, Serialize};

/// Where one dispatch stands. A dispatch emits selected, then started, then one terminal state.
#[derive(Serialize, Deserialize, Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[serde(rename_all = "snake_case")]
pub enum SelectionState {
    /// The model was chosen and is about to be called.
    Selected,
    /// The upstream accepted the attempt with a success status and its body is streaming.
    Started,
    /// The upstream body ended and the answer is complete.
    Completed,
    /// The upstream answered with an error status or the call failed.
    Rejected,
    /// The body stopped before the answer finished, such as on a client disconnect.
    Cancelled,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn serde_uses_snake_case_names() {
        for (state, name) in [
            (SelectionState::Selected, "selected"),
            (SelectionState::Started, "started"),
            (SelectionState::Completed, "completed"),
            (SelectionState::Rejected, "rejected"),
            (SelectionState::Cancelled, "cancelled"),
        ] {
            let json = serde_json::to_string(&state).unwrap();
            assert_eq!(json, format!("\"{name}\""));
            assert_eq!(
                serde_json::from_str::<SelectionState>(&json).unwrap(),
                state
            );
        }
    }
}
