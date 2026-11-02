use crate::schemas::role_reason::RoleReason;
use crate::schemas::role_source::RoleSource;
use crate::schemas::work_role::WorkRole;

/// The work role of one user turn, and whether a plan or skill backs it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct RoleDecision {
    /// The classified role.
    pub role: WorkRole,
    /// The user text references a plan, a tasks file or a skill invocation.
    pub plan_backed: bool,
    /// The rule that chose the role.
    pub reason: RoleReason,
}

#[allow(dead_code)] // Only the tests read role decisions.
impl RoleDecision {
    /// The kind of input that chose the role, derived from the reason.
    #[must_use]
    pub fn source(self) -> RoleSource {
        match self.reason {
            RoleReason::CallerHint => RoleSource::Hint,
            RoleReason::NoUserText | RoleReason::NoCue => RoleSource::Default,
            RoleReason::PlanningCue
            | RoleReason::InvestigationCue
            | RoleReason::ImplementationCue
            | RoleReason::ValidationCue
            | RoleReason::PlanMarker => RoleSource::Rule,
        }
    }
}
