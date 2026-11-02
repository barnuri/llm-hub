use crate::schemas::hub_selector::HubSelector;
use crate::schemas::limit_evidence::LimitEvidence;
use crate::schemas::model_context::strip_1m_suffix;
use crate::schemas::model_descriptor::ModelDescriptor;
use crate::schemas::model_descriptor_snapshot::ModelDescriptorSnapshot;
use crate::schemas::pool_limits::PoolLimits;
use crate::schemas::smart_auto_policy::SmartAutoPolicy;
use crate::services::copilot_auto::AutoTier;

/// The one verdict the pool and the rule effects share. `AllowedOverride`, `Admitted` and the
/// excludes carry the glob that decided it. `Admitted` carries the first include glob that matched.
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum RuleDecision<'a> {
    NotChat,
    AutoAlias,
    HubNamespace,
    AllowedOverride(&'a String),
    Admitted(Option<&'a String>),
    ExcludedByGlobal(&'a String),
    ExcludedByProfile(&'a String),
    NotInProfileInclude,
    NotInGlobalInclude,
}

/// The concrete chat models a Smart Auto policy allows. Pure: no I/O, one allocation.
/// Unknown, conflicting or too small capacity never satisfies a minimum.
#[derive(Debug)]
pub struct SmartAutoPool;

impl SmartAutoPool {
    const WILDCARD: u8 = b'*';
    const PROFILE_SEPARATOR: char = '/';

    /// Eligible descriptors sorted by qualified id. `endpoint` is the client route, e.g.
    /// `/v1/messages`. `prompt_tokens` is the estimated prompt size, and 0 skips the fit check.
    #[must_use]
    pub fn eligible<'a>(
        snapshot: &'a ModelDescriptorSnapshot,
        policy: &SmartAutoPolicy,
        endpoint: &str,
        prompt_tokens: u64,
    ) -> Vec<&'a ModelDescriptor> {
        let mut pool: Vec<&ModelDescriptor> = snapshot
            .iter()
            .filter(|descriptor| Self::admits(descriptor, policy, endpoint, prompt_tokens))
            .collect();
        pool.sort_unstable_by(|left, right| left.qualified_id.cmp(&right.qualified_id));
        pool
    }

    fn admits(
        descriptor: &ModelDescriptor,
        policy: &SmartAutoPolicy,
        endpoint: &str,
        prompt_tokens: u64,
    ) -> bool {
        Self::passes_rules(descriptor, policy)
            && Self::serves(descriptor, endpoint)
            && Self::meets_minima(descriptor, policy)
            && Self::fits_prompt(descriptor, prompt_tokens)
    }

    /// True for a model in the `hub/` namespace, which is a selector, not a backend model.
    pub(crate) fn is_hub(descriptor: &ModelDescriptor) -> bool {
        descriptor.profile == HubSelector::NAMESPACE
    }

    /// True for a Copilot Auto alias such as `auto` or `auto-fast`.
    pub(crate) fn is_auto_alias(descriptor: &ModelDescriptor) -> bool {
        AutoTier::from_model(Self::bare_id(&descriptor.qualified_id)).is_some()
    }

    fn bare_id(qualified_id: &str) -> &str {
        let id = strip_1m_suffix(qualified_id);
        id.split_once(Self::PROFILE_SEPARATOR)
            .map_or(id, |(_, model)| model)
    }

    /// A model that lists no routes is served on every route.
    fn serves(descriptor: &ModelDescriptor, endpoint: &str) -> bool {
        descriptor.endpoints.is_empty()
            || descriptor.endpoints.iter().any(|route| route == endpoint)
    }

    fn passes_rules(descriptor: &ModelDescriptor, policy: &SmartAutoPolicy) -> bool {
        matches!(
            Self::rule_decision(descriptor, policy),
            RuleDecision::Admitted(_) | RuleDecision::AllowedOverride(_)
        )
    }

    /// The one decision both the pool and `rule_effects` use. It checks, in order: not chat,
    /// auto alias, hub, allow, global exclude, profile exclude, profile include, global include.
    pub(crate) fn rule_decision<'a>(
        descriptor: &ModelDescriptor,
        policy: &'a SmartAutoPolicy,
    ) -> RuleDecision<'a> {
        if !descriptor.chat {
            return RuleDecision::NotChat;
        }
        if Self::is_auto_alias(descriptor) {
            return RuleDecision::AutoAlias;
        }
        if Self::is_hub(descriptor) {
            return RuleDecision::HubNamespace;
        }
        let qualified_id = strip_1m_suffix(&descriptor.qualified_id);
        if let Some(glob) = Self::first_match(&policy.allow, qualified_id) {
            return RuleDecision::AllowedOverride(glob);
        }
        if let Some(glob) = Self::first_match(&policy.exclude, qualified_id) {
            return RuleDecision::ExcludedByGlobal(glob);
        }
        let profile = descriptor.profile.as_str();
        if let Some(glob) = policy
            .profile_exclude
            .get(profile)
            .and_then(|globs| Self::first_match(globs, qualified_id))
        {
            return RuleDecision::ExcludedByProfile(glob);
        }
        let mut matched = None;
        for allow_list in policy.profile_include.get(profile).into_iter().flatten() {
            match Self::first_match(allow_list, qualified_id) {
                Some(glob) => matched = matched.or(Some(glob)),
                None => return RuleDecision::NotInProfileInclude,
            }
        }
        for allow_list in &policy.include {
            match Self::first_match(allow_list, qualified_id) {
                Some(glob) => matched = matched.or(Some(glob)),
                None => return RuleDecision::NotInGlobalInclude,
            }
        }
        RuleDecision::Admitted(matched)
    }

    /// The first glob in `globs` that matches `qualified_id`, which has its `[1m]` alias stripped.
    /// It matches the qualified id or the bare id, and an alias suffix on the glob is ignored.
    fn first_match<'a>(
        globs: impl IntoIterator<Item = &'a String>,
        qualified_id: &str,
    ) -> Option<&'a String> {
        let bare_id = Self::bare_id(qualified_id);
        globs.into_iter().find(|glob| {
            let glob = strip_1m_suffix(glob);
            Self::glob_matches(glob, qualified_id) || Self::glob_matches(glob, bare_id)
        })
    }

    /// Total context is at least the prompt allowance, and the local runtime caps both.
    fn meets_minima(descriptor: &ModelDescriptor, policy: &SmartAutoPolicy) -> bool {
        let limits = Self::effective(descriptor);
        Self::meets(limits.context, policy.min_context_tokens)
            && Self::meets(limits.input, policy.min_input_tokens)
            && Self::meets(limits.output, policy.min_output_tokens)
    }

    /// The capacities one model serves, with the local runtime cap applied to both prompt and total.
    fn effective(descriptor: &ModelDescriptor) -> PoolLimits {
        let runtime = Self::resolved(descriptor.runtime_context.as_ref());
        let input = Self::capped(Self::resolved(descriptor.input.as_ref()), runtime);
        let context = Self::capped(Self::resolved(descriptor.context.as_ref()), runtime).max(input);
        let output = Self::resolved(descriptor.output.as_ref());
        PoolLimits {
            context,
            input,
            output,
        }
    }

    /// The limits every pool member meets: each field is the smallest known value, or `None`
    /// when any member leaves it unknown or the pool is empty.
    #[must_use]
    pub fn advertised_limits(pool: &[&ModelDescriptor]) -> PoolLimits {
        let smallest = |field: fn(PoolLimits) -> Option<u64>| {
            pool.iter()
                .map(|descriptor| field(Self::effective(descriptor)))
                .reduce(|left, right| Some(left?.min(right?)))
                .flatten()
        };
        PoolLimits {
            context: smallest(|limits| limits.context),
            input: smallest(|limits| limits.input),
            output: smallest(|limits| limits.output),
        }
    }

    /// Rejects only on a resolved ceiling below the prompt. The prompt allowance beats the
    /// total context, and an accepted-request lower bound is never a ceiling.
    fn fits_prompt(descriptor: &ModelDescriptor, prompt_tokens: u64) -> bool {
        let prompt_limit = descriptor.input.as_ref().or(descriptor.context.as_ref());
        [prompt_limit, descriptor.runtime_context.as_ref()]
            .into_iter()
            .flatten()
            .filter(|evidence| !evidence.conflict && !evidence.source.is_lower_bound())
            .all(|ceiling| ceiling.value >= prompt_tokens)
    }

    /// A conflicting value is unresolved, so it counts as unknown.
    fn resolved(evidence: Option<&LimitEvidence>) -> Option<u64> {
        evidence
            .filter(|evidence| !evidence.conflict)
            .map(|evidence| evidence.value)
    }

    fn capped(capacity: Option<u64>, runtime: Option<u64>) -> Option<u64> {
        match runtime {
            Some(runtime) => capacity.map(|capacity| capacity.min(runtime)),
            None => capacity,
        }
    }

    fn meets(capacity: Option<u64>, minimum: Option<u64>) -> bool {
        minimum.is_none_or(|minimum| capacity.is_some_and(|capacity| capacity >= minimum))
    }

    /// `*` matches any run of characters, including `/`. Every other byte matches itself.
    fn glob_matches(glob: &str, text: &str) -> bool {
        let (glob, text) = (glob.as_bytes(), text.as_bytes());
        let (mut glob_at, mut text_at) = (0, 0);
        let mut last_star: Option<(usize, usize)> = None;
        while text_at < text.len() {
            match glob.get(glob_at) {
                Some(&Self::WILDCARD) => {
                    last_star = Some((glob_at, text_at));
                    glob_at += 1;
                }
                Some(&byte) if byte == text[text_at] => {
                    glob_at += 1;
                    text_at += 1;
                }
                _ => {
                    let Some((star_at, star_text_at)) = last_star else {
                        return false;
                    };
                    glob_at = star_at + 1;
                    text_at = star_text_at + 1;
                    last_star = Some((star_at, text_at));
                }
            }
        }
        glob[glob_at..].iter().all(|&byte| byte == Self::WILDCARD)
    }
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeSet;

    use super::*;
    use crate::consts::CONTEXT_TOKENS_1M;
    use crate::schemas::evidence_source::EvidenceSource;
    use crate::schemas::smart_auto_policy::tests::Fixture;
    use crate::schemas::smart_auto_rules::SmartAutoRules;

    const MESSAGES: &str = "/v1/messages";
    const CONFIDENCE: f32 = 0.9;

    fn limit(value: u64, source: EvidenceSource) -> LimitEvidence {
        LimitEvidence::new(value, source, "test", CONFIDENCE)
    }

    fn model(qualified_id: &str) -> ModelDescriptor {
        ModelDescriptor::for_test(qualified_id)
    }

    fn with_context(qualified_id: &str, context: u64) -> ModelDescriptor {
        ModelDescriptor {
            context: Some(limit(context, EvidenceSource::Seeded)),
            ..model(qualified_id)
        }
    }

    fn eligible_ids(
        models: Vec<ModelDescriptor>,
        policy: &SmartAutoPolicy,
        prompt_tokens: u64,
    ) -> Vec<String> {
        let snapshot = ModelDescriptorSnapshot::new(models);
        SmartAutoPool::eligible(&snapshot, policy, MESSAGES, prompt_tokens)
            .into_iter()
            .map(|descriptor| descriptor.qualified_id.clone())
            .collect()
    }

    fn min_context(tokens: u64) -> SmartAutoPolicy {
        SmartAutoPolicy {
            min_context_tokens: Some(tokens),
            ..SmartAutoPolicy::default()
        }
    }

    fn min_input(tokens: u64) -> SmartAutoPolicy {
        SmartAutoPolicy {
            min_input_tokens: Some(tokens),
            ..SmartAutoPolicy::default()
        }
    }

    #[test]
    fn min_1m_context_keeps_known_1m_models_and_drops_unknown_and_smaller_ones() {
        let models = vec![
            with_context("copilot/claude-opus-4.7", CONTEXT_TOKENS_1M),
            with_context("copilot/gpt-6-luna", CONTEXT_TOKENS_1M - 1),
            with_context("copilot/claude-haiku-4.5", 200_000),
            model("llama_swap/unknown"),
        ];

        let ids = eligible_ids(models, &min_context(CONTEXT_TOKENS_1M), 0);

        assert_eq!(ids, ["copilot/claude-opus-4.7"]);
    }

    #[test]
    fn min_1m_input_differs_from_min_1m_context() {
        let total_only = ModelDescriptor {
            input: Some(limit(872_000, EvidenceSource::ObservedRejection)),
            ..with_context("copilot/gpt-6-sol", CONTEXT_TOKENS_1M)
        };
        let prompt_1m = ModelDescriptor {
            input: Some(limit(CONTEXT_TOKENS_1M, EvidenceSource::ObservedRejection)),
            ..with_context("copilot/claude-opus-4.7", 200_000)
        };
        let models = || vec![total_only.clone(), prompt_1m.clone()];

        let by_context = eligible_ids(models(), &min_context(CONTEXT_TOKENS_1M), 0);
        let by_input = eligible_ids(models(), &min_input(CONTEXT_TOKENS_1M), 0);

        assert_eq!(by_context, ["copilot/claude-opus-4.7", "copilot/gpt-6-sol"]);
        assert_eq!(by_input, ["copilot/claude-opus-4.7"]);
    }

    #[test]
    fn unknown_input_and_output_fail_their_minima() {
        let models = vec![with_context("copilot/claude-opus-4.7", CONTEXT_TOKENS_1M)];
        let min_output = SmartAutoPolicy {
            min_output_tokens: Some(1),
            ..SmartAutoPolicy::default()
        };

        assert!(eligible_ids(models.clone(), &min_input(1), 0).is_empty());
        assert!(eligible_ids(models, &min_output, 0).is_empty());
    }

    #[test]
    fn lower_bound_meets_a_minimum_only_at_or_above_it() {
        let below = ModelDescriptor {
            input: Some(limit(
                CONTEXT_TOKENS_1M - 1,
                EvidenceSource::ObservedSuccess,
            )),
            ..model("copilot/below")
        };
        let at = ModelDescriptor {
            input: Some(limit(CONTEXT_TOKENS_1M, EvidenceSource::ObservedSuccess)),
            ..model("copilot/at")
        };

        let ids = eligible_ids(vec![below, at], &min_input(CONTEXT_TOKENS_1M), 0);

        assert_eq!(ids, ["copilot/at"]);
    }

    #[test]
    fn conflicting_limit_fails_a_minimum() {
        let conflicted = ModelDescriptor {
            input: Some(LimitEvidence {
                conflict: true,
                ..limit(CONTEXT_TOKENS_1M, EvidenceSource::Seeded)
            }),
            ..model("copilot/conflicted")
        };

        assert!(eligible_ids(vec![conflicted], &min_input(CONTEXT_TOKENS_1M), 0).is_empty());
    }

    #[test]
    fn runtime_allocation_caps_the_listed_context() {
        let mlx = ModelDescriptor {
            runtime_context: Some(limit(262_144, EvidenceSource::LocalRuntime)),
            ..with_context("llama_swap/mlx-model", CONTEXT_TOKENS_1M)
        };

        assert!(eligible_ids(vec![mlx], &min_context(CONTEXT_TOKENS_1M), 0).is_empty());
    }

    #[test]
    fn exclude_beats_include() {
        let policy = SmartAutoPolicy {
            include: vec![Fixture::globs(&["copilot/*"])],
            exclude: Fixture::globs(&["*fast-preview*"]),
            ..SmartAutoPolicy::default()
        };
        let models = vec![
            model("copilot/grok-fast-preview"),
            model("copilot/gpt-6-sol"),
        ];

        assert_eq!(eligible_ids(models, &policy, 0), ["copilot/gpt-6-sol"]);
    }

    #[test]
    fn multiple_include_lists_intersect() {
        let policy = SmartAutoPolicy {
            include: vec![
                Fixture::globs(&["copilot/*", "llama_swap/*"]),
                Fixture::globs(&["claude-*", "qwen*"]),
            ],
            ..SmartAutoPolicy::default()
        };
        let models = vec![
            model("copilot/claude-opus-4.7"),
            model("copilot/gpt-6-sol"),
            model("llama_swap/qwen3.6-35b"),
            model("other/claude-opus-4.7"),
        ];

        let ids = eligible_ids(models, &policy, 0);

        assert_eq!(ids, ["copilot/claude-opus-4.7", "llama_swap/qwen3.6-35b"]);
    }

    #[test]
    fn empty_include_list_admits_nothing() {
        let policy = SmartAutoPolicy {
            include: vec![BTreeSet::new()],
            ..SmartAutoPolicy::default()
        };

        assert!(eligible_ids(vec![model("copilot/gpt-6-sol")], &policy, 0).is_empty());
    }

    #[test]
    fn speech_non_chat_and_selector_entries_are_dropped() {
        let speech = ModelDescriptor {
            chat: false,
            ..model("ivrit/whisper")
        };
        let models = vec![
            speech,
            model("copilot/auto"),
            model("copilot/auto-fast"),
            model("hub/smart-auto-balance"),
            model("copilot/gpt-6-sol"),
        ];

        let ids = eligible_ids(models, &SmartAutoPolicy::default(), 0);

        assert_eq!(ids, ["copilot/gpt-6-sol"]);
    }

    #[test]
    fn empty_policy_keeps_every_chat_model_in_id_order() {
        let models = vec![
            model("llama_swap/qwen3.6-35b"),
            model("copilot/gpt-6-sol"),
            model("copilot/claude-opus-4.7"),
        ];

        let ids = eligible_ids(models, &SmartAutoPolicy::default(), 0);

        assert_eq!(
            ids,
            [
                "copilot/claude-opus-4.7",
                "copilot/gpt-6-sol",
                "llama_swap/qwen3.6-35b"
            ]
        );
    }

    #[test]
    fn one_m_suffix_is_ignored_on_globs_and_ids() {
        let exclude_suffixed_glob = SmartAutoPolicy {
            exclude: Fixture::globs(&["copilot/claude-opus-4.7[1m]"]),
            ..SmartAutoPolicy::default()
        };
        let include_plain_glob = SmartAutoPolicy {
            include: vec![Fixture::globs(&["claude-opus-4.7"])],
            ..SmartAutoPolicy::default()
        };

        let excluded = eligible_ids(
            vec![model("copilot/claude-opus-4.7"), model("copilot/gpt-6-sol")],
            &exclude_suffixed_glob,
            0,
        );
        let included = eligible_ids(
            vec![model("copilot/claude-opus-4.7[1m]")],
            &include_plain_glob,
            0,
        );

        assert_eq!(excluded, ["copilot/gpt-6-sol"]);
        assert_eq!(included, ["copilot/claude-opus-4.7[1m]"]);
    }

    #[test]
    fn endpoint_must_be_listed_unless_the_model_lists_none() {
        let chat_only = ModelDescriptor {
            endpoints: vec!["/v1/chat/completions".to_owned()],
            ..model("copilot/chat-only")
        };
        let messages = ModelDescriptor {
            endpoints: vec![MESSAGES.to_owned()],
            ..model("copilot/messages")
        };
        let models = vec![chat_only, messages, model("llama_swap/any-route")];

        let ids = eligible_ids(models, &SmartAutoPolicy::default(), 0);

        assert_eq!(ids, ["copilot/messages", "llama_swap/any-route"]);
    }

    #[test]
    fn prompt_must_fit_a_resolved_ceiling() {
        let small = with_context("copilot/small", 200_000);
        let exact = ModelDescriptor {
            input: Some(limit(300_000, EvidenceSource::ObservedRejection)),
            ..with_context("copilot/exact", 200_000)
        };
        let lower_bound = ModelDescriptor {
            input: Some(limit(100_000, EvidenceSource::ObservedSuccess)),
            ..model("copilot/lower-bound")
        };
        let models = vec![small, exact, lower_bound, model("llama_swap/unknown")];

        let ids = eligible_ids(models, &SmartAutoPolicy::default(), 300_000);

        assert_eq!(
            ids,
            ["copilot/exact", "copilot/lower-bound", "llama_swap/unknown"]
        );
    }

    #[test]
    fn glob_star_matches_any_run_including_slashes() {
        assert!(SmartAutoPool::glob_matches("*", ""));
        assert!(SmartAutoPool::glob_matches("*", "copilot/gpt-6-sol"));
        assert!(SmartAutoPool::glob_matches(
            "copilot*sol",
            "copilot/gpt-6-sol"
        ));
        assert!(SmartAutoPool::glob_matches("*a*a*b", "aaab"));
        assert!(SmartAutoPool::glob_matches("gpt-6-sol", "gpt-6-sol"));
        assert!(!SmartAutoPool::glob_matches("gpt-6", "gpt-6-sol"));
        assert!(!SmartAutoPool::glob_matches("*luna", "gpt-6-sol"));
        assert!(!SmartAutoPool::glob_matches("", "gpt"));
    }

    #[test]
    fn advertised_limits_take_the_smallest_known_value_per_field() {
        let big = ModelDescriptor {
            input: Some(limit(CONTEXT_TOKENS_1M, EvidenceSource::Seeded)),
            output: Some(limit(128_000, EvidenceSource::Listed)),
            ..with_context("copilot/big", CONTEXT_TOKENS_1M)
        };
        let mid = ModelDescriptor {
            input: Some(limit(400_000, EvidenceSource::Listed)),
            output: Some(limit(64_000, EvidenceSource::Listed)),
            ..with_context("copilot/mid", 500_000)
        };

        let limits = SmartAutoPool::advertised_limits(&[&big, &mid]);

        assert_eq!(
            limits,
            PoolLimits {
                context: Some(500_000),
                input: Some(400_000),
                output: Some(64_000),
            }
        );
    }

    #[test]
    fn advertised_limits_stay_unknown_when_one_member_is_unknown() {
        let known = ModelDescriptor {
            output: Some(limit(128_000, EvidenceSource::Listed)),
            ..with_context("copilot/known", 200_000)
        };
        let unknown = model("llama_swap/unknown");

        let limits = SmartAutoPool::advertised_limits(&[&known, &unknown]);

        assert_eq!(
            limits,
            PoolLimits {
                context: None,
                input: None,
                output: None,
            }
        );
        assert_eq!(SmartAutoPool::advertised_limits(&[]), PoolLimits::default());
    }
    fn policy_from(json: &str) -> SmartAutoPolicy {
        SmartAutoRules::from_json(json.as_bytes())
            .unwrap()
            .to_policy()
    }

    #[test]
    fn profile_exclude_removes_only_its_own_profiles_models() {
        let policy = policy_from(r#"{"profiles":{"copilot":{"exclude":["gpt-*"]}}}"#);
        let models = vec![
            model("copilot/gpt-6-sol"),
            model("llama_swap/gpt-6-sol"),
            model("copilot/claude-opus-4.7"),
        ];

        let ids = eligible_ids(models, &policy, 0);

        assert_eq!(ids, ["copilot/claude-opus-4.7", "llama_swap/gpt-6-sol"]);
    }

    #[test]
    fn profile_include_binds_only_its_own_profiles_models() {
        let policy = policy_from(r#"{"profiles":{"copilot":{"include":["claude-*"]}}}"#);
        let models = vec![
            model("copilot/claude-opus-4.7"),
            model("copilot/gpt-6-sol"),
            model("llama_swap/gpt-6-sol"),
        ];

        let ids = eligible_ids(models, &policy, 0);

        assert_eq!(ids, ["copilot/claude-opus-4.7", "llama_swap/gpt-6-sol"]);
    }

    #[test]
    fn global_and_profile_includes_both_apply() {
        let policy = policy_from(
            r#"{"include":["copilot/*","llama_swap/*"],
                "profiles":{"copilot":{"include":["claude-*"]}}}"#,
        );
        let models = vec![
            model("copilot/claude-opus-4.7"),
            model("copilot/gpt-6-sol"),
            model("llama_swap/gpt-6-sol"),
            model("llama_swap/claude-local"),
            model("other/claude-opus-4.7"),
        ];

        let ids = eligible_ids(models, &policy, 0);

        assert_eq!(
            ids,
            [
                "copilot/claude-opus-4.7",
                "llama_swap/claude-local",
                "llama_swap/gpt-6-sol"
            ]
        );
    }

    #[test]
    fn global_exclude_removes_every_profile_and_deny_wins_over_a_profile_include() {
        let policy = policy_from(
            r#"{"exclude":["*preview*"],
                "profiles":{"copilot":{"include":["gpt-*"],"exclude":["gpt-6-sol"]}}}"#,
        );
        let models = vec![
            model("copilot/gpt-6-sol"),
            model("copilot/gpt-6-preview"),
            model("llama_swap/gpt-6-preview"),
            model("llama_swap/gpt-6-sol"),
        ];

        let ids = eligible_ids(models, &policy, 0);

        assert_eq!(ids, ["llama_swap/gpt-6-sol"]);
    }

    #[test]
    fn allow_beats_a_global_exclude_and_the_exclude_still_applies_elsewhere() {
        let policy = policy_from(r#"{"exclude":["*preview*"],"allow":["copilot/gpt-6-preview"]}"#);
        let models = vec![
            model("copilot/gpt-6-preview"),
            model("copilot/grok-preview"),
            model("copilot/gpt-6-sol"),
        ];

        let ids = eligible_ids(models, &policy, 0);

        assert_eq!(ids, ["copilot/gpt-6-preview", "copilot/gpt-6-sol"]);
    }

    #[test]
    fn allow_beats_a_profile_exclude_only_for_the_model_it_names() {
        let policy = policy_from(
            r#"{"profiles":{"copilot":{"exclude":["gpt-*"]}},"allow":["copilot/gpt-6-sol"]}"#,
        );
        let models = vec![
            model("copilot/gpt-6-sol"),
            model("copilot/gpt-6-luna"),
            model("llama_swap/gpt-6-sol"),
        ];

        let ids = eligible_ids(models, &policy, 0);

        assert_eq!(ids, ["copilot/gpt-6-sol", "llama_swap/gpt-6-sol"]);
    }

    #[test]
    fn allow_admits_a_model_outside_the_include_lists() {
        let policy = policy_from(r#"{"include":["copilot/*"],"allow":["llama_swap/qwen*"]}"#);
        let models = vec![
            model("copilot/gpt-6-sol"),
            model("llama_swap/qwen3.6-35b"),
            model("llama_swap/other"),
        ];

        let ids = eligible_ids(models, &policy, 0);

        assert_eq!(ids, ["copilot/gpt-6-sol", "llama_swap/qwen3.6-35b"]);
    }

    #[test]
    fn allow_matches_the_bare_id_and_ignores_the_1m_suffix() {
        let policy = policy_from(r#"{"exclude":["*"],"allow":["claude-opus-4.7[1m]"]}"#);
        let models = vec![
            model("copilot/claude-opus-4.7[1m]"),
            model("copilot/gpt-6-sol"),
        ];

        let ids = eligible_ids(models, &policy, 0);

        assert_eq!(ids, ["copilot/claude-opus-4.7[1m]"]);
    }

    #[test]
    fn allow_never_lifts_not_chat_auto_or_hub() {
        let policy = policy_from(r#"{"exclude":["*"],"allow":["*"]}"#);
        let models = vec![
            ModelDescriptor {
                chat: false,
                ..model("copilot/tts")
            },
            model("copilot/auto"),
            model("hub/smart-auto-balance"),
            model("copilot/gpt-6-sol"),
        ];

        let ids = eligible_ids(models, &policy, 0);

        assert_eq!(ids, ["copilot/gpt-6-sol"]);
    }
}
