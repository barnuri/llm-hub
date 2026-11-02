use std::collections::BTreeMap;

use crate::consts::CONTEXT_TOKENS_1M;
use crate::schemas::app_error::AppError;
use crate::schemas::model_context::strip_1m_suffix;
use crate::schemas::smart_auto_policy::SmartAutoPolicy;
use crate::schemas::smart_auto_tier::SmartAutoTier;

/// A `hub/<name>` model id resolved to a Smart Auto tier and the policy its name carries.
/// The name is a tier selector, a built-in preset or an operator preset.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HubSelector {
    /// The name after `hub/`, without a `[1m]` suffix, e.g. `smart-auto-long-balance`.
    pub name: String,
    /// The tier the name selects. Balance applies when the name sets no tier.
    pub tier: SmartAutoTier,
    /// The constraints the name carries, with `tier` always set.
    pub policy: SmartAutoPolicy,
}

impl HubSelector {
    /// The reserved profile namespace of hub-level virtual models.
    pub const NAMESPACE: &'static str = "hub";
    const NAMESPACE_SEPARATOR: char = '/';
    const PRESET_SEPARATOR: char = '-';
    // (preset, min context, min input). `long-input` comes first, so it never resolves as `long`.
    const BUILTIN_PRESETS: [(&'static str, Option<u64>, Option<u64>); 2] = [
        ("long-input", None, Some(CONTEXT_TOKENS_1M)),
        ("long", Some(CONTEXT_TOKENS_1M), None),
    ];

    /// Resolves a `hub/*` model id. Any other id returns `Ok(None)` and keeps ordinary routing.
    /// Operator presets are keyed by the name after `hub/`.
    ///
    /// # Errors
    /// Returns `AppError::BadRequest` for a `hub/*` name that no tier or preset defines.
    pub fn parse(
        model_id: &str,
        operator_presets: &BTreeMap<String, SmartAutoPolicy>,
    ) -> Result<Option<HubSelector>, AppError> {
        let Some(name) = strip_1m_suffix(model_id)
            .strip_prefix(Self::NAMESPACE)
            .and_then(|rest| rest.strip_prefix(Self::NAMESPACE_SEPARATOR))
        else {
            return Ok(None);
        };
        let preset = Self::builtin_policy(name)
            .or_else(|| operator_presets.get(name).cloned())
            .ok_or_else(|| Self::unknown_name(name))?;
        let tier = preset.tier.unwrap_or_default();
        let policy = SmartAutoPolicy {
            tier: Some(tier),
            ..SmartAutoPolicy::default()
        }
        .merge(&preset);
        Ok(Some(HubSelector {
            name: name.to_string(),
            tier,
            policy,
        }))
    }

    /// Checks an operator preset name: ASCII letters, digits, `-`, `_` or `.`.
    /// Names under the `smart-auto` prefix are reserved, so no preset shadows a built-in.
    ///
    /// # Errors
    /// Returns a message naming the preset when the name is empty, malformed or reserved.
    pub fn validate_operator_preset_name(name: &str) -> Result<(), String> {
        let well_formed = !name.is_empty()
            && name
                .bytes()
                .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.'));
        if !well_formed {
            return Err(format!(
                "Smart Auto preset name {name:?} is malformed: use ASCII letters, digits, '-', '_' or '.'"
            ));
        }
        if name.starts_with(SmartAutoTier::SELECTOR) {
            return Err(format!(
                "Smart Auto preset name {name:?} is reserved: names that start with {:?} belong to built-in presets",
                SmartAutoTier::SELECTOR
            ));
        }
        Ok(())
    }

    /// Every built-in name after `hub/`: each tier, then each tier with each built-in preset.
    #[must_use]
    pub(crate) fn builtin_names() -> Vec<String> {
        let mut names = Vec::new();
        for tier in SmartAutoTier::ALL {
            names.push(tier.selector());
            for (preset, _, _) in Self::BUILTIN_PRESETS {
                names.push(format!(
                    "{}{sep}{preset}{sep}{}",
                    SmartAutoTier::SELECTOR,
                    tier.as_str(),
                    sep = Self::PRESET_SEPARATOR,
                ));
            }
        }
        names
    }

    fn builtin_policy(name: &str) -> Option<SmartAutoPolicy> {
        if let Some(tier) = SmartAutoTier::from_selector(name) {
            return Some(SmartAutoPolicy {
                tier: Some(tier),
                ..SmartAutoPolicy::default()
            });
        }
        let after_selector = name
            .strip_prefix(SmartAutoTier::SELECTOR)?
            .strip_prefix(Self::PRESET_SEPARATOR)?;
        Self::BUILTIN_PRESETS
            .iter()
            .find_map(|&(preset, min_context_tokens, min_input_tokens)| {
                let tier_suffix = after_selector.strip_prefix(preset)?;
                let tier_selector = format!("{}{tier_suffix}", SmartAutoTier::SELECTOR);
                Some(SmartAutoPolicy {
                    tier: Some(SmartAutoTier::from_selector(&tier_selector)?),
                    min_context_tokens,
                    min_input_tokens,
                    ..SmartAutoPolicy::default()
                })
            })
    }

    fn unknown_name(name: &str) -> AppError {
        AppError::BadRequest(format!(
            "unknown hub model {namespace}/{name}: use {selector}[-<tier>], {selector}-long[-<tier>], {selector}-long-input[-<tier>] or a configured Smart Auto preset",
            namespace = Self::NAMESPACE,
            selector = SmartAutoTier::SELECTOR,
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn resolve(model_id: &str) -> HubSelector {
        HubSelector::parse(model_id, &BTreeMap::new())
            .unwrap()
            .unwrap_or_else(|| panic!("{model_id} is not a hub selector"))
    }

    fn tier_only(tier: SmartAutoTier) -> SmartAutoPolicy {
        SmartAutoPolicy {
            tier: Some(tier),
            ..SmartAutoPolicy::default()
        }
    }

    #[test]
    fn every_builtin_name_resolves_to_a_selector() {
        let names = HubSelector::builtin_names();

        assert_eq!(
            names.len(),
            SmartAutoTier::ALL.len() * (1 + HubSelector::BUILTIN_PRESETS.len())
        );
        for name in &names {
            let selector = resolve(&format!("hub/{name}"));
            assert_eq!(&selector.name, name);
        }
        assert!(names.contains(&"smart-auto-long-input-fast".to_string()));
    }

    #[test]
    fn every_tier_selector_resolves_to_its_tier() {
        for tier in SmartAutoTier::ALL {
            let selector = resolve(&format!("hub/{}", tier.selector()));
            assert_eq!(selector.tier, tier);
            assert_eq!(selector.policy, tier_only(tier));
            assert_eq!(selector.name, tier.selector());
        }
    }

    #[test]
    fn bare_smart_auto_defaults_to_balance() {
        let selector = resolve("hub/smart-auto");
        assert_eq!(selector.tier, SmartAutoTier::Balance);
        assert_eq!(selector.policy, tier_only(SmartAutoTier::Balance));
    }

    #[test]
    fn one_million_context_suffix_is_tolerated() {
        for model_id in [
            "hub/smart-auto-fast[1m]",
            "hub/smart-auto-fast[1M]",
            " hub/smart-auto-fast ",
        ] {
            let selector = resolve(model_id);
            assert_eq!(selector.name, "smart-auto-fast", "{model_id}");
            assert_eq!(
                selector.policy,
                tier_only(SmartAutoTier::Fast),
                "{model_id}"
            );
        }
    }

    #[test]
    fn non_hub_ids_keep_ordinary_routing() {
        for model_id in [
            "copilot/gpt-5",
            "copilot/auto",
            "smart-auto-balance",
            "hub",
            "hubby/smart-auto",
            "HUB/smart-auto",
            "llmgw/hub/smart-auto",
            "",
        ] {
            assert!(
                HubSelector::parse(model_id, &BTreeMap::new())
                    .unwrap()
                    .is_none(),
                "{model_id}"
            );
        }
    }

    #[test]
    fn unknown_hub_names_are_explicit_errors() {
        for model_id in [
            "hub/smart-auto-xyz",
            "hub/smart-auto-",
            "hub/smart-auto-long-xyz",
            "hub/smart-auto-longbalance",
            "hub/smart-auto-input-balance",
            "hub/",
            "hub/team",
        ] {
            let error = HubSelector::parse(model_id, &BTreeMap::new()).unwrap_err();
            let AppError::BadRequest(message) = error else {
                panic!("{model_id}: expected BadRequest");
            };
            assert!(message.contains("unknown hub model"), "{message}");
        }
    }

    #[test]
    fn long_preset_requires_one_million_context_tokens() {
        for tier in SmartAutoTier::ALL {
            let selector = resolve(&format!("hub/smart-auto-long-{}", tier.as_str()));
            assert_eq!(selector.tier, tier);
            assert_eq!(
                selector.policy,
                SmartAutoPolicy {
                    min_context_tokens: Some(1_000_000),
                    ..tier_only(tier)
                }
            );
        }
        assert_eq!(resolve("hub/smart-auto-long").tier, SmartAutoTier::Balance);
    }

    #[test]
    fn long_input_preset_requires_one_million_input_tokens() {
        let selector = resolve("hub/smart-auto-long-input-intelligence[1m]");
        assert_eq!(selector.name, "smart-auto-long-input-intelligence");
        assert_eq!(
            selector.policy,
            SmartAutoPolicy {
                min_input_tokens: Some(1_000_000),
                ..tier_only(SmartAutoTier::Intelligence)
            }
        );
        assert_eq!(
            resolve("hub/smart-auto-long-input").policy,
            SmartAutoPolicy {
                min_input_tokens: Some(1_000_000),
                ..tier_only(SmartAutoTier::Balance)
            }
        );
    }

    #[test]
    fn operator_preset_resolves_to_its_policy() {
        let team = SmartAutoPolicy {
            tier: Some(SmartAutoTier::Efficiency),
            include: vec![["copilot/*".to_string()].into()],
            min_output_tokens: Some(32_000),
            ..SmartAutoPolicy::default()
        };
        let untiered = SmartAutoPolicy {
            include: vec![std::collections::BTreeSet::new()],
            exclude: ["*preview*".to_string()].into(),
            ..SmartAutoPolicy::default()
        };
        let presets = BTreeMap::from([
            ("team".to_string(), team.clone()),
            ("no-preview".to_string(), untiered),
        ]);

        let selector = HubSelector::parse("hub/team", &presets).unwrap().unwrap();
        assert_eq!(selector.tier, SmartAutoTier::Efficiency);
        assert_eq!(selector.policy, team);

        let selector = HubSelector::parse("hub/no-preview", &presets)
            .unwrap()
            .unwrap();
        assert_eq!(selector.tier, SmartAutoTier::Balance);
        assert_eq!(
            selector.policy,
            SmartAutoPolicy {
                exclude: ["*preview*".to_string()].into(),
                ..tier_only(SmartAutoTier::Balance)
            }
        );
    }

    #[test]
    fn builtin_names_win_over_operator_presets() {
        let presets = BTreeMap::from([(
            "smart-auto-balance".to_string(),
            SmartAutoPolicy {
                tier: Some(SmartAutoTier::Fast),
                ..SmartAutoPolicy::default()
            },
        )]);
        let selector = HubSelector::parse("hub/smart-auto-balance", &presets)
            .unwrap()
            .unwrap();
        assert_eq!(selector.policy, tier_only(SmartAutoTier::Balance));
    }

    #[test]
    fn operator_preset_names_are_validated() {
        for name in ["team", "team-long_v2.1", "Team"] {
            assert!(
                HubSelector::validate_operator_preset_name(name).is_ok(),
                "{name}"
            );
        }
        for name in ["", "a/b", "team long", "team[1m]"] {
            let error = HubSelector::validate_operator_preset_name(name).unwrap_err();
            assert!(error.contains("malformed"), "{name}: {error}");
        }
        for name in ["smart-auto", "smart-auto-balance", "smart-auto-team"] {
            let error = HubSelector::validate_operator_preset_name(name).unwrap_err();
            assert!(error.contains("reserved"), "{name}: {error}");
        }
    }
}
