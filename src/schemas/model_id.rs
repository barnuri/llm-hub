use crate::schemas::model_context::strip_1m_suffix;

/// A fully-qualified hub model id: `<profile>/<upstream model id>`.
/// Split happens on the FIRST slash only — upstream ids legitimately
/// contain slashes (`groq/meta-llama/Llama-4-Scout`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ModelId {
    pub profile: String,
    pub model: String,
}

impl ModelId {
    pub fn parse(raw: &str) -> Option<ModelId> {
        let raw = strip_1m_suffix(raw);
        let (profile, model) = raw.split_once('/')?;
        if profile.is_empty() || model.is_empty() {
            return None;
        }
        Some(ModelId {
            profile: profile.to_string(),
            model: model.to_string(),
        })
    }

    pub fn qualified(&self) -> String {
        format!("{}/{}", self.profile, self.model)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn splits_on_first_slash_only() {
        let id = ModelId::parse("groq/meta-llama/Llama-4-Scout").unwrap();
        assert_eq!(id.profile, "groq");
        assert_eq!(id.model, "meta-llama/Llama-4-Scout");
    }

    #[test]
    fn simple_id() {
        let id = ModelId::parse("openai/gpt-4o").unwrap();
        assert_eq!(id.profile, "openai");
        assert_eq!(id.model, "gpt-4o");
    }

    #[test]
    fn rejects_missing_slash() {
        assert_eq!(ModelId::parse("gpt-4o"), None);
    }

    #[test]
    fn rejects_empty_segments() {
        assert_eq!(ModelId::parse("/gpt-4o"), None);
        assert_eq!(ModelId::parse("openai/"), None);
        assert_eq!(ModelId::parse("/"), None);
        assert_eq!(ModelId::parse(""), None);
    }

    #[test]
    fn qualified_round_trip() {
        let id = ModelId::parse("vllm/org/model-x").unwrap();
        assert_eq!(id.qualified(), "vllm/org/model-x");
    }

    #[test]
    fn trailing_1m_suffix__stripped_from_upstream_id() {
        let id = ModelId::parse("llmgw/bedrock/anthropic.claude-opus-5[1m]").unwrap();
        assert_eq!(id.profile, "llmgw");
        assert_eq!(id.model, "bedrock/anthropic.claude-opus-5");
        assert_eq!(id.qualified(), "llmgw/bedrock/anthropic.claude-opus-5");
    }

    #[test]
    fn suffix_only_model_segment__rejected() {
        assert_eq!(ModelId::parse("llmgw/[1m]"), None);
    }
}
