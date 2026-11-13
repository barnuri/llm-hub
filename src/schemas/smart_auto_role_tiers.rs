use crate::schemas::smart_auto_tier::SmartAutoTier;
use crate::schemas::work_role::WorkRole;

/// The tier a client should pick for each work role when it has no other preference.
/// Advisory only: the hub never applies it, and `GET /api/smart-auto/tiers` publishes it.
pub const ROLE_TIER_DEFAULTS: [(WorkRole, SmartAutoTier); 5] = [
    (WorkRole::Planning, SmartAutoTier::Intelligence),
    (WorkRole::Investigation, SmartAutoTier::Balance),
    (WorkRole::Implementation, SmartAutoTier::Balance),
    (WorkRole::Validation, SmartAutoTier::Efficiency),
    (WorkRole::Conversation, SmartAutoTier::Fast),
];

/// The role with this name, ignoring ASCII case, e.g. `planning`.
#[must_use]
pub fn role_from_name(name: &str) -> Option<WorkRole> {
    ROLE_TIER_DEFAULTS
        .into_iter()
        .map(|(role, _)| role)
        .find(|role| role.as_str().eq_ignore_ascii_case(name))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_role_has_one_default_tier() {
        for (role, _) in ROLE_TIER_DEFAULTS {
            assert_eq!(role_from_name(role.as_str()), Some(role));
            assert_eq!(
                ROLE_TIER_DEFAULTS
                    .iter()
                    .filter(|(r, _)| *r == role)
                    .count(),
                1
            );
        }
        assert_eq!(role_from_name("PLANNING"), Some(WorkRole::Planning));
        assert_eq!(role_from_name("coding"), None);
    }

    #[test]
    fn planning_defaults_to_intelligence_and_conversation_to_fast() {
        let tier_of = |role| {
            ROLE_TIER_DEFAULTS
                .iter()
                .find(|(r, _)| *r == role)
                .map(|(_, tier)| *tier)
        };
        assert_eq!(
            tier_of(WorkRole::Planning),
            Some(SmartAutoTier::Intelligence)
        );
        assert_eq!(
            tier_of(WorkRole::Validation),
            Some(SmartAutoTier::Efficiency)
        );
        assert_eq!(tier_of(WorkRole::Conversation), Some(SmartAutoTier::Fast));
    }
}
