use serde::{Deserialize, Serialize};

/// The parts of one Smart Auto score: the inputs after tier weighting, and the total the router
/// ranks by. A `total` includes the capable bonus and subtracts `penalty`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct ScoreParts {
    /// The seed quality for the role, 0 to 10, before tier weighting.
    pub quality: u8,
    /// The cheapness, 0 to 10: the live cost score, else the seed.
    pub cost: u8,
    /// The speed, 0 to 10: the measured speed blended with the seed, else the seed.
    pub speed: u32,
    /// Points lost to the error rate and a low quota.
    pub penalty: u32,
    /// The quality reaches the capable floor, so the score carries the capable bonus.
    pub capable: bool,
    /// The weighted score the router ranks by.
    pub total: u32,
}
