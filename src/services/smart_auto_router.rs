use std::cmp::Reverse;
use std::collections::HashMap;
use std::time::Instant;

use crate::schemas::affinity_key::AffinityKey;
use crate::schemas::directive_outcome::DirectiveOutcome;
use crate::schemas::effort_level::EffortLevel;
use crate::schemas::live_signals::LiveSignals;
use crate::schemas::model_context::strip_window_suffix;
use crate::schemas::model_descriptor::ModelDescriptor;
use crate::schemas::model_directive::ModelDirective;
use crate::schemas::pin_miss::PinMiss;
use crate::schemas::ranked_model::RankedModel;
use crate::schemas::role_decision::RoleDecision;
use crate::schemas::role_reason::RoleReason;
use crate::schemas::route_decision::RouteDecision;
use crate::schemas::route_reason::RouteReason;
use crate::schemas::selection_why::SelectionWhy;
use crate::schemas::smart_auto_policy::SmartAutoPolicy;
use crate::schemas::smart_auto_tier::SmartAutoTier;
use crate::schemas::turn_signals::TurnSignals;
use crate::schemas::work_role::WorkRole;
use crate::services::smart_auto_affinity::{SmartAutoAffinity, StepDownCost};
use crate::services::smart_auto_directive::SmartAutoDirective;
use crate::services::smart_auto_effort::SmartAutoEffort;
use crate::services::smart_auto_live::SmartAutoLive;
use crate::services::smart_auto_roles::SmartAutoRoles;
use crate::services::smart_auto_suitability::SmartAutoSuitability;

/// Picks the Smart Auto solver of one turn from roles, affinity, suitability seeds and usage.
/// Pure and in memory: it reads the affinity store and never writes it.
#[derive(Debug)]
pub struct SmartAutoRouter;

/// One routing choice before effort and the selection reason are added.
struct Choice<'a> {
    model: &'a ModelDescriptor,
    turn: RoleDecision,
    reason: RouteReason,
    pin_miss: Option<PinMiss>,
    directive: Option<DirectiveOutcome>,
    user_pin: bool,
}

impl<'a> Choice<'a> {
    fn routed(
        model: &'a ModelDescriptor,
        turn: RoleDecision,
        reason: RouteReason,
        pin_miss: Option<PinMiss>,
    ) -> Choice<'a> {
        Choice {
            model,
            turn,
            reason,
            pin_miss,
            directive: None,
            user_pin: false,
        }
    }
}

impl SmartAutoRouter {
    const USAGE_BITS_PER_POINT: u32 = 8;
    const MAX_USAGE_POINTS: u32 = 2;
    /// A conversation turn this short is an approval that continues the kept model's work.
    const APPROVAL_MAX_WORDS: usize = 3;
    /// How many other models a selection reason lists.
    const RECORDED_RUNNERS_UP: usize = 3;
    /// The role a step-down is scored as: the cheap worker of plan-backed implementation.
    const CHEAP_WORK: RoleDecision = RoleDecision {
        role: WorkRole::Implementation,
        plan_backed: true,
        reason: RoleReason::NoCue,
    };

