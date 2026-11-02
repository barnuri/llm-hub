use serde::{Deserialize, Serialize};

/// A reasoning effort level, named as Copilot `/models` and the Anthropic API publish it.
/// Variants are declared from least to most reasoning, so `Ord` follows that scale.
#[derive(Serialize, Deserialize, Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
#[serde(rename_all = "snake_case")]
pub enum EffortLevel {
    /// Reasoning switched off.
    None,
    /// The least reasoning a reasoning model allows.
    Minimal,
    /// Light reasoning.
    Low,
    /// Moderate reasoning.
    Medium,
    /// Deep reasoning.
    High,
    /// Deeper reasoning than high.
    Xhigh,
    /// The most reasoning the model allows.
    Max,
}

impl EffortLevel {
    /// Every level, from least to most reasoning.
    pub const ALL: [EffortLevel; 7] = [
        EffortLevel::None,
        EffortLevel::Minimal,
        EffortLevel::Low,
        EffortLevel::Medium,
        EffortLevel::High,
        EffortLevel::Xhigh,
        EffortLevel::Max,
    ];

    /// The level with this name, ignoring ASCII case, e.g. `high`.
    /// Any other name returns `None`.
    #[must_use]
    pub fn from_name(name: &str) -> Option<EffortLevel> {
        Self::ALL
            .into_iter()
            .find(|level| level.as_str().eq_ignore_ascii_case(name))
    }

    /// The published name, e.g. `xhigh`.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            EffortLevel::None => "none",
            EffortLevel::Minimal => "minimal",
            EffortLevel::Low => "low",
            EffortLevel::Medium => "medium",
            EffortLevel::High => "high",
            EffortLevel::Xhigh => "xhigh",
            EffortLevel::Max => "max",
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_name_round_trips_ignoring_case() {
        for level in EffortLevel::ALL {
            assert_eq!(EffortLevel::from_name(level.as_str()), Some(level));
            assert_eq!(
                EffortLevel::from_name(&level.as_str().to_ascii_uppercase()),
                Some(level)
            );
        }
    }

    #[test]
    fn unknown_name_is_rejected() {
        for name in ["", "extreme", "x-high", " high"] {
            assert_eq!(EffortLevel::from_name(name), None, "{name}");
        }
    }

    #[test]
    fn order_runs_from_none_to_max() {
        assert!(EffortLevel::ALL.is_sorted());
        assert!(EffortLevel::None < EffortLevel::Minimal);
        assert!(EffortLevel::High < EffortLevel::Xhigh);
        assert!(EffortLevel::Xhigh < EffortLevel::Max);
    }

    #[test]
    fn serde_names_match_published_names() {
        for level in EffortLevel::ALL {
            let json = serde_json::to_string(&level).unwrap();
            assert_eq!(json, format!("\"{}\"", level.as_str()));
            assert_eq!(serde_json::from_str::<EffortLevel>(&json).unwrap(), level);
        }
    }
}
