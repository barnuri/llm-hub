use std::collections::{BTreeMap, BTreeSet};

use serde::{Deserialize, Serialize};

use crate::schemas::app_error::AppError;
use crate::schemas::smart_auto_tier::SmartAutoTier;
use crate::schemas::work_role::WorkRole;

/// Smart Auto routing constraints from one layer, or several layers merged.
/// Model filters are globs over qualified `<profile>/<model>` ids.
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq, Default)]
#[serde(default, deny_unknown_fields)]
pub struct SmartAutoPolicy {
    /// The selected tier; `None` leaves the choice to a later layer or the default.
    pub tier: Option<SmartAutoTier>,
    /// Allow-lists: a model must match one glob of every list. No lists means no restriction.
    pub include: Vec<BTreeSet<String>>,
    /// Deny globs. A model that matches any of them is ineligible, even when an include matches it.
    pub exclude: BTreeSet<String>,
    /// Allow globs. A model that matches one is eligible even when an exclude matches it.
    pub allow: BTreeSet<String>,
    /// Profile name to allow-lists that bind only that profile's models, with the same AND rule.
    pub profile_include: BTreeMap<String, Vec<BTreeSet<String>>>,
    /// Profile name to deny globs that remove only that profile's models.
    pub profile_exclude: BTreeMap<String, BTreeSet<String>>,
    /// The minimum total context window, in tokens.
    pub min_context_tokens: Option<u64>,
    /// The minimum prompt capacity, in tokens.
    pub min_input_tokens: Option<u64>,
    /// The minimum output capacity, in tokens.
    pub min_output_tokens: Option<u64>,
    /// The kind of work the caller says it does; it never widens the pool.
    pub role: Option<WorkRole>,
}

impl SmartAutoPolicy {
    const THOUSAND: u64 = 1_000;
    const MILLION: u64 = 1_000_000;

    /// Returns this policy tightened by a later layer; allow globs are the one relaxation and union in.
    /// The earlier tier is kept, and the later role hint replaces the earlier one.
    #[must_use]
    pub fn merge(&self, later: &SmartAutoPolicy) -> SmartAutoPolicy {
        let mut include = self.include.clone();
        for allow_list in &later.include {
            if !allow_list.is_empty() && !include.contains(allow_list) {
                include.push(allow_list.clone());
            }
        }
        include.retain(|allow_list| !allow_list.is_empty());
        let mut profile_include = self.profile_include.clone();
        for (profile, lists) in &later.profile_include {
            let own = profile_include.entry(profile.clone()).or_default();
            for allow_list in lists {
                if !allow_list.is_empty() && !own.contains(allow_list) {
                    own.push(allow_list.clone());
                }
            }
        }
        profile_include.retain(|_, lists| !lists.is_empty());
        let mut profile_exclude = self.profile_exclude.clone();
        for (profile, globs) in &later.profile_exclude {
            profile_exclude
                .entry(profile.clone())
                .or_default()
                .extend(globs.iter().cloned());
        }
        profile_exclude.retain(|_, globs| !globs.is_empty());
        SmartAutoPolicy {
            tier: self.tier.or(later.tier),
            include,
            exclude: self.exclude.union(&later.exclude).cloned().collect(),
            allow: self.allow.union(&later.allow).cloned().collect(),
            profile_include,
            profile_exclude,
            min_context_tokens: self.min_context_tokens.max(later.min_context_tokens),
            min_input_tokens: self.min_input_tokens.max(later.min_input_tokens),
            min_output_tokens: self.min_output_tokens.max(later.min_output_tokens),
            role: later.role.or(self.role),
        }
    }