    /// Routes one turn over the eligible `pool`. `anchor` holds the affinity store and the
    /// turn's key, `None` without a key. Returns `None` for an empty pool.
    #[must_use]
    #[allow(
        clippy::too_many_arguments,
        reason = "Each input is a separate per-request value; a bundle would only rename them"
    )]
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
        let choice = Self::choose(pool, policy, signals, anchor, usage, live, now)?;
        let turn = choice.turn;
        let effort = SmartAutoEffort::select(
            tier,
            turn.role,
            turn.plan_backed,
            requested,
            &choice.model.effort_levels,
        );
        let why = Self::why(pool, tier, &choice, usage, live);
        Some(RouteDecision {
            model: choice.model,
            role: turn.role,
            plan_backed: turn.plan_backed,
            effort,
            reason: choice.reason,
            pin_miss: choice.pin_miss,
            user_pin: choice.user_pin,
            why,
        })
    }

    /// The role rule, the directive outcome, the chosen model's score parts and the best
    /// other models, so the selection can be studied later.
    fn why(
        pool: &[&ModelDescriptor],
        tier: SmartAutoTier,
        choice: &Choice<'_>,
        usage: &HashMap<String, u64>,
        live: &LiveSignals,
    ) -> SelectionWhy {
        let turn = choice.turn;
        let score = SmartAutoSuitability::breakdown(
            &choice.model.qualified_id,
            turn.role,
            turn.plan_backed,
            tier,
            live.get(&choice.model.qualified_id),
        );
        let mut others: Vec<&ModelDescriptor> = pool
            .iter()
            .copied()
            .filter(|model| !std::ptr::eq(*model, choice.model))
            .collect();
        others.sort_by_key(|model| Reverse(Self::rank(model, tier, turn, usage, live)));
        let runners_up = others
            .into_iter()
            .take(Self::RECORDED_RUNNERS_UP)
            .map(|model| RankedModel {
                model: model.qualified_id.clone(),
                score: Self::rank(model, tier, turn, usage, live).0,
            })
            .collect();
        SelectionWhy {
            role_reason: Some(turn.reason),
            directive: choice.directive,
            score: Some(score),
            runners_up,
        }
    }

    fn choose<'a>(
        pool: &[&'a ModelDescriptor],
        policy: &SmartAutoPolicy,
        signals: &TurnSignals,
        anchor: Option<(&SmartAutoAffinity, &AffinityKey)>,
        usage: &HashMap<String, u64>,
        live: &LiveSignals,
        now: Instant,
    ) -> Option<Choice<'a>> {
        let tier = policy.tier.unwrap_or_default();
        let directive = Self::directive(signals);
        let outcome = match &directive {
            Some(ModelDirective::Model(names)) => {
                let turn = SmartAutoRoles::classify_user_text(signals, policy.role);
                if let Some(model) = Self::directed(pool, names, tier, turn, usage, live) {
                    return Some(Choice {
                        directive: Some(DirectiveOutcome::Applied),
                        user_pin: true,
                        ..Choice::routed(model, turn, RouteReason::UserDirective, None)
                    });
                }
                Some(DirectiveOutcome::NoMatch)
            }
            Some(ModelDirective::Auto) => Some(DirectiveOutcome::Cleared),
            None => None,
        };
        let released = outcome == Some(DirectiveOutcome::Cleared);
        let mut choice =
            Self::choose_by_rules(pool, policy, signals, anchor, usage, live, now, released)?;
        choice.directive = outcome;
        Some(choice)
    }

    /// The model directive of a user turn. A tool continuation carries no new user text.
    fn directive(signals: &TurnSignals) -> Option<ModelDirective> {
        if signals.tool_continuation {
            return None;
        }
        SmartAutoDirective::parse(&SmartAutoRoles::visible_text(
            signals.last_user_text.as_deref()?,
        ))
    }

    /// The best pool model the directive names. The names go most specific first.
    fn directed<'a>(
        pool: &[&'a ModelDescriptor],
        names: &[String],
        tier: SmartAutoTier,
        turn: RoleDecision,
        usage: &HashMap<String, u64>,
        live: &LiveSignals,
    ) -> Option<&'a ModelDescriptor> {
        names.iter().find_map(|name| {
            let matching: Vec<&ModelDescriptor> = pool
                .iter()
                .copied()
                .filter(|model| SmartAutoDirective::names_model(name, &model.qualified_id))
                .collect();
            Self::best(&matching, tier, turn, usage, live)
        })
    }

    /// The route without a model directive. `released` drops a user pin's hold, so the pin
    /// follows the usual switch rules again.
    #[allow(
        clippy::too_many_arguments,
        reason = "Each input is a separate per-request value; a bundle would only rename them"
    )]
    fn choose_by_rules<'a>(
        pool: &[&'a ModelDescriptor],
        policy: &SmartAutoPolicy,
        signals: &TurnSignals,
        anchor: Option<(&SmartAutoAffinity, &AffinityKey)>,
        usage: &HashMap<String, u64>,
        live: &LiveSignals,
        now: Instant,
        released: bool,
    ) -> Option<Choice<'a>> {
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
                return Some(Choice::routed(model, turn, reason, pin_miss));
            }
        };
        let user_pin = !released && affinity.is_user_pin(key, now);
        if signals.tool_continuation {
            let turn = RoleDecision {
                role: pinned_role,
                plan_backed: pinned_plan_backed,
                reason: RoleReason::KeptRole,
            };
            return Some(Choice {
                user_pin,
                ..Choice::routed(pinned, turn, RouteReason::ToolContinuationReplay, None)
            });
        }
        let turn = SmartAutoRoles::classify_user_text(signals, policy.role);
        if user_pin {
            return Some(Choice {
                user_pin,
                ..Choice::routed(pinned, turn, RouteReason::UserPinned, None)
            });
        }
        let cheap = Self::best(pool, tier, Self::CHEAP_WORK, usage, live)?;
        let cost = Self::step_down_cost(pinned, cheap, live, signals.estimated_prompt_tokens);
        let step_down_allowed = affinity.step_down_allowed(key, signals.user_turns, cost, now);
        if Self::steps_down(
            pinned_role,
            pinned_plan_backed,
            turn,
            signals,
            step_down_allowed,
        ) {
            let model = cheap;
            let reason = if std::ptr::eq(model, pinned) {
                RouteReason::Pinned
            } else {
                RouteReason::RoleSwitch
            };
            return Some(Choice::routed(model, turn, reason, None));
        }
        let settled = affinity.soft_switch_allowed(key, signals.user_turns, now);
        let switches = SmartAutoAffinity::should_switch(
            pinned_role,
            pinned_plan_backed,
            turn.role,
            turn.plan_backed,
            signals,
            settled,
        );
        if !switches {
            return Some(Choice::routed(pinned, turn, RouteReason::Pinned, None));
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
        Some(Choice::routed(model, turn, reason, None))
    }

    /// The prices a step-down from `pinned` to `cheap` weighs, when both models have known rates.
    fn step_down_cost(
        pinned: &ModelDescriptor,
        cheap: &ModelDescriptor,
        live: &LiveSignals,
        turn_tokens: u64,
    ) -> Option<StepDownCost> {
        let pinned = live.get(&pinned.qualified_id).rates?;
        let cheap = live.get(&cheap.qualified_id).rates?;
        Some(StepDownCost {
            pinned,
            cheap,
            pinned_blended: SmartAutoLive::blended_usd_per_1m(&pinned),
            cheap_blended: SmartAutoLive::blended_usd_per_1m(&cheap),
            turn_tokens,
        })
    }

    /// Whether a heavy pin steps down to the cheap pick at the start of a user turn. Only
    /// planning and implementation without a plan keep a premium model; any other turn moves,
    /// even on a warm cache, because the turn's tool loop then replays the cheap model. A short
    /// approval such as "yes, go" continues the heavy work. The decision is made once per user
    /// turn: a tool continuation never steps down, so a turn never churns between models.
    /// `allowed` is the switch budget, which bounds ping-pong across turns.
    fn steps_down(
        pinned_role: WorkRole,
        pinned_plan_backed: bool,
        turn: RoleDecision,
        signals: &TurnSignals,
        allowed: bool,
    ) -> bool {
        let pinned_is_heavy = pinned_role != WorkRole::Conversation
            && !(pinned_role == WorkRole::Implementation && pinned_plan_backed);
        allowed
            && !signals.tool_continuation
            && pinned_is_heavy
            && !Self::needs_premium(turn)
            && !Self::is_short_approval(signals)
    }

    /// Planning and implementation without a plan are the turns that keep a premium model.
    fn needs_premium(turn: RoleDecision) -> bool {
        turn.role == WorkRole::Planning
            || (turn.role == WorkRole::Implementation && !turn.plan_backed)
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
        let taken: Vec<&str> = taken.iter().map(|id| strip_window_suffix(id).0).collect();
        let mut ranked: Vec<&ModelDescriptor> = pool
            .iter()
            .copied()
            .filter(|model| !taken.contains(&strip_window_suffix(&model.qualified_id).0))
            .collect();
        ranked.sort_by_key(|model| Reverse(Self::rank(model, tier, turn, usage, live)));
        let picked_profile = pool
            .iter()
            .find(|model| taken.first() == Some(&strip_window_suffix(&model.qualified_id).0))
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
        pool.iter()
            .copied()
            .max_by_key(|model| Self::rank(model, tier, turn, usage, live))
    }

    /// The ranking key: the score, then the usage points, then the lowest qualified id.
    fn rank<'m>(
        model: &'m ModelDescriptor,
        tier: SmartAutoTier,
        turn: RoleDecision,
        usage: &HashMap<String, u64>,
        live: &LiveSignals,
    ) -> (u32, u32, Reverse<&'m str>) {
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
    }

    /// 0 below 128 requests, 1 below 32,768, else 2: usage separates only near ties.
    fn usage_points(model: &ModelDescriptor, usage: &HashMap<String, u64>) -> u32 {
        let requests = usage
            .get(strip_window_suffix(&model.qualified_id).0)
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
    use crate::schemas::directive_outcome::DirectiveOutcome;
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
    fn a_premium_pin_stays_between_premium_roles() {
        let now = Instant::now();
        let cold = now + SmartAutoAffinity::CACHE_WARM_WINDOW + Duration::from_secs(1);
        let affinity = pinned_store(OPUS, WorkRole::Implementation, false, now);
        let owned = models(&[HAIKU, OPUS, SONNET]);
        let pool: Vec<&ModelDescriptor> = owned.iter().collect();
        let decision = route_pinned(&pool, &affinity, &signals(PLANNING_TEXT, 6, false), cold);
        assert_eq!(decision.model.qualified_id, OPUS);
        assert_eq!(decision.role, WorkRole::Planning);
        assert_eq!(decision.reason, RouteReason::Pinned);
    }

    #[test]
    fn a_warm_heavy_pin_steps_down_for_plan_backed_work_and_its_tool_loop_stays() {
        let now = Instant::now();
        let affinity = pinned_store(OPUS, WorkRole::Planning, false, now);
        let key = key(&policy(SmartAutoTier::Balance));
        let owned = models(&[HAIKU, OPUS, SONNET]);
        let pool: Vec<&ModelDescriptor> = owned.iter().collect();

        let turn = route_pinned(&pool, &affinity, &signals(SCOPED_TEXT, 2, false), now);
        assert_eq!(turn.model.qualified_id, HAIKU);
        assert_eq!(turn.role, WorkRole::Implementation);
        assert_eq!(turn.reason, RouteReason::RoleSwitch);

        assert!(affinity.commit(&key, HAIKU, turn.role, turn.plan_backed, 2, now));
        let replay = route_pinned(&pool, &affinity, &signals(SCOPED_TEXT, 2, true), now);
        assert_eq!(replay.model.qualified_id, HAIKU);
        assert_eq!(replay.reason, RouteReason::ToolContinuationReplay);
    }

    #[test]
    fn a_warm_heavy_pin_keeps_its_model_for_planning_and_hard_implementation() {
        let now = Instant::now();
        let affinity = pinned_store(OPUS, WorkRole::Planning, false, now);
        let owned = models(&[HAIKU, OPUS, SONNET]);
        let pool: Vec<&ModelDescriptor> = owned.iter().collect();
        let planning = route_pinned(&pool, &affinity, &signals(PLANNING_TEXT, 2, false), now);
        assert_eq!(planning.model.qualified_id, OPUS);
        assert_eq!(planning.reason, RouteReason::Pinned);
        let hard = "implement the retry logic for the storage client";
        let hard = route_pinned(&pool, &affinity, &signals(hard, 2, false), now);
        assert_eq!(hard.model.qualified_id, OPUS);
        assert_eq!(hard.role, WorkRole::Implementation);
        assert!(!hard.plan_backed);
    }

    #[test]
    fn a_warm_heavy_pin_steps_down_for_review_work() {
        let now = Instant::now();
        let affinity = pinned_store(OPUS, WorkRole::Planning, false, now);
        let owned = models(&[HAIKU, OPUS, SONNET]);
        let pool: Vec<&ModelDescriptor> = owned.iter().collect();
        let decision = route_pinned(&pool, &affinity, &signals(REVIEW_TEXT, 2, false), now);
        assert_eq!(decision.model.qualified_id, HAIKU);
        assert_eq!(decision.reason, RouteReason::RoleSwitch);
    }

    #[test]
    fn step_downs_stop_when_the_switch_budget_is_spent() {
        let now = Instant::now();
        let affinity = pinned_store(OPUS, WorkRole::Planning, false, now);
        let key = key(&policy(SmartAutoTier::Balance));
        assert!(affinity.commit(&key, HAIKU, WorkRole::Implementation, true, 2, now));
        assert!(affinity.commit(&key, OPUS, WorkRole::Planning, false, 3, now));
        let owned = models(&[HAIKU, OPUS, SONNET]);
        let pool: Vec<&ModelDescriptor> = owned.iter().collect();
        let decision = route_pinned(&pool, &affinity, &signals(SCOPED_TEXT, 4, false), now);
        assert_eq!(decision.model.qualified_id, OPUS);
        assert_eq!(decision.reason, RouteReason::Pinned);
    }

    fn priced(input: f64, output: f64, read: f64, write: f64) -> ModelSignal {
        ModelSignal {
            rates: Some(crate::configs::TokenRates {
                input_per_1m: input,
                output_per_1m: output,
                cache_read_per_1m: read,
                cache_write_per_1m: write,
            }),
            ..ModelSignal::default()
        }
    }

    fn route_priced<'a>(
        pool: &[&'a ModelDescriptor],
        affinity: &SmartAutoAffinity,
        turn: &TurnSignals,
        now: Instant,
    ) -> RouteDecision<'a> {
        let policy = policy(SmartAutoTier::Balance);
        let key = key(&policy);
        // The cheap model writes cache dearly, so a big warm prefix outweighs a small turn.
        let live = live(&[
            (OPUS, priced(4.0, 20.0, 0.2, 5.0)),
            (HAIKU, priced(1.0, 5.0, 0.1, 10.0)),
        ]);
        SmartAutoRouter::route(
            pool,
            &policy,
            turn,
            Some((affinity, &key)),
            &HashMap::new(),
            &live,
            None,
            now,
        )
        .unwrap()
    }

    #[test]
    fn a_warm_prefix_dearer_to_rewrite_than_the_turn_saves_blocks_the_step_down() {
        let now = Instant::now();
        let affinity = pinned_store(OPUS, WorkRole::Planning, false, now);
        affinity.record_prefix(&key(&policy(SmartAutoTier::Balance)), 150_000);
        let owned = models(&[HAIKU, OPUS, SONNET]);
        let pool: Vec<&ModelDescriptor> = owned.iter().collect();
        let small = TurnSignals {
            estimated_prompt_tokens: 1_000,
            ..signals(SCOPED_TEXT, 2, false)
        };
        let kept = route_priced(&pool, &affinity, &small, now);
        assert_eq!(kept.model.qualified_id, OPUS);
        assert_eq!(kept.reason, RouteReason::Pinned);

        let large = TurnSignals {
            estimated_prompt_tokens: 2_000_000,
            ..small
        };
        let moved = route_priced(&pool, &affinity, &large, now);
        assert_eq!(moved.model.qualified_id, HAIKU);
        assert_eq!(moved.reason, RouteReason::RoleSwitch);
    }

    #[test]
    fn a_cold_prefix_never_blocks_the_step_down() {
        let now = Instant::now();
        let cold = now + SmartAutoAffinity::CACHE_WARM_WINDOW + Duration::from_secs(1);
        let affinity = pinned_store(OPUS, WorkRole::Planning, false, now);
        affinity.record_prefix(&key(&policy(SmartAutoTier::Balance)), 150_000);
        let owned = models(&[HAIKU, OPUS, SONNET]);
        let pool: Vec<&ModelDescriptor> = owned.iter().collect();
        let small = TurnSignals {
            estimated_prompt_tokens: 1_000,
            ..signals(SCOPED_TEXT, 2, false)
        };
        let decision = route_priced(&pool, &affinity, &small, cold);
        assert_eq!(decision.model.qualified_id, HAIKU);
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
    fn a_heavy_pin_steps_down_on_a_plain_conversation_turn() {
        let now = Instant::now();
        let cold = now + SmartAutoAffinity::CACHE_WARM_WINDOW + Duration::from_secs(1);
        let affinity = pinned_store(OPUS, WorkRole::Planning, false, now);
        let owned = models(&[HAIKU, OPUS, SONNET]);
        let pool: Vec<&ModelDescriptor> = owned.iter().collect();
        let chat = "thanks, what does the new storage layer do for us";

        let warm = route_pinned(&pool, &affinity, &signals(chat, 2, false), now);
        assert_eq!(warm.model.qualified_id, HAIKU);
        assert_eq!(warm.reason, RouteReason::RoleSwitch);

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
    fn a_directive_beats_the_role_pick() {
        let owned = models(&[HAIKU, OPUS, SONNET]);
        let pool: Vec<&ModelDescriptor> = owned.iter().collect();
        let decision = route_fresh(
            &pool,
            SmartAutoTier::Balance,
            "use sonnet. plan the storage layer",
        );
        assert_eq!(decision.model.qualified_id, SONNET);
        assert_eq!(decision.reason, RouteReason::UserDirective);
        assert_eq!(decision.role, WorkRole::Planning);
        assert!(decision.user_pin);
        assert_eq!(decision.why.directive, Some(DirectiveOutcome::Applied));
    }

    #[test]
    fn a_directive_for_a_model_outside_the_pool_leaves_the_route_to_the_rules() {
        let owned = models(&[HAIKU, OPUS, SONNET]);
        let pool: Vec<&ModelDescriptor> = owned.iter().collect();
        let decision = route_fresh(
            &pool,
            SmartAutoTier::Balance,
            "use gemini. plan the storage layer",
        );
        assert_eq!(decision.model.qualified_id, OPUS);
        assert_eq!(decision.reason, RouteReason::Fresh);
        assert!(!decision.user_pin);
        assert_eq!(decision.why.directive, Some(DirectiveOutcome::NoMatch));
    }

    #[test]
    fn a_tool_continuation_ignores_directive_text() {
        let owned = models(&[HAIKU, OPUS, SONNET]);
        let pool: Vec<&ModelDescriptor> = owned.iter().collect();
        let decision = SmartAutoRouter::route(
            &pool,
            &policy(SmartAutoTier::Balance),
            &signals("use haiku. plan the storage layer", 1, true),
            None,
            &HashMap::new(),
            &LiveSignals::default(),
            None,
            Instant::now(),
        )
        .unwrap();
        assert_eq!(decision.model.qualified_id, OPUS);
        assert_eq!(decision.why.directive, None);
    }

    fn user_pinned_store(model: &str, now: Instant) -> SmartAutoAffinity {
        let affinity = pinned_store(model, WorkRole::Conversation, false, now);
        affinity.mark_user_pin(&key(&policy(SmartAutoTier::Balance)), model);
        affinity
    }

    #[test]
    fn a_user_pin_holds_across_a_role_change() {
        let owned = models(&[HAIKU, OPUS, SONNET]);
        let pool: Vec<&ModelDescriptor> = owned.iter().collect();
        let now = Instant::now();
        let affinity = user_pinned_store(HAIKU, now);
        let cold = now + SmartAutoAffinity::CACHE_WARM_WINDOW + Duration::from_secs(1);

        let decision = route_pinned(&pool, &affinity, &signals(PLANNING_TEXT, 5, false), cold);

        assert_eq!(decision.model.qualified_id, HAIKU);
        assert_eq!(decision.reason, RouteReason::UserPinned);
        assert!(decision.user_pin);
    }

    #[test]
    fn a_tool_continuation_keeps_the_user_pin() {
        let owned = models(&[HAIKU, OPUS, SONNET]);
        let pool: Vec<&ModelDescriptor> = owned.iter().collect();
        let now = Instant::now();
        let affinity = user_pinned_store(HAIKU, now);

        let decision = route_pinned(&pool, &affinity, &signals("", 1, true), now);

        assert_eq!(decision.model.qualified_id, HAIKU);
        assert_eq!(decision.reason, RouteReason::ToolContinuationReplay);
        assert!(decision.user_pin);
        assert_eq!(decision.why.role_reason, Some(RoleReason::KeptRole));
    }

    #[test]
    fn use_auto_releases_the_user_pin() {
        let owned = models(&[HAIKU, OPUS, SONNET]);
        let pool: Vec<&ModelDescriptor> = owned.iter().collect();
        let now = Instant::now();
        let affinity = user_pinned_store(HAIKU, now);
        let cold = now + SmartAutoAffinity::CACHE_WARM_WINDOW + Duration::from_secs(1);

        let decision = route_pinned(
            &pool,
            &affinity,
            &signals("use auto. plan the storage layer", 5, false),
            cold,
        );

        assert_eq!(decision.model.qualified_id, OPUS);
        assert_ne!(decision.reason, RouteReason::UserPinned);
        assert!(!decision.user_pin);
        assert_eq!(decision.why.directive, Some(DirectiveOutcome::Cleared));
    }

    #[test]
    fn the_reason_records_the_score_and_the_best_other_models() {
        let owned = models(&[HAIKU, OPUS, SONNET, LOCAL_CODER, "copilot/gpt-5-mini"]);
        let pool: Vec<&ModelDescriptor> = owned.iter().collect();

        let decision = route_fresh(&pool, SmartAutoTier::Balance, PLANNING_TEXT);

        let why = &decision.why;
        assert_eq!(why.role_reason, Some(RoleReason::PlanningCue));
        let score = why.score.unwrap();
        assert_eq!(score.quality, 10);
        assert!(score.capable);
        assert_eq!(
            score.total,
            SmartAutoSuitability::score(
                OPUS,
                WorkRole::Planning,
                false,
                SmartAutoTier::Balance,
                ModelSignal::default(),
            )
        );
        assert_eq!(why.runners_up.len(), SmartAutoRouter::RECORDED_RUNNERS_UP);
        assert!(why.runners_up.iter().all(|ranked| ranked.model != OPUS));
        assert!(
            why.runners_up
                .windows(2)
                .all(|pair| pair[0].score >= pair[1].score)
        );
        assert!(why.runners_up[0].score <= score.total);
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
    const TIMED_SELECTIONS: usize = 200;
    /// Context window of every timed model: the long-context fixture.
    const LONG_CONTEXT_TOKENS: u64 = 1_000_000;
    /// Estimated prompt size of the timed request, in tokens.
    const LONG_PROMPT_TOKENS: u64 = 200_000;
    /// Repetitions of the planning sentence that fill most of the 8 KiB text bound.
    const LONG_TEXT_REPEATS: usize = 170;
    const LIMIT_CONFIDENCE: f32 = 0.9;
    /// Target selection time for the benchmark report.
    const P95_TARGET: Duration = Duration::from_millis(1);

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
        let pool = SmartAutoPool::eligible(&snapshot, &timed_policy, MESSAGES, LONG_PROMPT_TOKENS);
        let started = Instant::now();

        for _ in 0..TIMED_SELECTIONS {
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
            assert!(decision.is_some());
        }

        let elapsed = started.elapsed();
        let timed_selections = u32::try_from(TIMED_SELECTIONS).expect("benchmark count fits u32");
        let per_selection = elapsed / timed_selections;
        println!(
            "selection average over {TIMED_SELECTIONS} runs: {per_selection:?} (target {P95_TARGET:?})"
        );
    }
}
