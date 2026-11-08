use std::cmp::Reverse;
use std::collections::HashMap;
use std::time::Instant;

use crate::schemas::affinity_key::AffinityKey;
use crate::schemas::effort_level::EffortLevel;
use crate::schemas::model_context::strip_1m_suffix;
use crate::schemas::model_descriptor::ModelDescriptor;
use crate::schemas::role_decision::RoleDecision;
use crate::schemas::role_reason::RoleReason;
use crate::schemas::route_decision::RouteDecision;
use crate::schemas::route_reason::RouteReason;
use crate::schemas::smart_auto_policy::SmartAutoPolicy;
use crate::schemas::smart_auto_tier::SmartAutoTier;
use crate::schemas::turn_signals::TurnSignals;
use crate::schemas::work_role::WorkRole;
use crate::services::smart_auto_affinity::SmartAutoAffinity;
use crate::services::smart_auto_effort::SmartAutoEffort;
use crate::services::smart_auto_roles::SmartAutoRoles;
use crate::services::smart_auto_suitability::SmartAutoSuitability;

/// Picks the Smart Auto solver of one turn from roles, affinity, suitability seeds and usage.
/// Pure and in memory: it reads the affinity store and never writes it.
#[derive(Debug)]
pub struct SmartAutoRouter;

impl SmartAutoRouter {
    const USAGE_BITS_PER_POINT: u32 = 8;
    const MAX_USAGE_POINTS: u32 = 2;
    /// A conversation turn this short is an approval that continues the kept model's work.
    const APPROVAL_MAX_WORDS: usize = 3;

