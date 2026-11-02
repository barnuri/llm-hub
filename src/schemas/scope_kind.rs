/// What a scope key identifies, so equal raw ids of different kinds never share a key.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ScopeKind {
    Workflow,
    Worker,
    Attempt,
    Turn,
}

impl ScopeKind {
    /// The name hashed into a scope key to separate the kinds.
    pub(crate) fn label(self) -> &'static str {
        match self {
            ScopeKind::Workflow => "workflow",
            ScopeKind::Worker => "worker",
            ScopeKind::Attempt => "attempt",
            ScopeKind::Turn => "turn",
        }
    }
}
