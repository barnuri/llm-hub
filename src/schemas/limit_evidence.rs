use serde::{Deserialize, Serialize};

use crate::schemas::evidence_source::EvidenceSource;

/// One claimed token limit for a model, with its provenance.
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq)]
pub struct LimitEvidence {
    /// Limit in tokens.
    pub value: u64,
    /// Where the value came from.
    pub source: EvidenceSource,
    /// Profile and endpoint the value applies to, e.g. `copilot:/v1/messages`.
    pub scope: String,
    /// Unix milliseconds (UTC) when the value was observed, if it was observed.
    pub observed_at: Option<u64>,
    /// How far the value can be trusted, from 0.0 to 1.0.
    pub confidence: f32,
    /// True when another source for the same scope disagrees.
    pub conflict: bool,
}

impl LimitEvidence {
    /// Evidence with no observation time and no known conflict.
    #[must_use]
    pub fn new(value: u64, source: EvidenceSource, scope: &str, confidence: f32) -> LimitEvidence {
        LimitEvidence {
            value,
            source,
            scope: scope.to_string(),
            observed_at: None,
            confidence,
            conflict: false,
        }
    }

    /// True when both values cannot hold for the same scope at once.
    /// A lower bound only disagrees with a ceiling below it.
    #[must_use]
    pub fn conflicts_with(&self, other: &LimitEvidence) -> bool {
        if self.scope != other.scope {
            return false;
        }
        match (self.source.is_lower_bound(), other.source.is_lower_bound()) {
            (true, true) => false,
            (true, false) => self.value > other.value,
            (false, true) => other.value > self.value,
            (false, false) => self.value != other.value,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn evidence(value: u64, source: EvidenceSource, scope: &str) -> LimitEvidence {
        LimitEvidence {
            value,
            source,
            scope: scope.to_owned(),
            observed_at: None,
            confidence: 0.5,
            conflict: false,
        }
    }

    #[test]
    fn serde_round_trip_keeps_every_field() {
        let original = LimitEvidence {
            observed_at: Some(1_700_000_000_000),
            conflict: true,
            ..evidence(
                200_000,
                EvidenceSource::ObservedRejection,
                "copilot:/v1/messages",
            )
        };
        let json = serde_json::to_string(&original).unwrap();
        assert!(json.contains(r#""source":"observed_rejection""#));
        let parsed: LimitEvidence = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed, original);
    }

    #[test]
    fn different_values_in_the_same_scope_conflict() {
        let listed = evidence(
            128_000,
            EvidenceSource::Listed,
            "copilot:/v1/chat/completions",
        );
        let seeded = evidence(
            200_000,
            EvidenceSource::Seeded,
            "copilot:/v1/chat/completions",
        );
        assert!(listed.conflicts_with(&seeded));
        assert!(seeded.conflicts_with(&listed));
    }

    #[test]
    fn equal_values_do_not_conflict() {
        let listed = evidence(
            128_000,
            EvidenceSource::Listed,
            "copilot:/v1/chat/completions",
        );
        let seeded = evidence(
            128_000,
            EvidenceSource::Seeded,
            "copilot:/v1/chat/completions",
        );
        assert!(!listed.conflicts_with(&seeded));
    }

    #[test]
    fn different_scopes_never_conflict() {
        let messages = evidence(1_000_000, EvidenceSource::Listed, "copilot:/v1/messages");
        let chat = evidence(
            128_000,
            EvidenceSource::Listed,
            "copilot:/v1/chat/completions",
        );
        assert!(!messages.conflicts_with(&chat));
    }

    #[test]
    fn success_below_a_ceiling_does_not_conflict() {
        let success = evidence(
            90_000,
            EvidenceSource::ObservedSuccess,
            "local:/v1/chat/completions",
        );
        let runtime = evidence(
            131_072,
            EvidenceSource::LocalRuntime,
            "local:/v1/chat/completions",
        );
        assert!(!success.conflicts_with(&runtime));
        assert!(!runtime.conflicts_with(&success));
    }

    #[test]
    fn success_above_a_ceiling_conflicts() {
        let success = evidence(
            150_000,
            EvidenceSource::ObservedSuccess,
            "local:/v1/chat/completions",
        );
        let runtime = evidence(
            131_072,
            EvidenceSource::LocalRuntime,
            "local:/v1/chat/completions",
        );
        assert!(success.conflicts_with(&runtime));
        assert!(runtime.conflicts_with(&success));
    }

    #[test]
    fn two_lower_bounds_never_conflict() {
        let small = evidence(
            10_000,
            EvidenceSource::ObservedSuccess,
            "local:/v1/messages",
        );
        let large = evidence(
            90_000,
            EvidenceSource::ObservedSuccess,
            "local:/v1/messages",
        );
        assert!(!small.conflicts_with(&large));
    }
}
