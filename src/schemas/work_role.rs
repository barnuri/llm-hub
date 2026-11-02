use serde::{Deserialize, Serialize};

/// The kind of work a request does, which sets the model selection objective.
/// A role hint never widens the eligible model pool.
#[derive(Serialize, Deserialize, Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[serde(rename_all = "snake_case")]
pub enum WorkRole {
    /// Design, decomposition and full-context understanding.
    Planning,
    /// Tool use, hypothesis testing and interpreting evidence.
    Investigation,
    /// A scoped task with a plan or skill, served by the cheapest and fastest capable worker.
    Implementation,
    /// Reliable review and failure detection.
    Validation,
    /// General chat, served with prompt-appropriate quality and speed.
    Conversation,
}

impl WorkRole {
    /// The `snake_case` name the serde form and the response headers spell.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            WorkRole::Planning => "planning",
            WorkRole::Investigation => "investigation",
            WorkRole::Implementation => "implementation",
            WorkRole::Validation => "validation",
            WorkRole::Conversation => "conversation",
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn serde_round_trips_snake_case_names() {
        for (role, name) in [
            (WorkRole::Planning, "planning"),
            (WorkRole::Investigation, "investigation"),
            (WorkRole::Implementation, "implementation"),
            (WorkRole::Validation, "validation"),
            (WorkRole::Conversation, "conversation"),
        ] {
            let json = serde_json::to_string(&role).unwrap();
            assert_eq!(json, format!("\"{name}\""));
            assert_eq!(serde_json::from_str::<WorkRole>(&json).unwrap(), role);
        }
    }

    #[test]
    fn unknown_role_is_rejected() {
        assert!(serde_json::from_str::<WorkRole>("\"coding\"").is_err());
    }
}
