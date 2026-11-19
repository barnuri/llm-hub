use serde::{Deserialize, Serialize};

/// What a model directive in the user text did to the route.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DirectiveOutcome {
    /// The named model is in the pool, so the turn routed to it and pinned it.
    Applied,
    /// The text asked for auto, so the user pin was released.
    Cleared,
    /// The text named a model that is not in the eligible pool, so the router chose.
    NoMatch,
}
