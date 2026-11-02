use crate::schemas::model_descriptor::ModelDescriptor;
use crate::schemas::model_id::ModelId;
use crate::schemas::review_request::ReviewRequest;
use crate::schemas::selection_stage::SelectionStage;
use crate::schemas::smart_auto_tier::SmartAutoTier;
use crate::schemas::stage_budget::StageBudget;
use crate::schemas::work_role::WorkRole;
use crate::services::smart_auto_pool::SmartAutoPool;

/// Who reviews one draft, and how many repairs may follow, planned before the solver runs.
#[derive(Debug, Clone, PartialEq)]
pub struct ReviewGraph {
    /// Reviewers in call order, each a concrete model other than the solver.
    pub reviewers: Vec<ModelDescriptor>,
    /// The stage the reviewers run as: a judge for Balance, critics for Intelligence.
    pub reviewer_stage: SelectionStage,
    /// True when a non-accepting verdict may trigger one repair call.
    pub repair: bool,
    /// True when a reviewer sits on a single-resident local profile, so reviewer calls run one at a time.
    pub serialized: bool,
    /// The limits the review runs under, solver call included.
    pub budget: StageBudget,
}

impl ReviewGraph {
    /// Client routes whose answers are reviewed; a Responses caller is streamed unreviewed.
    pub const REVIEWED_ENDPOINTS: [&'static str; 3] =
        ["/v1/chat/completions", "/chat/completions", "/v1/messages"];
    /// Judges a Balance review may use.
    pub const BALANCE_REVIEWERS: usize = 1;
    /// Critics an Intelligence review may use.
    pub const INTELLIGENCE_REVIEWERS: usize = 2;

    /// Plans the review of one request, or `None` when the request is served as a single stream.
    /// Reviewers come from the pool under the caller's filters, prefer another profile, and must fit the evidence.
    #[must_use]
    pub fn plan(
        request: &ReviewRequest<'_>,
        is_single_resident: &dyn Fn(&str) -> bool,
    ) -> Option<ReviewGraph> {
        let budget = StageBudget::for_tier(request.tier)?;
        let reviewer_stage = match request.tier {
            SmartAutoTier::Intelligence => SelectionStage::Critic,
            _ => SelectionStage::Judge,
        };
        let reviewed_route = Self::REVIEWED_ENDPOINTS.contains(&request.endpoint);
        if !reviewed_route || (request.plan_backed && request.role == WorkRole::Implementation) {
            return None;
        }
        let wanted = match request.tier {
            SmartAutoTier::Balance => Self::BALANCE_REVIEWERS,
            SmartAutoTier::Intelligence => Self::INTELLIGENCE_REVIEWERS,
            SmartAutoTier::Efficiency | SmartAutoTier::Fast => return None,
        };
        let solver_profile = ModelId::parse(request.solver).map(|id| id.profile);
        let mut candidates: Vec<&ModelDescriptor> = SmartAutoPool::eligible(
            request.snapshot,
            request.policy,
            request.endpoint,
            request.evidence_tokens,
        )
        .into_iter()
        .filter(|descriptor| descriptor.qualified_id != request.solver)
        .filter(|descriptor| (request.admits)(&descriptor.qualified_id))
        .collect();
        // `false` sorts first, so reviewers from another profile are preferred.
        candidates.sort_by_key(|descriptor| {
            Some(descriptor.profile.as_str()) == solver_profile.as_deref()
        });
        let reviewers: Vec<ModelDescriptor> =
            candidates.into_iter().take(wanted).cloned().collect();
        if reviewers.is_empty() {
            return None;
        }
        let serialized = reviewers
            .iter()
            .any(|reviewer| is_single_resident(&reviewer.profile));
        Some(ReviewGraph {
            reviewers,
            reviewer_stage,
            repair: true,
            serialized,
            budget,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::schemas::evidence_source::EvidenceSource;
    use crate::schemas::limit_evidence::LimitEvidence;
    use crate::schemas::model_descriptor_snapshot::ModelDescriptorSnapshot;
    use crate::schemas::smart_auto_policy::SmartAutoPolicy;

    const SOLVER: &str = "copilot/solver";
    const ENDPOINT: &str = "/v1/chat/completions";

    fn model(profile: &str, name: &str, context: u64) -> ModelDescriptor {
        ModelDescriptor {
            endpoints: vec![ENDPOINT.to_owned()],
            context: Some(LimitEvidence::new(
                context,
                EvidenceSource::Listed,
                profile,
                0.6,
            )),
            ..ModelDescriptor::for_test(&format!("{profile}/{name}"))
        }
    }

    fn snapshot(models: Vec<ModelDescriptor>) -> ModelDescriptorSnapshot {
        let mut all = vec![model("copilot", "solver", 1_000_000)];
        all.extend(models);
        ModelDescriptorSnapshot::new(all)
    }

    fn request<'a>(
        tier: SmartAutoTier,
        snapshot: &'a ModelDescriptorSnapshot,
        policy: &'a SmartAutoPolicy,
    ) -> ReviewRequest<'a> {
        ReviewRequest {
            tier,
            role: WorkRole::Planning,
            plan_backed: false,
            solver: SOLVER,
            policy,
            snapshot,
            endpoint: ENDPOINT,
            evidence_tokens: 1_000,
            admits: &allow_all,
        }
    }

    fn never_resident(_: &str) -> bool {
        false
    }

    fn allow_all(_: &str) -> bool {
        true
    }

    #[test]
    fn efficiency_and_fast_get_no_review() {
        let snap = snapshot(vec![model("copilot", "judge", 200_000)]);
        let policy = SmartAutoPolicy::default();
        for tier in [SmartAutoTier::Efficiency, SmartAutoTier::Fast] {
            assert!(ReviewGraph::plan(&request(tier, &snap, &policy), &never_resident).is_none());
        }
    }

    #[test]
    fn plan_backed_simple_implementation_gets_no_review() {
        let snap = snapshot(vec![model("copilot", "judge", 200_000)]);
        let policy = SmartAutoPolicy::default();
        let mut req = request(SmartAutoTier::Balance, &snap, &policy);
        req.role = WorkRole::Implementation;
        req.plan_backed = true;
        assert!(ReviewGraph::plan(&req, &never_resident).is_none());
        req.plan_backed = false;
        assert!(ReviewGraph::plan(&req, &never_resident).is_some());
    }

    #[test]
    fn unreviewed_routes_get_no_review() {
        let snap = snapshot(vec![model("copilot", "judge", 200_000)]);
        let policy = SmartAutoPolicy::default();
        let mut req = request(SmartAutoTier::Balance, &snap, &policy);
        req.endpoint = "/responses";
        assert!(ReviewGraph::plan(&req, &never_resident).is_none());
    }

    #[test]
    fn balance_takes_one_judge_and_intelligence_two_critics() {
        let snap = snapshot(vec![
            model("copilot", "judge-a", 200_000),
            model("copilot", "judge-b", 200_000),
            model("local", "judge-c", 200_000),
        ]);
        let policy = SmartAutoPolicy::default();
        let balance = ReviewGraph::plan(
            &request(SmartAutoTier::Balance, &snap, &policy),
            &never_resident,
        )
        .expect("balance reviews");
        assert_eq!(balance.reviewers.len(), 1);
        assert!(balance.repair);
        assert_eq!(balance.budget.calls_left(), StageBudget::BALANCE_MAX_CALLS);

        let intelligence = ReviewGraph::plan(
            &request(SmartAutoTier::Intelligence, &snap, &policy),
            &never_resident,
        )
        .expect("intelligence reviews");
        assert_eq!(intelligence.reviewers.len(), 2);
        assert_ne!(intelligence.reviewers[0].qualified_id, SOLVER);
        assert_eq!(intelligence.reviewers[0].profile, "local");
    }

    #[test]
    fn the_solver_never_reviews_itself() {
        let snap = ModelDescriptorSnapshot::new(vec![model("copilot", "solver", 200_000)]);
        let policy = SmartAutoPolicy::default();
        assert!(
            ReviewGraph::plan(
                &request(SmartAutoTier::Balance, &snap, &policy),
                &never_resident
            )
            .is_none()
        );
    }

    #[test]
    fn a_one_million_minimum_excludes_small_judges() {
        let snap = snapshot(vec![
            model("copilot", "small", 200_000),
            model("copilot", "large", 1_000_000),
        ]);
        let policy = SmartAutoPolicy {
            min_context_tokens: Some(1_000_000),
            ..SmartAutoPolicy::default()
        };
        let graph = ReviewGraph::plan(
            &request(SmartAutoTier::Balance, &snap, &policy),
            &never_resident,
        )
        .expect("the large judge reviews");
        assert_eq!(graph.reviewers[0].qualified_id, "copilot/large");

        let only_small = snapshot(vec![model("copilot", "small", 200_000)]);
        assert!(
            ReviewGraph::plan(
                &request(SmartAutoTier::Balance, &only_small, &policy),
                &never_resident
            )
            .is_none()
        );
    }

    #[test]
    fn a_judge_that_cannot_fit_the_evidence_is_rejected() {
        let snap = snapshot(vec![model("copilot", "small", 8_000)]);
        let policy = SmartAutoPolicy::default();
        let mut req = request(SmartAutoTier::Balance, &snap, &policy);
        req.evidence_tokens = 50_000;
        assert!(ReviewGraph::plan(&req, &never_resident).is_none());
    }

    #[test]
    fn a_single_resident_reviewer_serializes_its_stages() {
        let snap = snapshot(vec![model("llama", "judge", 200_000)]);
        let policy = SmartAutoPolicy::default();
        let single_resident = |profile: &str| profile == "llama";
        let graph = ReviewGraph::plan(
            &request(SmartAutoTier::Balance, &snap, &policy),
            &single_resident,
        )
        .expect("reviews");
        assert!(graph.serialized);
    }
}