    /// Parses a token count such as `200000`, `200k` or `1m`, where `1m` is 1,000,000.
    ///
    /// # Errors
    /// Returns `AppError::BadRequest` for an empty, non-numeric or overflowing value.
    pub fn parse_token_count(raw: &str) -> Result<u64, AppError> {
        let malformed = || {
            AppError::BadRequest(format!(
                "invalid token count {raw:?}: use digits with an optional k or m suffix, e.g. 200k or 1m"
            ))
        };
        let trimmed = raw.trim();
        let lower = trimmed.to_ascii_lowercase();
        let (digits, multiplier) = if let Some(digits) = lower.strip_suffix('m') {
            (digits, Self::MILLION)
        } else if let Some(digits) = lower.strip_suffix('k') {
            (digits, Self::THOUSAND)
        } else {
            (lower.as_str(), 1)
        };
        if digits.is_empty() || !digits.bytes().all(|byte| byte.is_ascii_digit()) {
            return Err(malformed());
        }
        digits
            .parse::<u64>()
            .ok()
            .and_then(|count| count.checked_mul(multiplier))
            .ok_or_else(malformed)
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;

    /// Builds the policies and glob sets the tests compare against.
    pub(crate) struct Fixture;

    impl Fixture {
        /// A glob set from string slices, built the way the header and flag parsers build one.
        pub(crate) fn globs(patterns: &[&str]) -> BTreeSet<String> {
            patterns
                .iter()
                .map(|&pattern| pattern.to_string())
                .collect()
        }

        fn restrictive() -> SmartAutoPolicy {
            SmartAutoPolicy {
                tier: Some(SmartAutoTier::Intelligence),
                include: vec![Fixture::globs(&["copilot/*", "llama_swap/*"])],
                exclude: Fixture::globs(&["*fast-preview*"]),
                allow: Fixture::globs(&["copilot/gpt-6-sol"]),
                profile_include: BTreeMap::from([(
                    "copilot".to_string(),
                    vec![Fixture::globs(&["gpt-*"])],
                )]),
                profile_exclude: BTreeMap::from([(
                    "llama_swap".to_string(),
                    Fixture::globs(&["*preview*"]),
                )]),
                min_context_tokens: Some(1_000_000),
                min_input_tokens: Some(200_000),
                min_output_tokens: Some(32_000),
                role: Some(WorkRole::Planning),
            }
        }
    }

    #[test]
    fn allow_globs_union_across_layers_and_never_clear_one() {
        let parent = SmartAutoPolicy {
            allow: Fixture::globs(&["copilot/gpt-6-sol"]),
            ..SmartAutoPolicy::default()
        };
        let child = SmartAutoPolicy {
            allow: Fixture::globs(&["llama_swap/*"]),
            ..SmartAutoPolicy::default()
        };
        assert_eq!(
            parent.merge(&child).allow,
            Fixture::globs(&["copilot/gpt-6-sol", "llama_swap/*"])
        );
        assert_eq!(
            parent.merge(&SmartAutoPolicy::default()).allow,
            parent.allow
        );
    }

    #[test]
    fn serde_round_trips_every_field() {
        let policy = Fixture::restrictive();
        let json = serde_json::to_value(&policy).unwrap();
        assert_eq!(json["tier"], "intelligence");
        assert_eq!(json["role"], "planning");
        assert_eq!(json["min_context_tokens"], 1_000_000);
        assert_eq!(
            serde_json::from_value::<SmartAutoPolicy>(json).unwrap(),
            policy
        );
    }

    #[test]
    fn missing_fields_deserialize_to_an_unrestricted_policy() {
        let policy: SmartAutoPolicy = serde_json::from_str("{}").unwrap();
        assert_eq!(policy, SmartAutoPolicy::default());
        assert_eq!(policy.tier.unwrap_or_default(), SmartAutoTier::Balance);
    }

    #[test]
    fn unknown_fields_are_rejected() {
        assert!(serde_json::from_str::<SmartAutoPolicy>(r#"{"min_ctx":1}"#).is_err());
    }

    #[test]
    fn includes_intersect_by_requiring_every_allow_list() {
        let parent = SmartAutoPolicy {
            include: vec![Fixture::globs(&["copilot/*"])],
            ..SmartAutoPolicy::default()
        };
        let child = SmartAutoPolicy {
            include: vec![Fixture::globs(&["copilot/gpt-*", "openai/*"])],
            ..SmartAutoPolicy::default()
        };
        let merged = parent.merge(&child);
        assert_eq!(
            merged.include,
            vec![
                Fixture::globs(&["copilot/*"]),
                Fixture::globs(&["copilot/gpt-*", "openai/*"])
            ]
        );
        assert_eq!(merged.merge(&child).include, merged.include);
    }

    #[test]
    fn empty_include_means_no_restriction_and_never_clears_one() {
        let restricted = SmartAutoPolicy {
            include: vec![Fixture::globs(&["copilot/*"])],
            ..SmartAutoPolicy::default()
        };
        let empty_list = SmartAutoPolicy {
            include: vec![BTreeSet::new()],
            ..SmartAutoPolicy::default()
        };
        assert!(
            SmartAutoPolicy::default()
                .merge(&empty_list)
                .include
                .is_empty()
        );
        assert_eq!(restricted.merge(&empty_list).include, restricted.include);
        assert_eq!(empty_list.merge(&restricted).include, restricted.include);
    }

    #[test]
    fn excludes_union() {
        let parent = SmartAutoPolicy {
            exclude: Fixture::globs(&["*fast-preview*"]),
            ..SmartAutoPolicy::default()
        };
        let child = SmartAutoPolicy {
            exclude: Fixture::globs(&["openai/*"]),
            ..SmartAutoPolicy::default()
        };
        assert_eq!(
            parent.merge(&child).exclude,
            Fixture::globs(&["*fast-preview*", "openai/*"])
        );
    }

    #[test]
    fn a_later_include_never_lifts_an_earlier_exclude() {
        let parent = SmartAutoPolicy {
            exclude: Fixture::globs(&["copilot/gpt-5-mini"]),
            ..SmartAutoPolicy::default()
        };
        let child = SmartAutoPolicy {
            include: vec![Fixture::globs(&["copilot/gpt-5-mini"])],
            ..SmartAutoPolicy::default()
        };
        let merged = parent.merge(&child);
        assert!(merged.exclude.contains("copilot/gpt-5-mini"));
        assert_eq!(
            merged.include,
            vec![Fixture::globs(&["copilot/gpt-5-mini"])]
        );
    }

    #[test]
    fn profile_rules_only_tighten_like_global_rules() {
        let parent = SmartAutoPolicy {
            profile_include: BTreeMap::from([(
                "copilot".to_string(),
                vec![Fixture::globs(&["gpt-*"])],
            )]),
            profile_exclude: BTreeMap::from([("copilot".to_string(), Fixture::globs(&["a"]))]),
            ..SmartAutoPolicy::default()
        };
        let child = SmartAutoPolicy {
            profile_include: BTreeMap::from([
                (
                    "copilot".to_string(),
                    vec![Fixture::globs(&["gpt-5*", "o3"]), BTreeSet::new()],
                ),
                ("openai".to_string(), vec![Fixture::globs(&["gpt-*"])]),
            ]),
            profile_exclude: BTreeMap::from([
                ("copilot".to_string(), Fixture::globs(&["b"])),
                ("groq".to_string(), BTreeSet::new()),
            ]),
            ..SmartAutoPolicy::default()
        };

        let merged = parent.merge(&child);

        assert_eq!(
            merged.profile_include,
            BTreeMap::from([
                (
                    "copilot".to_string(),
                    vec![
                        Fixture::globs(&["gpt-*"]),
                        Fixture::globs(&["gpt-5*", "o3"])
                    ]
                ),
                ("openai".to_string(), vec![Fixture::globs(&["gpt-*"])]),
            ])
        );
        assert_eq!(
            merged.profile_exclude,
            BTreeMap::from([("copilot".to_string(), Fixture::globs(&["a", "b"]))])
        );
        assert_eq!(merged.merge(&child), merged);
    }

    #[test]
    fn minimums_take_the_max_and_none_never_lowers_one() {
        let parent = SmartAutoPolicy {
            min_context_tokens: Some(1_000_000),
            min_input_tokens: Some(100_000),
            ..SmartAutoPolicy::default()
        };
        let child = SmartAutoPolicy {
            min_context_tokens: Some(200_000),
            min_input_tokens: Some(400_000),
            min_output_tokens: Some(16_000),
            ..SmartAutoPolicy::default()
        };
        let merged = parent.merge(&child);
        assert_eq!(merged.min_context_tokens, Some(1_000_000));
        assert_eq!(merged.min_input_tokens, Some(400_000));
        assert_eq!(merged.min_output_tokens, Some(16_000));
        assert_eq!(merged.merge(&SmartAutoPolicy::default()), merged);
    }

    #[test]
    fn merging_an_unrestricted_layer_never_relaxes() {
        let policy = Fixture::restrictive();
        assert_eq!(policy.merge(&SmartAutoPolicy::default()), policy);
        assert_eq!(SmartAutoPolicy::default().merge(&policy), policy);
    }

    #[test]
    fn earlier_tier_is_kept_and_later_role_replaces_the_hint() {
        let parent = SmartAutoPolicy {
            tier: Some(SmartAutoTier::Efficiency),
            role: Some(WorkRole::Planning),
            ..SmartAutoPolicy::default()
        };
        let child = SmartAutoPolicy {
            tier: Some(SmartAutoTier::Intelligence),
            role: Some(WorkRole::Implementation),
            ..SmartAutoPolicy::default()
        };
        let merged = parent.merge(&child);
        assert_eq!(merged.tier, Some(SmartAutoTier::Efficiency));
        assert_eq!(merged.role, Some(WorkRole::Implementation));
        let unset = SmartAutoPolicy::default().merge(&child);
        assert_eq!(unset.tier, Some(SmartAutoTier::Intelligence));
    }

    #[test]
    fn token_count_accepts_plain_k_and_m_values() {
        for (raw, expected) in [
            ("1m", 1_000_000),
            ("1M", 1_000_000),
            ("200k", 200_000),
            ("200K", 200_000),
            (" 128000 ", 128_000),
            ("0", 0),
        ] {
            assert_eq!(
                SmartAutoPolicy::parse_token_count(raw).unwrap(),
                expected,
                "{raw}"
            );
        }
    }

    #[test]
    fn token_count_rejects_malformed_values() {
        for raw in [
            "",
            "m",
            "k",
            "1.5m",
            "-1",
            "+1",
            "1mm",
            "1g",
            "1 m",
            "1_000",
            "abc",
            "18446744073709551615k",
        ] {
            let error = SmartAutoPolicy::parse_token_count(raw).unwrap_err();
            assert!(matches!(error, AppError::BadRequest(_)), "{raw}");
        }
    }
}
