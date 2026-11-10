/// Counts and byte sizes of one request's prompt, with no prompt text. The strings are the project's
/// directory basename and the stream key, a hex hash of the system prompt and tool set.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct PromptShape {
    /// Characters of every text block in `system`.
    pub system_chars: u64,
    /// Entries in `messages`.
    pub messages: u64,
    /// The last message is a user message with a text block (not only tool results).
    pub turn_start: bool,
    /// Tool definitions in the request.
    pub tool_count: u64,
    /// Serialized bytes of every tool definition.
    pub tool_bytes: u64,
    /// Tools whose name starts with `mcp__`.
    pub mcp_tool_count: u64,
    /// Serialized bytes of the MCP tool definitions.
    pub mcp_tool_bytes: u64,
    /// Characters of the text block that lists the deferred tools, when present.
    pub deferred_listing_chars: Option<u64>,
    /// Characters of the text block that lists the skills, when present.
    pub skills_listing_chars: Option<u64>,
    /// Summed characters of the blocks that inject a CLAUDE.md file, when present.
    pub claude_md_chars: Option<u64>,
    /// Image blocks in messages, including those inside tool results.
    pub image_count: u64,
    /// The basename of the working directory the body states, never the full path.
    pub project: Option<String>,
    /// Hex hash of the system text and the sorted tool names. Requests with the same key come from
    /// the same conversation or subagent type.
    pub stream_key: Option<String>,
}
