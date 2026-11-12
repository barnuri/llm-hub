use std::cmp::Reverse;
use std::collections::HashMap;
use std::time::Instant;

use crate::schemas::affinity_key::AffinityKey;
use crate::schemas::effort_level::EffortLevel;
use crate::schemas::live_signals::LiveSignals;
use crate::schemas::model_context::strip_1m_suffix;
use crate::schemas::model_descriptor::ModelDescriptor;
use crate::schemas::pin_miss::PinMiss;
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
    #[allow(clippy::too_many_arguments)] // each input is a separate per-request value; a bundle would only rename them
    pub fn route<'a>(
        pool: &[&'a ModelDescriptor],
        policy: &SmartAutoPolicy,
        signals: &TurnSignals,
        anchor: Option<(&SmartAutoAffinity, &AffinityKey)>,
        usage: &HashMap<String, u64>,
        live: &LiveSignals,
        requested: Option<EffortLevel>,
        now: Instant,
    ) -> Option<RouteDecision<'a>> {
        let tier = policy.tier.unwrap_or_default();
        let (model, turn, reason, pin_miss) =
            Self::choose(pool, policy, signals, anchor, usage, live, now)?;
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
            pin_miss,
        })
    }

    fn choose<'a>(
        pool: &[&'a ModelDescriptor],
        policy: &SmartAutoPolicy,
        signals: &TurnSignals,
        anchor: Option<(&SmartAutoAffinity, &AffinityKey)>,
        usage: &HashMap<String, u64>,
        live: &LiveSignals,
        now: Instant,
    ) -> Option<(
        &'a ModelDescriptor,
        RoleDecision,
        RouteReason,
        Option<PinMiss>,
    )> {
        let tier = policy.tier.unwrap_or_default();
        let pin = match anchor {
            None => Err(PinMiss::NoKey),
            Some((affinity, key)) => {
                affinity
                    .lookup(key, pool, now)
                    .and_then(|(model, role, plan_backed)| {
                        let pinned = Self::find(pool, &model).ok_or(PinMiss::LeftPool)?;
                        Ok((pinned, role, plan_backed, affinity, key))
                    })
            }
        };
        let (pinned, pinned_role, pinned_plan_backed, affinity, key) = match pin {
            Ok(found) => found,
            Err(miss) => {
                let turn = SmartAutoRoles::classify_user_text(signals, policy.role);
                let incumbent = Self::incumbent(pool, anchor, turn.role, now);
                let (model, reason) =
                    Self::pick(pool, tier, turn, incumbent, usage, live, RouteReason::Fresh)?;
                let pin_miss = (reason == RouteReason::Fresh).then_some(miss);
                return Some((model, turn, reason, pin_miss));
            }
        };
        if signals.tool_continuation {
            let turn = RoleDecision {
                role: pinned_role,
                plan_backed: pinned_plan_backed,
                reason: RoleReason::CallerHint,
            };
            return Some((pinned, turn, RouteReason::ToolContinuationReplay, None));
        }
        let turn = SmartAutoRoles::classify_user_text(signals, policy.role);
        let settled = affinity.soft_switch_allowed(key, signals.user_turns, now);
        let switches =
            SmartAutoAffinity::should_switch(
                pinned_role,
                pinned_plan_backed,
                turn.role,
                turn.plan_backed,
                signals,
                settled,
            ) || Self::steps_down(pinned_role, pinned_plan_backed, turn, signals, settled);
        if !switches {
            return Some((pinned, turn, RouteReason::Pinned, None));
        }
        let incumbent = Self::incumbent(pool, anchor, turn.role, now);
        let (model, reason) = Self::pick(
            pool,
            tier,
            turn,
            incumbent,
            usage,
            live,
            RouteReason::RoleSwitch,
        )?;
        let reason = if std::ptr::eq(model, pinned) {
            RouteReason::Pinned
        } else {
            reason
        };
        Some((model, turn, reason, None))
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
        live: &LiveSignals,
        otherwise: RouteReason,
    ) -> Option<(&'a ModelDescriptor, RouteReason)> {
        if let Some(incumbent) = incumbent {
            return Some((incumbent, RouteReason::Incumbent));
        }
        Self::best(pool, tier, turn, usage, live).map(|model| (model, otherwise))
    }

    /// The models to try after the pick fails, best first, by the same score as the pick.
    /// `taken` (the pick and any explicit fallbacks) is skipped. The first one comes from another
    /// profile when the pool has one, because a gateway that hangs usually hangs for all of its
    /// models. At most `limit` come back.
    #[must_use]
    pub fn runners_up(
        pool: &[&ModelDescriptor],
        tier: SmartAutoTier,
        turn: RoleDecision,
        taken: &[String],
        usage: &HashMap<String, u64>,
        live: &LiveSignals,
        limit: usize,
    ) -> Vec<String> {
        let taken: Vec<&str> = taken.iter().map(|id| strip_1m_suffix(id)).collect();
        let mut ranked: Vec<&ModelDescriptor> = pool
            .iter()
            .copied()
            .filter(|model| !taken.contains(&strip_1m_suffix(&model.qualified_id)))
            .collect();
        ranked.sort_by_key(|model| {
            Reverse((
                SmartAutoSuitability::score(
                    &model.qualified_id,
                    turn.role,
                    turn.plan_backed,
                    tier,
                    live.get(&model.qualified_id),
                ),
                Self::usage_points(model, usage),
                Reverse(model.qualified_id.as_str()),
            ))
        });
        let picked_profile = pool
            .iter()
            .find(|model| taken.first() == Some(&strip_1m_suffix(&model.qualified_id)))
            .map(|model| model.profile.as_str());
        if let Some(other) = ranked
            .iter()
            .position(|model| Some(model.profile.as_str()) != picked_profile)
        {
            let first = ranked.remove(other);
            ranked.insert(0, first);
        }
        ranked
            .into_iter()
            .take(limit)
            .map(|model| model.qualified_id.clone())
            .collect()
    }

    /// The highest seed score, then the most usage points, then the lowest qualified id.
    fn best<'a>(
        pool: &[&'a ModelDescriptor],
        tier: SmartAutoTier,
        turn: RoleDecision,
        usage: &HashMap<String, u64>,
        live: &LiveSignals,
    ) -> Option<&'a ModelDescriptor> {
        pool.iter().copied().max_by_key(|model| {
            (
                SmartAutoSuitability::score(
                    &model.qualified_id,
                    turn.role,
                    turn.plan_backed,
                    tier,
                    live.get(&model.qualified_id),
                ),
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
    use crate::schemas::live_signals::ModelSignal;
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
            &LiveSignals::default(),
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
            &LiveSignals::default(),
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
            &LiveSignals::default(),
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
            &LiveSignals::default(),
            None,
            Instant::now(),
        )
        .unwrap();
        assert_eq!(used.model.qualified_id, "acme/model-b");
    }

    fn route_live(pool: &[&ModelDescriptor], tier: SmartAutoTier, live: &LiveSignals) -> String {
        SmartAutoRouter::route(
            pool,
            &policy(tier),
            &signals(PLANNING_TEXT, 1, false),
            None,
            &HashMap::new(),
            live,
            None,
            Instant::now(),
        )
        .unwrap()
        .model
        .qualified_id
        .clone()
    }

    fn live(rows: &[(&str, ModelSignal)]) -> LiveSignals {
        LiveSignals::new(
            rows.iter()
                .map(|(id, signal)| ((*id).to_owned(), *signal))
                .collect(),
        )
    }

    fn planning_turn() -> RoleDecision {
        RoleDecision {
            role: WorkRole::Planning,
            plan_backed: false,
            reason: RoleReason::NoCue,
        }
    }

    fn runners(
        pool: &[&ModelDescriptor],
        taken: &[&str],
        live: &LiveSignals,
        limit: usize,
    ) -> Vec<String> {
        let taken: Vec<String> = taken.iter().map(|id| (*id).to_owned()).collect();
        SmartAutoRouter::runners_up(
            pool,
            SmartAutoTier::Balance,
            planning_turn(),
            &taken,
            &HashMap::new(),
            live,
            limit,
        )
    }

    #[test]
    fn runners_up_skip_the_pick_and_follow_the_score() {
        let owned = models(&["acme/model-a", "acme/model-b", "acme/model-c"]);
        let pool: Vec<&ModelDescriptor> = owned.iter().collect();
        let flaky = ModelSignal {
            error_rate: Some(0.5),
            ..ModelSignal::default()
        };
        let signals = live(&[("acme/model-b", flaky)]);

        let next = runners(&pool, &["acme/model-a"], &signals, 5);

        assert_eq!(next, ["acme/model-c", "acme/model-b"]);
    }

    #[test]
    fn the_first_runner_up_comes_from_another_profile_when_there_is_one() {
        let owned = models(&["acme/model-a", "acme/model-b", "beta/model-z"]);
        let pool: Vec<&ModelDescriptor> = owned.iter().collect();

        let next = runners(&pool, &["acme/model-a"], &LiveSignals::default(), 2);

        assert_eq!(next, ["beta/model-z", "acme/model-b"]);
    }

    #[test]
    fn runners_up_are_capped_and_skip_explicit_fallbacks() {
        let owned = models(&[
            "acme/model-a",
            "acme/model-b",
            "acme/model-c",
            "acme/model-d",
        ]);
        let pool: Vec<&ModelDescriptor> = owned.iter().collect();

        let next = runners(
            &pool,
            &["acme/model-a", "acme/model-b[1m]"],
            &LiveSignals::default(),
            1,
        );

        assert_eq!(next, ["acme/model-c"]);
        assert!(runners(&pool[..1], &["acme/model-a"], &LiveSignals::default(), 2).is_empty());
    }

    #[test]
    fn a_failing_model_loses_to_an_equal_one() {
        let owned = models(&["acme/model-a", "acme/model-b"]);
        let pool: Vec<&ModelDescriptor> = owned.iter().collect();
        let flaky = ModelSignal {
            error_rate: Some(0.3),
            ..ModelSignal::default()
        };
        let signals = live(&[("acme/model-a", flaky)]);
        assert_eq!(
            route_live(&pool, SmartAutoTier::Balance, &signals),
            "acme/model-b"
        );
    }

    #[test]
    fn a_nearly_empty_quota_moves_the_pick_to_another_profile() {
        let owned = models(&["acme/model-a", "beta/model-a"]);
        let pool: Vec<&ModelDescriptor> = owned.iter().collect();
        let low = ModelSignal {
            quota_remaining_pct: Some(5.0),
            ..ModelSignal::default()
        };
        let signals = live(&[("acme/model-a", low)]);
        assert_eq!(
            route_live(&pool, SmartAutoTier::Balance, &signals),
            "beta/model-a"
        );
    }

    #[test]
    fn the_fast_tier_follows_measured_speed() {
        let owned = models(&["acme/model-a", "acme/model-b"]);
        let pool: Vec<&ModelDescriptor> = owned.iter().collect();
        let quick = ModelSignal {
            speed: Some(10),
            ..ModelSignal::default()
        };
        let slow = ModelSignal {
            speed: Some(0),
            ..ModelSignal::default()
        };
        let signals = live(&[("acme/model-a", slow), ("acme/model-b", quick)]);
        assert_eq!(
            route_live(&pool, SmartAutoTier::Fast, &signals),
            "acme/model-b"
        );
    }

    #[test]
    fn the_efficiency_tier_follows_measured_cost() {
        let owned = models(&["acme/model-a", "acme/model-b"]);
        let pool: Vec<&ModelDescriptor> = owned.iter().collect();
        let dear = ModelSignal {
            cost: Some(1),
            ..ModelSignal::default()
        };
        let cheap = ModelSignal {
            cost: Some(9),
            ..ModelSignal::default()
        };
        let signals = live(&[("acme/model-a", dear), ("acme/model-b", cheap)]);
        assert_eq!(
            route_live(&pool, SmartAutoTier::Efficiency, &signals),
            "acme/model-b"
        );
    }

    #[test]
    fn an_empty_pool_routes_nowhere() {
        let decision = SmartAutoRouter::route(
            &[],
            &policy(SmartAutoTier::Balance),
            &signals(PLANNING_TEXT, 1, false),
            None,
            &HashMap::new(),
            &LiveSignals::default(),
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
            &LiveSignals::default(),
            None,
            Instant::now(),
        )
        .unwrap();
        assert_eq!(decision.model.qualified_id, HAIKU);
        assert_eq!(decision.reason, RouteReason::Fresh);
    }

    #[test]
    fn a_turn_without_an_affinity_key_routes_fresh_for_no_key() {
        let owned = models(&[HAIKU, OPUS, SONNET]);
        let pool: Vec<&ModelDescriptor> = owned.iter().collect();

        let decision = route_fresh(&pool, SmartAutoTier::Balance, PLANNING_TEXT);

        assert_eq!(decision.reason, RouteReason::Fresh);
        assert_eq!(decision.pin_miss, Some(PinMiss::NoKey));
    }

    #[test]
    fn a_keyed_turn_without_a_pin_routes_fresh_for_no_pin() {
        let owned = models(&[HAIKU, OPUS, SONNET]);
        let pool: Vec<&ModelDescriptor> = owned.iter().collect();
        let affinity = SmartAutoAffinity::new();

        let decision = route_pinned(
            &pool,
            &affinity,
            &signals(PLANNING_TEXT, 1, false),
            Instant::now(),
        );

        assert_eq!(decision.reason, RouteReason::Fresh);
        assert_eq!(decision.pin_miss, Some(PinMiss::NoPin));
    }

    #[test]
    fn a_pin_idle_past_its_limit_routes_fresh_for_idle_expired() {
        let owned = models(&[HAIKU, OPUS, SONNET]);
        let pool: Vec<&ModelDescriptor> = owned.iter().collect();
        let start = Instant::now();
        let affinity = pinned_store(OPUS, WorkRole::Planning, false, start);
        let expired = start + Duration::from_mins(10) + Duration::from_secs(1);

        let decision = route_pinned(&pool, &affinity, &signals(PLANNING_TEXT, 5, false), expired);

        assert_eq!(decision.reason, RouteReason::Fresh);
        assert_eq!(decision.pin_miss, Some(PinMiss::IdleExpired));
    }

    #[test]
    fn a_pin_whose_model_left_the_pool_routes_fresh_for_left_pool() {
        let owned = models(&[HAIKU, SONNET]);
        let pool: Vec<&ModelDescriptor> = owned.iter().collect();
        let now = Instant::now();
        let affinity = pinned_store(OPUS, WorkRole::Planning, false, now);

        let decision = route_pinned(&pool, &affinity, &signals(PLANNING_TEXT, 5, false), now);

        assert_eq!(decision.reason, RouteReason::Fresh);
        assert_eq!(decision.pin_miss, Some(PinMiss::LeftPool));
    }

    #[test]
    fn a_pinned_turn_has_no_pin_miss() {
        let owned = models(&[HAIKU, OPUS, SONNET]);
        let pool: Vec<&ModelDescriptor> = owned.iter().collect();
        let now = Instant::now();
        let affinity = pinned_store(OPUS, WorkRole::Planning, false, now);

        let decision = route_pinned(&pool, &affinity, &signals("yes, go ahead", 5, false), now);

        assert_eq!(decision.reason, RouteReason::Pinned);
        assert_eq!(decision.pin_miss, None);
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
                &LiveSignals::default(),
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