    /// Routes one turn over the eligible `pool`. `anchor` holds the affinity store and the
    /// turn's key, `None` without a key. Returns `None` for an empty pool.
    #[must_use]
    pub fn route<'a>(
        pool: &[&'a ModelDescriptor],
        policy: &SmartAutoPolicy,
        signals: &TurnSignals,
        anchor: Option<(&SmartAutoAffinity, &AffinityKey)>,
        usage: &HashMap<String, u64>,
        requested: Option<EffortLevel>,
        now: Instant,
    ) -> Option<RouteDecision<'a>> {
        let tier = policy.tier.unwrap_or_default();
        let (model, turn, reason) = Self::choose(pool, policy, signals, anchor, usage, now)?;
        let effort = SmartAutoEffort::select(
            tier,
            turn.role,
            turn.plan_backed,
            requested,
            &model.effort_levels,
        );
        Some(RouteDecision {
            model,
            role: turn.role,
            plan_backed: turn.plan_backed,
            effort,
            reason,
        })
    }

    fn choose<'a>(
        pool: &[&'a ModelDescriptor],
        policy: &SmartAutoPolicy,
        signals: &TurnSignals,
        anchor: Option<(&SmartAutoAffinity, &AffinityKey)>,
        usage: &HashMap<String, u64>,
        now: Instant,
    ) -> Option<(&'a ModelDescriptor, RoleDecision, RouteReason)> {
        let tier = policy.tier.unwrap_or_default();
        let pin = anchor.and_then(|(affinity, key)| {
            let (model, role, plan_backed) = affinity.lookup(key, pool, now)?;
            Some((Self::find(pool, &model)?, role, plan_backed, affinity, key))
        });
        let Some((pinned, pinned_role, pinned_plan_backed, affinity, key)) = pin else {
            let turn = SmartAutoRoles::classify_user_text(signals, policy.role);
            let incumbent = Self::incumbent(pool, anchor, turn.role, now);
            let (model, reason) =
                Self::pick(pool, tier, turn, incumbent, usage, RouteReason::Fresh)?;
            return Some((model, turn, reason));
        };
        if signals.tool_continuation {
            let turn = RoleDecision {
                role: pinned_role,
                plan_backed: pinned_plan_backed,
                reason: RoleReason::CallerHint,
            };
            return Some((pinned, turn, RouteReason::ToolContinuationReplay));
        }
        let turn = SmartAutoRoles::classify_user_text(signals, policy.role);
        let settled = affinity.soft_switch_allowed(key, signals.user_turns, now);
        let switches = SmartAutoAffinity::should_switch(
            pinned_role,
            pinned_plan_backed,
            turn.role,
            turn.plan_backed,
            signals,
            settled,
        ) || Self::steps_down(pinned_role, pinned_plan_backed, turn, signals, settled);
        if !switches {
            return Some((pinned, turn, RouteReason::Pinned));
        }
        let incumbent = Self::incumbent(pool, anchor, turn.role, now);
        let (model, reason) =
            Self::pick(pool, tier, turn, incumbent, usage, RouteReason::RoleSwitch)?;
        let reason = if std::ptr::eq(model, pinned) {
            RouteReason::Pinned
        } else {
            reason
        };
        Some((model, turn, reason))
    }

    /// Whether a settled heavy pin steps down to the conversation pick at the start of a turn.
    /// Only a plain conversation turn (no role cue) qualifies, and not a short approval such as
    /// "yes, go", which continues the heavy work. A tool continuation never steps down, so a
    /// tool loop keeps its model, its prompt cache and its signed thinking blocks.
    fn steps_down(
        pinned_role: WorkRole,
        pinned_plan_backed: bool,
        turn: RoleDecision,
        signals: &TurnSignals,
        settled: bool,
    ) -> bool {
        let pinned_is_heavy = pinned_role != WorkRole::Conversation
            && !(pinned_role == WorkRole::Implementation && pinned_plan_backed);
        settled
            && !signals.tool_continuation
            && pinned_is_heavy
            && turn.role == WorkRole::Conversation
            && turn.reason == RoleReason::NoCue
            && !Self::is_short_approval(signals)
    }

    /// A user turn of at most `APPROVAL_MAX_WORDS` visible words.
    fn is_short_approval(signals: &TurnSignals) -> bool {
        let text = signals
            .last_user_text
            .as_deref()
            .map(SmartAutoRoles::visible_text)
            .unwrap_or_default();
        text.split_whitespace().count() <= Self::APPROVAL_MAX_WORDS
    }

    fn incumbent<'a>(
        pool: &[&'a ModelDescriptor],
        anchor: Option<(&SmartAutoAffinity, &AffinityKey)>,
        role: WorkRole,
        now: Instant,
    ) -> Option<&'a ModelDescriptor> {
        let (affinity, key) = anchor?;
        Self::find(pool, &affinity.incumbent(key, role, pool, now)?)
    }

    fn pick<'a>(
        pool: &[&'a ModelDescriptor],
        tier: SmartAutoTier,
        turn: RoleDecision,
        incumbent: Option<&'a ModelDescriptor>,
        usage: &HashMap<String, u64>,
        otherwise: RouteReason,
    ) -> Option<(&'a ModelDescriptor, RouteReason)> {
        if let Some(incumbent) = incumbent {
            return Some((incumbent, RouteReason::Incumbent));
        }
        Self::best(pool, tier, turn, usage).map(|model| (model, otherwise))
    }

    /// The highest seed score, then the most usage points, then the lowest qualified id.
    fn best<'a>(
        pool: &[&'a ModelDescriptor],
        tier: SmartAutoTier,
        turn: RoleDecision,
        usage: &HashMap<String, u64>,
    ) -> Option<&'a ModelDescriptor> {
        pool.iter().copied().max_by_key(|model| {
            (
                SmartAutoSuitability::score(&model.qualified_id, turn.role, turn.plan_backed, tier),
                Self::usage_points(model, usage),
                Reverse(model.qualified_id.as_str()),
            )
        })
    }

    /// 0 below 128 requests, 1 below 32,768, else 2: usage separates only near ties.
    fn usage_points(model: &ModelDescriptor, usage: &HashMap<String, u64>) -> u32 {
        let requests = usage
            .get(strip_1m_suffix(&model.qualified_id))
            .copied()
            .unwrap_or(0);
        ((u64::BITS - requests.leading_zeros()) / Self::USAGE_BITS_PER_POINT)
            .min(Self::MAX_USAGE_POINTS)
    }

    fn find<'a>(pool: &[&'a ModelDescriptor], qualified_id: &str) -> Option<&'a ModelDescriptor> {
        pool.iter()
            .copied()
            .find(|model| model.qualified_id == qualified_id)
    }
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use super::*;
    use crate::schemas::evidence_source::EvidenceSource;
    use crate::schemas::limit_evidence::LimitEvidence;
    use crate::schemas::model_descriptor_snapshot::ModelDescriptorSnapshot;
    use crate::schemas::smart_auto_headers::SmartAutoHeaders;
    use crate::services::smart_auto_pool::SmartAutoPool;

    const MESSAGES: &str = "/v1/messages";
    const OPUS: &str = "copilot/claude-opus-4.6";
    const SONNET: &str = "copilot/claude-sonnet-4.6";
    const HAIKU: &str = "copilot/claude-haiku-4.5";
    const LOCAL_CODER: &str = "llama_swap/qwen3-coder-30b";
    const PLANNING_TEXT: &str = "plan the storage layer for the new service";
    const SCOPED_TEXT: &str = "implement task 2 from agent-spec/storage/plan.md";
    const REVIEW_TEXT: &str = "review the diff of the storage layer";

    fn model(qualified_id: &str) -> ModelDescriptor {
        ModelDescriptor::for_test(qualified_id)
    }

    fn models(ids: &[&str]) -> Vec<ModelDescriptor> {
        ids.iter().map(|id| model(id)).collect()
    }

    fn signals(text: &str, user_turns: usize, tool_continuation: bool) -> TurnSignals {
        TurnSignals {
            tool_continuation,
            user_turns,
            last_user_text: Some(text.to_owned()),
            session_id: Some("session-1".to_owned()),
            ..TurnSignals::for_test()
        }
    }

    fn policy(tier: SmartAutoTier) -> SmartAutoPolicy {
        SmartAutoPolicy {
            tier: Some(tier),
            ..SmartAutoPolicy::default()
        }
    }

    fn key(policy: &SmartAutoPolicy) -> AffinityKey {
        AffinityKey::new(
            &signals("", 1, false),
            &SmartAutoHeaders::default(),
            policy,
            MESSAGES,
        )
        .unwrap()
    }

    fn route_fresh<'a>(
        pool: &[&'a ModelDescriptor],
        tier: SmartAutoTier,
        text: &str,
    ) -> RouteDecision<'a> {
        SmartAutoRouter::route(
            pool,
            &policy(tier),
            &signals(text, 1, false),
            None,
            &HashMap::new(),
            None,
            Instant::now(),
        )
        .unwrap()
    }

    fn route_pinned<'a>(
        pool: &[&'a ModelDescriptor],
        affinity: &SmartAutoAffinity,
        turn: &TurnSignals,
        now: Instant,
    ) -> RouteDecision<'a> {
        let policy = policy(SmartAutoTier::Balance);
        let key = key(&policy);
        SmartAutoRouter::route(
            pool,
            &policy,
            turn,
            Some((affinity, &key)),
            &HashMap::new(),
            None,
            now,
        )
        .unwrap()
    }

    fn pinned_store(
        model: &str,
        role: WorkRole,
        plan_backed: bool,
        now: Instant,
    ) -> SmartAutoAffinity {
        let affinity = SmartAutoAffinity::new();
        let key = key(&policy(SmartAutoTier::Balance));
        assert!(affinity.commit(&key, model, role, plan_backed, 1, now));
        affinity
    }

    #[test]
    fn planning_picks_a_strong_model() {
        let owned = models(&[HAIKU, OPUS, SONNET]);
        let pool: Vec<&ModelDescriptor> = owned.iter().collect();
        let decision = route_fresh(&pool, SmartAutoTier::Balance, PLANNING_TEXT);
        assert_eq!(decision.model.qualified_id, OPUS);
        assert_eq!(decision.role, WorkRole::Planning);
        assert_eq!(decision.reason, RouteReason::Fresh);
    }

    #[test]
    fn plan_backed_implementation_picks_a_cheap_model() {
        let owned = models(&[HAIKU, OPUS, SONNET]);
        let pool: Vec<&ModelDescriptor> = owned.iter().collect();
        let decision = route_fresh(&pool, SmartAutoTier::Balance, SCOPED_TEXT);
        assert_eq!(decision.model.qualified_id, HAIKU);
        assert_eq!(decision.role, WorkRole::Implementation);
        assert!(decision.plan_backed);
    }

    #[test]
    fn efficiency_picks_a_cheap_model_across_providers() {
        let owned = models(&[HAIKU, LOCAL_CODER, OPUS]);
        let pool: Vec<&ModelDescriptor> = owned.iter().collect();
        let decision = route_fresh(&pool, SmartAutoTier::Efficiency, SCOPED_TEXT);
        assert_eq!(decision.model.qualified_id, LOCAL_CODER);
    }

    #[test]
    fn tiers_change_the_preference() {
        let owned = models(&[HAIKU, OPUS, SONNET]);
        let pool: Vec<&ModelDescriptor> = owned.iter().collect();
        let pick = |tier| {
            route_fresh(&pool, tier, PLANNING_TEXT)
                .model
                .qualified_id
                .as_str()
        };
        assert_eq!(pick(SmartAutoTier::Intelligence), OPUS);
        assert_eq!(pick(SmartAutoTier::Balance), OPUS);
        assert_eq!(pick(SmartAutoTier::Efficiency), SONNET);
        assert_eq!(pick(SmartAutoTier::Fast), SONNET);
    }

    #[test]
    fn tool_continuation_replays_the_pinned_role_plan_backing_and_effort() {
        let now = Instant::now();
        let affinity = pinned_store(HAIKU, WorkRole::Implementation, true, now);
        let mut haiku = model(HAIKU);
        haiku.effort_levels = vec!["low".to_owned(), "medium".to_owned()];
        let opus = model(OPUS);
        let pool = [&haiku, &opus];
        let decision = route_pinned(&pool, &affinity, &signals(PLANNING_TEXT, 5, true), now);
        assert_eq!(decision.model.qualified_id, HAIKU);
        assert_eq!(decision.role, WorkRole::Implementation);
        assert!(decision.plan_backed);
        assert_eq!(decision.effort, Some(EffortLevel::Low));
        assert_eq!(decision.reason, RouteReason::ToolContinuationReplay);
    }

    #[test]
    fn a_pin_dropped_from_the_pool_is_not_reused() {
        let now = Instant::now();
        let affinity = pinned_store(OPUS, WorkRole::Planning, false, now);
        let owned = models(&[HAIKU, SONNET]);
        let pool: Vec<&ModelDescriptor> = owned.iter().collect();
        let decision = route_pinned(&pool, &affinity, &signals(PLANNING_TEXT, 2, false), now);
        assert_eq!(decision.model.qualified_id, SONNET);
        assert_eq!(decision.reason, RouteReason::Fresh);
    }

    #[test]
    fn a_minor_role_change_keeps_the_pin() {
        let now = Instant::now();
        let affinity = pinned_store(OPUS, WorkRole::Planning, false, now);
        let owned = models(&[HAIKU, OPUS, SONNET]);
        let pool: Vec<&ModelDescriptor> = owned.iter().collect();
        let decision = route_pinned(&pool, &affinity, &signals(REVIEW_TEXT, 6, false), now);
        assert_eq!(decision.model.qualified_id, OPUS);
        assert_eq!(decision.role, WorkRole::Validation);
        assert_eq!(decision.reason, RouteReason::Pinned);
    }

    #[test]
    fn planning_to_plan_backed_implementation_switches_once_settled() {
        let now = Instant::now();
        let cold = now + SmartAutoAffinity::CACHE_WARM_WINDOW + Duration::from_secs(1);
        let affinity = pinned_store(OPUS, WorkRole::Planning, false, now);
        let owned = models(&[HAIKU, OPUS, SONNET]);
        let pool: Vec<&ModelDescriptor> = owned.iter().collect();

        let unsettled = route_pinned(&pool, &affinity, &signals(SCOPED_TEXT, 2, false), cold);
        assert_eq!(unsettled.model.qualified_id, OPUS);
        assert_eq!(unsettled.reason, RouteReason::Pinned);

        let settled = route_pinned(&pool, &affinity, &signals(SCOPED_TEXT, 3, false), cold);
        assert_eq!(settled.model.qualified_id, HAIKU);
        assert_eq!(settled.reason, RouteReason::RoleSwitch);
    }

    #[test]
    fn a_warm_heavy_pin_keeps_its_model_for_a_role_cue() {
        let now = Instant::now();
        let cold = now + SmartAutoAffinity::CACHE_WARM_WINDOW + Duration::from_secs(1);
        let affinity = pinned_store(OPUS, WorkRole::Planning, false, now);
        let owned = models(&[HAIKU, OPUS, SONNET]);
        let pool: Vec<&ModelDescriptor> = owned.iter().collect();

        let warm = route_pinned(&pool, &affinity, &signals(SCOPED_TEXT, 3, false), now);
        assert_eq!(warm.model.qualified_id, OPUS);
        assert_eq!(warm.role, WorkRole::Implementation);
        assert_eq!(warm.reason, RouteReason::Pinned);

        let cold_turn = route_pinned(&pool, &affinity, &signals(SCOPED_TEXT, 3, false), cold);
        assert_eq!(cold_turn.model.qualified_id, HAIKU);
        assert_eq!(cold_turn.reason, RouteReason::RoleSwitch);
    }

    #[test]
    fn returning_to_a_role_reuses_its_incumbent() {
        let now = Instant::now();
        let cold = now + SmartAutoAffinity::CACHE_WARM_WINDOW + Duration::from_secs(1);
        let affinity = pinned_store(OPUS, WorkRole::Planning, false, now);
        let key = key(&policy(SmartAutoTier::Balance));
        assert!(affinity.commit(&key, HAIKU, WorkRole::Implementation, true, 3, now));
        let owned = models(&[HAIKU, OPUS, SONNET]);
        let pool: Vec<&ModelDescriptor> = owned.iter().collect();
        let decision = route_pinned(&pool, &affinity, &signals(PLANNING_TEXT, 5, false), cold);
        assert_eq!(decision.model.qualified_id, OPUS);
        assert_eq!(decision.role, WorkRole::Planning);
        assert_eq!(decision.reason, RouteReason::Incumbent);
    }

    #[test]
    fn a_settled_heavy_pin_steps_down_on_a_plain_conversation_turn() {
        let now = Instant::now();
        let cold = now + SmartAutoAffinity::CACHE_WARM_WINDOW + Duration::from_secs(1);
        let affinity = pinned_store(OPUS, WorkRole::Planning, false, now);
        let owned = models(&[HAIKU, OPUS, SONNET]);
        let pool: Vec<&ModelDescriptor> = owned.iter().collect();
        let chat = "thanks, what does the new storage layer do for us";

        let unsettled = route_pinned(&pool, &affinity, &signals(chat, 2, false), cold);
        assert_eq!(unsettled.model.qualified_id, OPUS);
        assert_eq!(unsettled.reason, RouteReason::Pinned);

        let settled = route_pinned(&pool, &affinity, &signals(chat, 3, false), cold);
        assert_eq!(settled.model.qualified_id, HAIKU);
        assert_eq!(settled.role, WorkRole::Conversation);
        assert_eq!(settled.reason, RouteReason::RoleSwitch);
    }

    #[test]
    fn a_short_approval_keeps_the_heavy_pin() {
        let now = Instant::now();
        let affinity = pinned_store(OPUS, WorkRole::Planning, false, now);
        let owned = models(&[HAIKU, OPUS, SONNET]);
        let pool: Vec<&ModelDescriptor> = owned.iter().collect();
        let decision = route_pinned(&pool, &affinity, &signals("yes, go ahead", 5, false), now);
        assert_eq!(decision.model.qualified_id, OPUS);
        assert_eq!(decision.reason, RouteReason::Pinned);
    }

    #[test]
    fn a_tool_continuation_never_steps_down() {
        let now = Instant::now();
        let affinity = pinned_store(OPUS, WorkRole::Planning, false, now);
        let owned = models(&[HAIKU, OPUS, SONNET]);
        let pool: Vec<&ModelDescriptor> = owned.iter().collect();
        let chat = "thanks, what does the new storage layer do for us";
        let decision = route_pinned(&pool, &affinity, &signals(chat, 5, true), now);
        assert_eq!(decision.model.qualified_id, OPUS);
        assert_eq!(decision.reason, RouteReason::ToolContinuationReplay);
    }

    #[test]
    fn a_cheap_pin_does_not_step_down() {
        let now = Instant::now();
        let affinity = pinned_store(HAIKU, WorkRole::Implementation, true, now);
        let owned = models(&[HAIKU, OPUS, SONNET]);
        let pool: Vec<&ModelDescriptor> = owned.iter().collect();
        let chat = "thanks, what does the new storage layer do for us";
        let decision = route_pinned(&pool, &affinity, &signals(chat, 5, false), now);
        assert_eq!(decision.model.qualified_id, HAIKU);
        assert_eq!(decision.reason, RouteReason::Pinned);
    }

    #[test]
    fn effort_snaps_to_the_published_levels() {
        let mut opus = model(OPUS);
        opus.effort_levels = vec!["low".to_owned(), "high".to_owned()];
        let plain = model(SONNET);
        let decision = route_fresh(&[&opus], SmartAutoTier::Balance, PLANNING_TEXT);
        assert_eq!(decision.effort, Some(EffortLevel::High));
        let decision = route_fresh(&[&plain], SmartAutoTier::Balance, PLANNING_TEXT);
        assert_eq!(decision.effort, None);
        let requested = SmartAutoRouter::route(
            &[&opus],
            &policy(SmartAutoTier::Balance),
            &signals(PLANNING_TEXT, 1, false),
            None,
            &HashMap::new(),
            Some(EffortLevel::Low),
            Instant::now(),
        )
        .unwrap();
        assert_eq!(requested.effort, Some(EffortLevel::Low));
    }

    #[test]
    fn ties_break_by_usage_then_by_qualified_id() {
        let owned = models(&["acme/model-b", "acme/model-a"]);
        let pool: Vec<&ModelDescriptor> = owned.iter().collect();
        let first = route_fresh(&pool, SmartAutoTier::Balance, PLANNING_TEXT);
        let second = route_fresh(&pool, SmartAutoTier::Balance, PLANNING_TEXT);
        assert_eq!(first, second);
        assert_eq!(first.model.qualified_id, "acme/model-a");

        let usage = HashMap::from([("acme/model-b".to_owned(), 300)]);
        let used = SmartAutoRouter::route(
            &pool,
            &policy(SmartAutoTier::Balance),
            &signals(PLANNING_TEXT, 1, false),
            None,
            &usage,
            None,
            Instant::now(),
        )
        .unwrap();
        assert_eq!(used.model.qualified_id, "acme/model-b");
    }

    #[test]
    fn an_empty_pool_routes_nowhere() {
        let decision = SmartAutoRouter::route(
            &[],
            &policy(SmartAutoTier::Balance),
            &signals(PLANNING_TEXT, 1, false),
            None,
            &HashMap::new(),
            None,
            Instant::now(),
        );
        assert_eq!(decision, None);
    }

    #[test]
    fn a_tool_continuation_without_a_pin_reads_the_user_text() {
        let owned = models(&[HAIKU, OPUS, SONNET]);
        let pool: Vec<&ModelDescriptor> = owned.iter().collect();
        let decision = SmartAutoRouter::route(
            &pool,
            &policy(SmartAutoTier::Balance),
            &signals(SCOPED_TEXT, 1, true),
            None,
            &HashMap::new(),
            None,
            Instant::now(),
        )
        .unwrap();
        assert_eq!(decision.model.qualified_id, HAIKU);
        assert_eq!(decision.reason, RouteReason::Fresh);
    }

    /// Models in the timed catalog, the size of a large gateway's chat list.
    const CATALOG_MODELS: usize = 50;
    /// Provider profiles the generated catalog models are spread over.
    const CATALOG_PROFILES: usize = 5;
    /// Selections timed in one run.
    const TIMED_SELECTIONS: usize = 2_000;
    /// Context window of every timed model: the long-context fixture.
    const LONG_CONTEXT_TOKENS: u64 = 1_000_000;
    /// Estimated prompt size of the timed request, in tokens.
    const LONG_PROMPT_TOKENS: u64 = 200_000;
    /// Repetitions of the planning sentence that fill most of the 8 KiB text bound.
    const LONG_TEXT_REPEATS: usize = 170;
    const LIMIT_CONFIDENCE: f32 = 0.9;
    const P95_PERCENT: usize = 95;
    /// Target p95 selection time, printed next to the measured value.
    const P95_TARGET: Duration = Duration::from_millis(1);
    /// Asserted p95 bound, loose enough for shared CI runners.
    const P95_CI_BOUND: Duration = Duration::from_millis(10);

    /// The timed catalog: the known models first, then generated ones spread over profiles.
    fn catalog() -> Vec<ModelDescriptor> {
        let known = [OPUS, SONNET, HAIKU];
        (0..CATALOG_MODELS)
            .map(|index| {
                let id = known.get(index).map_or_else(
                    || format!("provider{}/model-{index}", index % CATALOG_PROFILES),
                    |id| (*id).to_owned(),
                );
                ModelDescriptor {
                    context: Some(LimitEvidence::new(
                        LONG_CONTEXT_TOKENS,
                        EvidenceSource::Listed,
                        "bench",
                        LIMIT_CONFIDENCE,
                    )),
                    endpoints: vec![MESSAGES.to_owned()],
                    ..ModelDescriptor::for_test(&id)
                }
            })
            .collect()
    }

    #[test]
    fn selection_p95_stays_under_the_ci_bound_on_a_long_context_catalog() {
        let snapshot = ModelDescriptorSnapshot::new(catalog());
        let timed_policy = SmartAutoPolicy {
            min_context_tokens: Some(LONG_CONTEXT_TOKENS),
            ..policy(SmartAutoTier::Balance)
        };
        let key = key(&policy(SmartAutoTier::Balance));
        let affinity = pinned_store(OPUS, WorkRole::Planning, false, Instant::now());
        let turn = TurnSignals {
            estimated_prompt_tokens: LONG_PROMPT_TOKENS,
            last_user_text: Some(format!("{PLANNING_TEXT}. ").repeat(LONG_TEXT_REPEATS)),
            user_turns: 3,
            ..TurnSignals::for_test()
        };
        let usage = HashMap::new();
        let mut timings = Vec::with_capacity(TIMED_SELECTIONS);

        for _ in 0..TIMED_SELECTIONS {
            let started = Instant::now();
            let pool =
                SmartAutoPool::eligible(&snapshot, &timed_policy, MESSAGES, LONG_PROMPT_TOKENS);
            let decision = SmartAutoRouter::route(
                &pool,
                &timed_policy,
                &turn,
                Some((&affinity, &key)),
                &usage,
                None,
                started,
            );
            timings.push(started.elapsed());
            assert!(decision.is_some());
        }

        timings.sort_unstable();
        let p95 = timings[(timings.len() * P95_PERCENT).div_ceil(100) - 1];
        println!("selection p95 over {TIMED_SELECTIONS} runs: {p95:?} (target {P95_TARGET:?})");
        assert!(
            p95 < P95_CI_BOUND,
            "p95 selection took {p95:?}, over the CI bound {P95_CI_BOUND:?}"
        );
    }
}
