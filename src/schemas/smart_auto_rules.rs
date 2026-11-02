use std::collections::{BTreeMap, BTreeSet};

use serde::{Deserialize, Serialize};

pub mod profile_rules;
pub mod smart_auto_rules_mode;

use crate::schemas::smart_auto_policy::SmartAutoPolicy;
pub use profile_rules::ProfileRules;
pub use smart_auto_rules_mode::SmartAutoRulesMode;

/// The user-editable Smart Auto rules: global allow, exclude and include globs, and per-profile ones.
/// Globs match the qualified `<profile>/<model>` id and the bare model id.
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq, Default)]
#[serde(default, deny_unknown_fields)]
pub struct SmartAutoRules {
    pub include: Vec<String>,
    pub exclude: Vec<String>,
    pub allow: Vec<String>,
    pub profiles: BTreeMap<String, ProfileRules>,
}

impl SmartAutoRules {
    /// Parses the rules JSON and checks every glob.
    ///
    /// # Errors
    /// Returns a message for invalid JSON, an unknown field, an empty glob or a glob with whitespace.
    pub fn from_json(raw: &[u8]) -> Result<Self, String> {
        let rules: SmartAutoRules =
            serde_json::from_slice(raw).map_err(|e| format!("invalid rules JSON: {e}"))?;
        rules.validate()?;
        Ok(rules)
    }

    /// Checks that no glob is empty or contains whitespace, and names the list that holds it.
    ///
    /// # Errors
    /// Returns a message that names the bad glob and its list.
    pub fn validate(&self) -> Result<(), String> {
        Self::check_globs("include", &self.include)?;
        Self::check_globs("exclude", &self.exclude)?;
        Self::check_globs("allow", &self.allow)?;
        for (profile, rules) in &self.profiles {
            Self::check_globs(&format!("profiles.{profile}.include"), &rules.include)?;
            Self::check_globs(&format!("profiles.{profile}.exclude"), &rules.exclude)?;
        }
        Ok(())
    }

    /// The policy these rules add. Empty lists add no restriction.
    #[must_use]
    pub fn to_policy(&self) -> SmartAutoPolicy {
        let mut policy = SmartAutoPolicy {
            exclude: Self::globs(&self.exclude),
            allow: Self::globs(&self.allow),
            ..SmartAutoPolicy::default()
        };
        if !self.include.is_empty() {
            policy.include.push(Self::globs(&self.include));
        }
        for (profile, rules) in &self.profiles {
            if !rules.include.is_empty() {
                policy
                    .profile_include
                    .insert(profile.clone(), vec![Self::globs(&rules.include)]);
            }
            if !rules.exclude.is_empty() {
                policy
                    .profile_exclude
                    .insert(profile.clone(), Self::globs(&rules.exclude));
            }
        }
        policy
    }

    fn check_globs(location: &str, values: &[String]) -> Result<(), String> {
        for glob in values {
            if glob.is_empty() {
                return Err(format!("{location} has an empty glob: remove the entry"));
            }
            if glob.chars().any(char::is_whitespace) {
                return Err(format!("{location} glob {glob:?} contains whitespace"));
            }
        }
        Ok(())
    }

    fn globs(values: &[String]) -> BTreeSet<String> {
        values.iter().cloned().collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn empty_glob_and_whitespace_are_rejected_with_their_location() {
        let empty = SmartAutoRules::from_json(br#"{"exclude":[""]}"#).unwrap_err();
        assert!(
            empty.contains("exclude") && empty.contains("empty"),
            "{empty}"
        );
        let spaced =
            SmartAutoRules::from_json(br#"{"profiles":{"copilot":{"include":["gpt 5"]}}}"#)
                .unwrap_err();
        assert!(
            spaced.contains("profiles.copilot.include") && spaced.contains("whitespace"),
            "{spaced}"
        );
    }

    #[test]
    fn invalid_json_and_unknown_fields_are_rejected() {
        assert!(
            SmartAutoRules::from_json(b"")
                .unwrap_err()
                .contains("invalid")
        );
        assert!(SmartAutoRules::from_json(br#"{"includes":[]}"#).is_err());
    }

    #[test]
    fn empty_lists_add_no_restriction() {
        let rules = SmartAutoRules::from_json(
            br#"{"include":[],"profiles":{"copilot":{"include":[],"exclude":[]}}}"#,
        )
        .unwrap();
        assert_eq!(rules.to_policy(), SmartAutoPolicy::default());
    }

    #[test]
    fn rules_map_to_global_and_profile_policy_fields() {
        let rules = SmartAutoRules::from_json(
            br#"{"include":["copilot/*"],"exclude":["*preview*"],"allow":["copilot/gpt-6-preview"],
                "profiles":{"llama_swap":{"include":["qwen*"],"exclude":["*large*"]}}}"#,
        )
        .unwrap();
        let policy = rules.to_policy();
        assert_eq!(
            policy.include,
            vec![SmartAutoRules::globs(&["copilot/*".to_string()])]
        );
        assert_eq!(
            policy.exclude,
            SmartAutoRules::globs(&["*preview*".to_string()])
        );
        assert_eq!(
            policy.allow,
            SmartAutoRules::globs(&["copilot/gpt-6-preview".to_string()])
        );
        assert_eq!(
            policy.profile_include["llama_swap"],
            vec![SmartAutoRules::globs(&["qwen*".to_string()])]
        );
        assert_eq!(
            policy.profile_exclude["llama_swap"],
            SmartAutoRules::globs(&["*large*".to_string()])
        );
    }

    #[test]
    fn allow_globs_are_checked_like_the_other_lists() {
        let empty = SmartAutoRules::from_json(br#"{"allow":[""]}"#).unwrap_err();
        assert!(
            empty.contains("allow") && empty.contains("empty"),
            "{empty}"
        );
        let spaced = SmartAutoRules::from_json(br#"{"allow":["gpt 5"]}"#).unwrap_err();
        assert!(
            spaced.contains("allow") && spaced.contains("whitespace"),
            "{spaced}"
        );
    }

    #[test]
    fn files_saved_before_allow_existed_still_load_with_no_allow() {
        let rules =
            SmartAutoRules::from_json(br#"{"include":["copilot/*"],"exclude":["*x*"]}"#).unwrap();
        assert!(rules.allow.is_empty());
        assert!(rules.to_policy().allow.is_empty());
    }

    #[test]
    fn mode_names_parse_without_regard_to_case() {
        assert_eq!(
            SmartAutoRulesMode::from_name("merge"),
            Some(SmartAutoRulesMode::Merge)
        );
        assert_eq!(
            SmartAutoRulesMode::from_name("Override"),
            Some(SmartAutoRulesMode::Override)
        );
        assert_eq!(SmartAutoRulesMode::from_name("widen"), None);
    }
}
