use serde::Serialize;

/// How usable a model has been over the insights window.
#[derive(Serialize, Debug, Clone, Copy, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum ModelHealth {
    Healthy,
    Degraded,
    Failing,
    InsufficientData,
}
