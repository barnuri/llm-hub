use serde::{Deserialize, Serialize};

use crate::schemas::limit_evidence::LimitEvidence;

/// Everything the hub knows about one concrete model on one profile.
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq)]
pub struct ModelDescriptor {
    /// `<profile>/<model>` as the hub advertises it.
    pub qualified_id: String,
    /// Hub profile that serves the model, e.g. `copilot`.
    pub profile: String,
    /// Client routes that serve this model, e.g. `/v1/messages`.
    pub endpoints: Vec<String>,
    /// Combined input and output capacity.
    pub context: Option<LimitEvidence>,
    /// Prompt tokens the upstream accepts. `None` when no source reports it.
    pub input: Option<LimitEvidence>,
    /// Completion tokens the upstream returns at most.
    pub output: Option<LimitEvidence>,
    /// Capacity the local inference server actually allocated.
    pub runtime_context: Option<LimitEvidence>,
    /// Reasoning effort levels the upstream publishes, empty when it publishes none.
    pub effort_levels: Vec<String>,
    /// True for text chat models, false for speech and embedding models.
    pub chat: bool,
    /// `None` when no source reports vision support.
    pub vision: Option<bool>,
    /// `None` when no source reports tool calling support.
    pub tool_support: Option<bool>,
}

#[cfg(test)]
impl ModelDescriptor {
    /// A chat model on `qualified_id` with no endpoints, limits or effort levels.
    /// Tests set the fields they need with struct update syntax.
    pub(crate) fn for_test(qualified_id: &str) -> ModelDescriptor {
        let profile = qualified_id
            .split_once('/')
            .map_or("", |(profile, _)| profile);
        ModelDescriptor {
            qualified_id: qualified_id.to_owned(),
            profile: profile.to_owned(),
            endpoints: Vec::new(),
            context: None,
            input: None,
            output: None,
            runtime_context: None,
            effort_levels: Vec::new(),
            chat: true,
            vision: None,
            tool_support: None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::schemas::evidence_source::EvidenceSource;

    #[test]
    fn serde_round_trip_keeps_unknowns_as_null() {
        let original = ModelDescriptor {
            qualified_id: "copilot/claude-opus-4-5".to_owned(),
            profile: "copilot".to_owned(),
            endpoints: vec!["/v1/messages".to_owned(), "/v1/chat/completions".to_owned()],
            context: None,
            input: Some(LimitEvidence {
                value: 168_000,
                source: EvidenceSource::Listed,
                scope: "copilot:/v1/messages".to_owned(),
                observed_at: None,
                confidence: 0.6,
                conflict: false,
            }),
            output: None,
            runtime_context: None,
            effort_levels: vec!["low".to_owned(), "medium".to_owned(), "high".to_owned()],
            chat: true,
            vision: Some(true),
            tool_support: None,
        };
        let json = serde_json::to_value(&original).unwrap();
        assert_eq!(json["output"], serde_json::Value::Null);
        assert_eq!(json["tool_support"], serde_json::Value::Null);
        assert_eq!(json["input"]["source"], "listed");
        let parsed: ModelDescriptor = serde_json::from_value(json).unwrap();
        assert_eq!(parsed, original);
    }
}
