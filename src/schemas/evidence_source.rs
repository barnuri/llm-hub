use serde::{Deserialize, Serialize};

use crate::services::model_limits::LimitSource;

/// Where a model limit value came from.
#[derive(Serialize, Deserialize, Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[serde(rename_all = "snake_case")]
pub enum EvidenceSource {
    /// Published by the model vendor for the whole model family.
    OfficialFamily,
    /// Published by the serving provider for its deployment of the model.
    OfficialProvider,
    /// Read from a community catalog such as models.dev or a gateway's model list.
    Community,
    /// Reported by the local inference server, such as llama.cpp `--ctx-size`.
    LocalRuntime,
    /// Set by the operator in hub configuration.
    OperatorOverride,
    /// Measured earlier and compiled into the hub.
    Seeded,
    /// A request of this size succeeded, so the limit is at least this value.
    ObservedSuccess,
    /// Read from an upstream rejection that states the maximum.
    ObservedRejection,
    /// Listed by the upstream `/models` endpoint.
    Listed,
}

impl EvidenceSource {
    /// An accepted request proves only that the limit is at least its size.
    #[must_use]
    pub fn is_lower_bound(self) -> bool {
        self == EvidenceSource::ObservedSuccess
    }
}

impl From<LimitSource> for EvidenceSource {
    fn from(source: LimitSource) -> EvidenceSource {
        match source {
            LimitSource::Seeded => EvidenceSource::Seeded,
            LimitSource::Learned => EvidenceSource::ObservedRejection,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn limit_sources_map_to_seeded_and_observed_rejection() {
        assert_eq!(
            EvidenceSource::from(LimitSource::Seeded),
            EvidenceSource::Seeded
        );
        assert_eq!(
            EvidenceSource::from(LimitSource::Learned),
            EvidenceSource::ObservedRejection
        );
    }

    #[test]
    fn serializes_as_snake_case() {
        let json = serde_json::to_string(&EvidenceSource::ObservedRejection).unwrap();
        assert_eq!(json, r#""observed_rejection""#);
        let parsed: EvidenceSource = serde_json::from_str(r#""official_family""#).unwrap();
        assert_eq!(parsed, EvidenceSource::OfficialFamily);
    }

    #[test]
    fn only_observed_success_is_a_lower_bound() {
        assert!(EvidenceSource::ObservedSuccess.is_lower_bound());
        assert!(!EvidenceSource::ObservedRejection.is_lower_bound());
        assert!(!EvidenceSource::Listed.is_lower_bound());
    }
}
