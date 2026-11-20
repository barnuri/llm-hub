use serde::Serialize;

/// Sessions in the insights window. A session is one salted worker key.
///
/// A verified session has at least one tests, review or user outcome. It is solved when its
/// last verified outcome passed.
#[derive(Serialize, Debug, Clone, Default, PartialEq)]
pub struct SessionsInsight {
    pub total: u64,
    pub verified: u64,
    pub solved: u64,
    /// Solved share of the verified sessions; `None` with no verified session.
    pub solved_pct: Option<f64>,
    /// Up to three models by the lower bound of their solved rate, among models that led
    /// enough verified sessions. A model leads a session when it wrote the most output tokens.
    pub best_models: Vec<String>,
}
