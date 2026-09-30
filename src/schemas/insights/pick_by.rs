use serde::{Deserialize, Serialize};

/// Ranking criterion for `/api/insights/pick`.
#[derive(Serialize, Deserialize, Debug, Clone, Copy, PartialEq, Eq, Default)]
#[serde(rename_all = "snake_case")]
pub enum PickBy {
    #[default]
    Overall,
    Stability,
    Speed,
    Ttft,
    Decode,
    Prefill,
}

impl PickBy {
    /// Every criterion, in the order reports list them.
    pub const ALL: [PickBy; 6] = [
        PickBy::Overall,
        PickBy::Stability,
        PickBy::Speed,
        PickBy::Ttft,
        PickBy::Decode,
        PickBy::Prefill,
    ];
}
