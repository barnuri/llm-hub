use crate::schemas::model_descriptor_snapshot::ModelDescriptorSnapshot;
use crate::schemas::smart_auto_policy::SmartAutoPolicy;
use crate::schemas::smart_auto_tier::SmartAutoTier;
use crate::schemas::work_role::WorkRole;

/// What the planner needs about one routed request.
pub struct ReviewRequest<'a> {
    pub tier: SmartAutoTier,
    pub role: WorkRole,
    /// The solver was picked from a plan, so a simple implementation needs no review.
    pub plan_backed: bool,
    /// Qualified id of the routed solver.
    pub solver: &'a str,
    /// The caller's hard filters, which reviewers must also satisfy.
    pub policy: &'a SmartAutoPolicy,
    pub snapshot: &'a ModelDescriptorSnapshot,
    /// The client route, such as `/chat/completions`.
    pub endpoint: &'a str,
    /// Estimated tokens of the evidence a reviewer reads: the task plus the draft.
    pub evidence_tokens: u64,
    /// The caller's profile filter over qualified ids; a reviewer outside it is never picked.
    pub admits: &'a dyn Fn(&str) -> bool,
}

// Prints every field but `admits`, a closure that has no `Debug`.
impl std::fmt::Debug for ReviewRequest<'_> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ReviewRequest")
            .field("tier", &self.tier)
            .field("role", &self.role)
            .field("plan_backed", &self.plan_backed)
            .field("solver", &self.solver)
            .field("policy", &self.policy)
            .field("snapshot", &self.snapshot)
            .field("endpoint", &self.endpoint)
            .field("evidence_tokens", &self.evidence_tokens)
            .finish_non_exhaustive()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn admit_all(_: &str) -> bool {
        true
    }

    #[test]
    fn debug_prints_every_field_but_the_admits_closure() {
        let policy = SmartAutoPolicy::default();
        let snapshot = ModelDescriptorSnapshot::default();
        let request = ReviewRequest {
            tier: SmartAutoTier::Intelligence,
            role: WorkRole::Implementation,
            plan_backed: true,
            solver: "copilot/claude-opus-5-5",
            policy: &policy,
            snapshot: &snapshot,
            endpoint: "/chat/completions",
            evidence_tokens: 1_234,
            admits: &admit_all,
        };

        let printed = format!("{request:?}");

        assert!(printed.starts_with(
            "ReviewRequest { tier: Intelligence, role: Implementation, plan_backed: true, "
        ));
        assert!(printed.contains("solver: \"copilot/claude-opus-5-5\""));
        assert!(printed.contains("endpoint: \"/chat/completions\""));
        assert!(printed.contains("evidence_tokens: 1234"));
        assert!(printed.ends_with(", .. }"));
        assert!(!printed.contains("admits"));
    }
}
