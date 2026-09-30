use serde::Serialize;

/// A model and its value for the chosen `PickBy` criterion.
#[derive(Serialize, Debug, Clone, PartialEq)]
pub struct PickCandidate {
    pub model: String,
    pub value: f64,
}
