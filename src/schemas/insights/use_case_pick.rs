use serde::Serialize;

/// The model to use for one declared use case (coding, planning, ...).
///
/// `ranked` is false when no tagged model has enough calls to rank; the pick
/// then rests on the registry declaration alone.
#[derive(Serialize, Debug, Clone, PartialEq, Eq)]
pub struct UseCasePick {
    pub use_case: String,
    pub model: String,
    pub ranked: bool,
    pub reason: String,
    pub candidates: Vec<String>,
}
