use serde::{Deserialize, Serialize};

/// One model and the score the router gave it for a turn.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RankedModel {
    /// The qualified model id.
    pub model: String,
    /// The router score of the model for the turn's role and tier.
    pub score: u32,
}
