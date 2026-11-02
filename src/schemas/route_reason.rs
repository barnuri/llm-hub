/// Why the router chose its model.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum RouteReason {
    /// The turn stays on its pin because the role did not change materially or is not settled.
    Pinned,
    /// A tool continuation replays the pinned solver, whatever the role.
    ToolContinuationReplay,
    /// The turn returns to a role and reuses the model that role used before.
    Incumbent,
    /// A material role change moved the turn to the best model for the new role.
    RoleSwitch,
    /// No usable pin or incumbent exists, so the best model for the role was picked.
    Fresh,
}

impl RouteReason {
    /// The `snake_case` reason code that the response header and the decision log carry.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            RouteReason::Pinned => "pinned",
            RouteReason::ToolContinuationReplay => "tool_continuation_replay",
            RouteReason::Incumbent => "incumbent",
            RouteReason::RoleSwitch => "role_switch",
            RouteReason::Fresh => "fresh",
        }
    }
}
