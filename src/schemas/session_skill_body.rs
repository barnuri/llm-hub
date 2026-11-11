use serde::Serialize;

/// One skill body in a session's newest prompt, by skill name. Estimated tokens are characters divided by four.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct SessionSkillBody {
    pub name: String,
    pub chars: u64,
    pub estimated_tokens: u64,
}
