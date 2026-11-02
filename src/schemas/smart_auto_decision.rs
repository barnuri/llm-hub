use serde::Serialize;

use crate::schemas::work_role::WorkRole;

/// One Smart Auto route: the tier, role, effort and model a request was sent with.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct SmartAutoDecision {
    pub ts_ms: u64,
    pub session: Option<String>,
    pub tier: &'static str,
    pub role: WorkRole,
    pub effort: Option<&'static str>,
    /// The qualified id the route chose first.
    pub model: String,
    pub reason: &'static str,
}
