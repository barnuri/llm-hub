/// Where a role decision came from.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum RoleSource {
    /// A caller hint from a header or a preset.
    Hint,
    /// A keyword or plan-marker rule over the user text.
    Rule,
    /// No rule matched, so the turn is plain conversation.
    Default,
}
