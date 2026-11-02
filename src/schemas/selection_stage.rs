use serde::{Deserialize, Serialize};

// Only the solver is dispatched today, so the review variants are never built yet.
#[allow(dead_code)]
#[derive(Serialize, Deserialize, Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[serde(rename_all = "snake_case")]
pub enum SelectionStage {
    /// The main answer to the request.
    Solver,
    /// A reviewer that judges the solver's answer.
    Judge,
    /// A reviewer that critiques the solver's answer.
    Critic,
    /// A call that repairs an answer the review rejected.
    Repair,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn serde_uses_snake_case_names() {
        for (stage, name) in [
            (SelectionStage::Solver, "solver"),
            (SelectionStage::Judge, "judge"),
            (SelectionStage::Critic, "critic"),
            (SelectionStage::Repair, "repair"),
        ] {
            let json = serde_json::to_string(&stage).unwrap();
            assert_eq!(json, format!("\"{name}\""));
            assert_eq!(
                serde_json::from_str::<SelectionStage>(&json).unwrap(),
                stage
            );
        }
    }

    #[test]
    fn unknown_stage_is_rejected() {
        assert!(serde_json::from_str::<SelectionStage>("\"planner\"").is_err());
    }
}
