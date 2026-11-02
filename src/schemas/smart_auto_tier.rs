use serde::{Deserialize, Serialize};

use crate::services::copilot_auto::AutoTier;

/// The Smart Auto cost, quality and speed policy a caller selects.
/// Serde names match the `smart-auto-<tier>` selector suffixes.
#[derive(Serialize, Deserialize, Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
#[serde(rename_all = "snake_case")]
pub enum SmartAutoTier {
    /// The cheapest capable worker, with no automatic reviewer.
    Efficiency,
    /// A role-fit solver, reviewed only when review adds value.
    #[default]
    Balance,
    /// The strongest eligible role-fit solver, with independent review for difficult work.
    Intelligence,
    /// The capable worker with the lowest measured latency.
    Fast,
}

impl SmartAutoTier {
    /// The bare selector that names the default tier.
    pub const SELECTOR: &'static str = "smart-auto";
    /// Every tier, in cost order.
    pub const ALL: [SmartAutoTier; 4] = [
        SmartAutoTier::Efficiency,
        SmartAutoTier::Balance,
        SmartAutoTier::Intelligence,
        SmartAutoTier::Fast,
    ];
    const SELECTOR_SEPARATOR: char = '-';

    /// The tier of a bare selector: `smart-auto` is Balance and `smart-auto-<tier>` names one.
    /// Any other name returns `None`.
    #[must_use]
    pub fn from_selector(bare_selector: &str) -> Option<SmartAutoTier> {
        if bare_selector == Self::SELECTOR {
            return Some(SmartAutoTier::default());
        }
        Self::from_name(
            bare_selector
                .strip_prefix(Self::SELECTOR)?
                .strip_prefix(Self::SELECTOR_SEPARATOR)?,
        )
    }

    /// The tier with this name, ignoring ASCII case, e.g. `balance`.
    #[must_use]
    pub fn from_name(name: &str) -> Option<SmartAutoTier> {
        Self::ALL
            .into_iter()
            .find(|tier| tier.as_str().eq_ignore_ascii_case(name))
    }

    /// The tier name, shared with the Copilot Auto tier of the same name, e.g. `balance`.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        AutoTier::from(self).as_str()
    }

    /// The canonical bare selector, e.g. `smart-auto-balance`.
    #[must_use]
    #[allow(dead_code)] // The `/v1/models` listing names each tier through this.
    pub fn selector(self) -> String {
        format!(
            "{}{}{}",
            Self::SELECTOR,
            Self::SELECTOR_SEPARATOR,
            self.as_str()
        )
    }
}

impl From<SmartAutoTier> for AutoTier {
    fn from(tier: SmartAutoTier) -> AutoTier {
        match tier {
            SmartAutoTier::Efficiency => AutoTier::Efficiency,
            SmartAutoTier::Balance => AutoTier::Balance,
            SmartAutoTier::Intelligence => AutoTier::Intelligence,
            SmartAutoTier::Fast => AutoTier::Fast,
        }
    }
}

impl From<AutoTier> for SmartAutoTier {
    fn from(tier: AutoTier) -> SmartAutoTier {
        match tier {
            AutoTier::Efficiency => SmartAutoTier::Efficiency,
            AutoTier::Balance => SmartAutoTier::Balance,
            AutoTier::Intelligence => SmartAutoTier::Intelligence,
            AutoTier::Fast => SmartAutoTier::Fast,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_tier_is_balance() {
        assert_eq!(SmartAutoTier::default(), SmartAutoTier::Balance);
    }

    #[test]
    fn bare_smart_auto_aliases_balance() {
        assert_eq!(
            SmartAutoTier::from_selector("smart-auto"),
            Some(SmartAutoTier::Balance)
        );
    }

    #[test]
    fn every_canonical_selector_round_trips() {
        for tier in SmartAutoTier::ALL {
            assert_eq!(SmartAutoTier::from_selector(&tier.selector()), Some(tier));
        }
        assert_eq!(
            SmartAutoTier::Intelligence.selector(),
            "smart-auto-intelligence"
        );
    }

    #[test]
    fn unknown_or_malformed_selectors_are_rejected() {
        for name in [
            "smart-auto-",
            "smart-auto-max",
            "smart-autobalance",
            "auto-balance",
            "smart-auto-long-balance",
            "",
        ] {
            assert_eq!(SmartAutoTier::from_selector(name), None, "{name}");
        }
    }

    #[test]
    fn serde_names_match_selector_suffixes() {
        for tier in SmartAutoTier::ALL {
            let json = serde_json::to_string(&tier).unwrap();
            assert_eq!(json, format!("\"{}\"", tier.as_str()));
            assert_eq!(serde_json::from_str::<SmartAutoTier>(&json).unwrap(), tier);
            assert!(tier.selector().ends_with(tier.as_str()));
        }
    }

    #[test]
    fn converts_to_and_from_copilot_auto_tier() {
        for tier in SmartAutoTier::ALL {
            assert_eq!(SmartAutoTier::from(AutoTier::from(tier)), tier);
        }
        assert_eq!(
            SmartAutoTier::from(AutoTier::default()),
            SmartAutoTier::default()
        );
    }
}
