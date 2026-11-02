use crate::schemas::effort_level::EffortLevel;
use crate::schemas::model_descriptor::ModelDescriptor;
use crate::schemas::route_reason::RouteReason;
use crate::schemas::work_role::WorkRole;

/// One Smart Auto routing decision. The proxy commits it to affinity only on success.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct RouteDecision<'a> {
    /// The concrete model to call.
    pub model: &'a ModelDescriptor,
    /// The work role of the turn.
    pub role: WorkRole,
    /// The user text references a plan, a tasks file or a skill invocation.
    pub plan_backed: bool,
    /// The effort to send. `None` when the model publishes no selectable level.
    pub effort: Option<EffortLevel>,
    /// Why the router chose this model.
    pub reason: RouteReason,
}
