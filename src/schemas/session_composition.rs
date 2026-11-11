use serde::Serialize;

use crate::schemas::session_skill_body::SessionSkillBody;
use crate::schemas::session_tool_result::SessionToolResult;

/// Context composition from a session's newest prompt-shape row. Token figures are estimates (characters or bytes / 4).
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct SessionComposition {
    pub system_chars: u64,
    pub system_estimated_tokens: u64,
    pub tool_count: u64,
    pub tool_bytes: u64,
    pub tool_estimated_tokens: u64,
    pub mcp_tool_count: u64,
    pub mcp_tool_bytes: u64,
    pub mcp_tool_estimated_tokens: u64,
    pub deferred_listing_chars: Option<u64>,
    pub deferred_listing_estimated_tokens: Option<u64>,
    pub skills_listing_chars: Option<u64>,
    pub skills_listing_estimated_tokens: Option<u64>,
    pub claude_md_chars: Option<u64>,
    pub claude_md_estimated_tokens: Option<u64>,
    pub image_count: u64,
    /// Tool-result families, largest first.
    pub tool_results: Vec<SessionToolResult>,
    /// Skill bodies in the user messages, largest first.
    pub skill_bodies: Vec<SessionSkillBody>,
}
