use std::collections::{BTreeMap, BTreeSet};

use serde_json::Value;

use crate::schemas::hub_selector::HubSelector;
use crate::schemas::smart_auto_headers::SmartAutoHeaders;
use crate::schemas::smart_auto_policy::SmartAutoPolicy;
use crate::schemas::smart_auto_tier::SmartAutoTier;
use crate::schemas::work_role::WorkRole;

/// The `--smart-auto-*` launch flags: a Smart Auto policy that a launcher sends as headers or maps to a preset id.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct SmartAutoFlags {
    policy: SmartAutoPolicy,
}

impl SmartAutoFlags {
    const FLAG_PREFIX: &str = "--smart-auto-";
    const TIER_FLAG: &str = "--smart-auto-tier";
    const MIN_CONTEXT_FLAG: &str = "--smart-auto-min-context";
    const MIN_INPUT_FLAG: &str = "--smart-auto-min-input";
    const INCLUDE_FLAG: &str = "--smart-auto-include";
    const EXCLUDE_FLAG: &str = "--smart-auto-exclude";
    const ROLE_FLAG: &str = "--smart-auto-role";
    const END_OF_OPTIONS: &str = "--";
    /// Smart Auto flags that a preset id cannot carry; the Claude launcher sends these as headers.
    const PRESET_ONLY: &str = "Smart Auto flags other than a tier or a 1M minimum need the claude launcher, which sends them as headers";
    const LIST_SEPARATOR: &str = ",";
    const ROLE_NAMES: &str = "planning, investigation, implementation, validation or conversation";

    /// True when no Smart Auto flag was given.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.policy == SmartAutoPolicy::default()
    }

    /// The tier the flags name, if any.
    #[must_use]
    pub fn tier(&self) -> Option<SmartAutoTier> {
        self.policy.tier
    }

    /// Removes every `--smart-auto-*` flag at the front of `args`, stopping at the first other argument.
    ///
    /// # Errors
    /// Returns a message for a flag without a value, an unknown flag or a bad value.
    pub fn take_leading_flags(&mut self, args: &mut Vec<String>) -> Result<(), String> {
        while let Some((flag, value)) = Self::take_flag_at(args, 0)? {
            self.apply(&flag, &value)?;
        }
        Ok(())
    }

    /// Removes every `--smart-auto-*` flag before `--`.
    ///
    /// # Errors
    /// Returns a message for a flag without a value, an unknown flag or a bad value.
    pub fn take_flags(&mut self, args: &mut Vec<String>) -> Result<(), String> {
        let mut index = 0;
        while index < args.len() && args[index] != Self::END_OF_OPTIONS {
            match Self::take_flag_at(args, index)? {
                Some((flag, value)) => self.apply(&flag, &value)?,
                None => index += 1,
            }
        }
        Ok(())
    }

    /// The `Name: value` lines that send these flags as Smart Auto request headers.
    #[must_use]
    pub fn header_lines(&self) -> Vec<String> {
        let policy = &self.policy;
        let mut lines = Vec::new();
        if let Some(tier) = policy.tier {
            lines.push(Self::header_line(SmartAutoHeaders::TIER, tier.as_str()));
        }
        for allow_list in &policy.include {
            lines.push(Self::header_line(
                SmartAutoHeaders::INCLUDE,
                &Self::join_globs(allow_list),
            ));
        }
        if !policy.exclude.is_empty() {
            lines.push(Self::header_line(
                SmartAutoHeaders::EXCLUDE,
                &Self::join_globs(&policy.exclude),
            ));
        }
        if let Some(tokens) = policy.min_context_tokens {
            lines.push(Self::header_line(
                SmartAutoHeaders::MIN_CONTEXT,
                &tokens.to_string(),
            ));
        }
        if let Some(tokens) = policy.min_input_tokens {
            lines.push(Self::header_line(
                SmartAutoHeaders::MIN_INPUT,
                &tokens.to_string(),
            ));
        }
        if let Some(role) = policy.role {
            lines.push(Self::header_line(SmartAutoHeaders::ROLE, role.as_str()));
        }
        lines
    }

    /// The built-in Smart Auto model id that carries exactly these flags, for a launcher without headers.
    ///
    /// # Errors
    /// Returns a message when the flags are more than a tier or a built-in one-million minimum.
    pub fn preset_model(&self) -> Result<String, String> {
        let mut wanted = self.policy.clone();
        if wanted.tier.is_none() {
            wanted.tier = Some(SmartAutoTier::default());
        }
        HubSelector::builtin_names()
            .into_iter()
            .find(|name| {
                HubSelector::parse(&Self::qualified(name), &BTreeMap::new())
                    .ok()
                    .flatten()
                    .is_some_and(|selector| selector.policy == wanted)
            })
            .map(|name| Self::qualified(&name))
            .ok_or_else(|| Self::PRESET_ONLY.to_string())
    }

    fn apply(&mut self, flag: &str, value: &str) -> Result<(), String> {
        let value = value.trim();
        match flag {
            Self::TIER_FLAG => {
                Self::set_once(flag, &mut self.policy.tier, Self::parse_tier(flag, value)?)
            }
            Self::MIN_CONTEXT_FLAG => Self::set_once(
                flag,
                &mut self.policy.min_context_tokens,
                Self::parse_tokens(flag, value)?,
            ),
            Self::MIN_INPUT_FLAG => Self::set_once(
                flag,
                &mut self.policy.min_input_tokens,
                Self::parse_tokens(flag, value)?,
            ),
            Self::INCLUDE_FLAG => {
                let globs = Self::parse_list(flag, value)?;
                match self.policy.include.first_mut() {
                    Some(allow_list) => allow_list.extend(globs),
                    None => self.policy.include.push(globs),
                }
                Ok(())
            }
            Self::EXCLUDE_FLAG => {
                self.policy.exclude.extend(Self::parse_list(flag, value)?);
                Ok(())
            }
            Self::ROLE_FLAG => {
                Self::set_once(flag, &mut self.policy.role, Self::parse_role(flag, value)?)
            }
            _ => Err(format!(
                "unknown flag {flag}: use {}, {}, {}, {}, {} or {}",
                Self::TIER_FLAG,
                Self::MIN_CONTEXT_FLAG,
                Self::MIN_INPUT_FLAG,
                Self::INCLUDE_FLAG,
                Self::EXCLUDE_FLAG,
                Self::ROLE_FLAG,
            )),
        }
    }

    /// The flag and value at `args[index]`, removed from `args`, when that argument is a Smart Auto flag.
    fn take_flag_at(
        args: &mut Vec<String>,
        index: usize,
    ) -> Result<Option<(String, String)>, String> {
        let Some(arg) = args.get(index).cloned() else {
            return Ok(None);
        };
        let (flag, inline) = match arg.split_once('=') {
            Some((flag, value)) => (flag.to_string(), Some(value.to_string())),
            None => (arg.clone(), None),
        };
        if !flag.starts_with(Self::FLAG_PREFIX) {
            return Ok(None);
        }
        if let Some(value) = inline {
            args.remove(index);
            return Ok(Some((flag, value)));
        }
        let value = args
            .get(index + 1)
            .filter(|value| !value.starts_with('-'))
            .cloned()
            .ok_or_else(|| format!("{flag} needs a value, e.g. {flag} balance"))?;
        args.drain(index..=index + 1);
        Ok(Some((flag, value)))
    }

    fn qualified(name: &str) -> String {
        format!("{}/{name}", HubSelector::NAMESPACE)
    }

    fn header_line(name: &str, value: &str) -> String {
        format!("{name}: {value}")
    }

    fn join_globs(globs: &BTreeSet<String>) -> String {
        globs
            .iter()
            .map(String::as_str)
            .collect::<Vec<_>>()
            .join(Self::LIST_SEPARATOR)
    }

    fn set_once<T>(flag: &str, slot: &mut Option<T>, value: T) -> Result<(), String> {
        if slot.is_some() {
            return Err(format!("{flag} is given more than once"));
        }
        *slot = Some(value);
        Ok(())
    }

    fn parse_tier(flag: &str, value: &str) -> Result<SmartAutoTier, String> {
        SmartAutoTier::from_name(value).ok_or_else(|| {
            let names = SmartAutoTier::ALL.map(SmartAutoTier::as_str).join(", ");
            format!("{flag}: unknown tier {value:?}: use one of {names}")
        })
    }

    fn parse_tokens(flag: &str, value: &str) -> Result<u64, String> {
        SmartAutoPolicy::parse_token_count(value)
            .map_err(|error| format!("{flag}: {}", error.message()))
    }

    fn parse_role(flag: &str, value: &str) -> Result<WorkRole, String> {
        serde_json::from_value(Value::String(value.to_string()))
            .map_err(|_| format!("{flag}: unknown role {value:?}: use {}", Self::ROLE_NAMES))
    }

    fn parse_list(flag: &str, value: &str) -> Result<BTreeSet<String>, String> {
        let mut globs = BTreeSet::new();
        for glob in value.split(Self::LIST_SEPARATOR).map(str::trim) {
            if glob.is_empty() {
                return Err(format!("{flag} has an empty entry in {value:?}"));
            }
            globs.insert(glob.to_string());
        }
        Ok(globs)
    }
}

#[cfg(test)]
mod tests {
    use axum::http::{HeaderMap, HeaderName, HeaderValue};

    use super::*;
    use crate::consts::CONTEXT_TOKENS_1M;

    fn strings(args: &[&str]) -> Vec<String> {
        args.iter().map(ToString::to_string).collect()
    }

    fn flags_of(args: &[&str]) -> SmartAutoFlags {
        let mut flags = SmartAutoFlags::default();
        flags.take_flags(&mut strings(args)).unwrap();
        flags
    }

    #[test]
    fn flags_are_taken_in_both_forms_from_both_positions() {
        let mut args = strings(&[
            "--smart-auto-tier=fast",
            "--smart-auto-min-context",
            "1m",
            "claude",
            "--smart-auto-role",
            "planning",
        ]);
        let mut flags = SmartAutoFlags::default();

        flags.take_leading_flags(&mut args).unwrap();
        assert_eq!(args, ["claude", "--smart-auto-role", "planning"]);
        flags.take_flags(&mut args).unwrap();

        assert_eq!(args, ["claude"]);
        assert_eq!(flags.policy.tier, Some(SmartAutoTier::Fast));
        assert_eq!(flags.policy.min_context_tokens, Some(CONTEXT_TOKENS_1M));
        assert_eq!(flags.policy.role, Some(WorkRole::Planning));
    }

    #[test]
    fn flags_after_end_of_options_stay_in_the_arguments() {
        let mut args = strings(&[
            "m",
            "--smart-auto-tier",
            "fast",
            "--",
            "--smart-auto-tier",
            "x",
        ]);
        let mut flags = SmartAutoFlags::default();

        flags.take_flags(&mut args).unwrap();

        assert_eq!(args, ["m", "--", "--smart-auto-tier", "x"]);
        assert_eq!(flags.tier(), Some(SmartAutoTier::Fast));
    }

    #[test]
    fn repeated_scalar_flag_is_an_error() {
        let mut flags = SmartAutoFlags::default();

        let error = flags
            .take_flags(&mut strings(&[
                "--smart-auto-tier=fast",
                "--smart-auto-tier",
                "balance",
            ]))
            .unwrap_err();

        assert!(error.contains("more than once"), "{error}");
    }

    #[test]
    fn bad_values_and_flags_are_named_errors() {
        let cases: [&[&str]; 7] = [
            &["--smart-auto-tier=max"],
            &["--smart-auto-min-context=1g"],
            &["--smart-auto-role=coding"],
            &["--smart-auto-include=copilot/*,"],
            &["--smart-auto-tier"],
            &["--smart-auto-bogus=x"],
            &["--smart-auto-tier=balance", "--smart-auto-min-input", "--x"],
        ];
        for args in cases {
            let error = SmartAutoFlags::default()
                .take_flags(&mut strings(args))
                .unwrap_err();
            assert!(error.contains("--smart-auto"), "{error}");
        }
    }

    #[test]
    fn include_flags_share_one_list_and_excludes_union() {
        let flags = flags_of(&[
            "--smart-auto-include=copilot/*,local/*",
            "--smart-auto-include",
            "copilot/gpt-*",
            "--smart-auto-exclude=*preview*",
            "--smart-auto-exclude",
            "*mini*",
        ]);

        assert_eq!(
            flags.policy.include,
            vec![BTreeSet::from([
                "copilot/*".to_string(),
                "copilot/gpt-*".to_string(),
                "local/*".to_string(),
            ])]
        );
        assert_eq!(
            flags.policy.exclude,
            BTreeSet::from(["*mini*".to_string(), "*preview*".to_string()])
        );
    }

    #[test]
    fn header_lines_round_trip_through_the_hub_header_parser() {
        let flags = flags_of(&[
            "--smart-auto-tier=efficiency",
            "--smart-auto-min-context",
            "1m",
            "--smart-auto-min-input=200k",
            "--smart-auto-include=copilot/*",
            "--smart-auto-exclude=openai/*",
            "--smart-auto-role=implementation",
        ]);
        let mut headers = HeaderMap::new();
        for line in flags.header_lines() {
            let (name, value) = line.split_once(": ").unwrap();
            headers.append(
                HeaderName::from_bytes(name.as_bytes()).unwrap(),
                HeaderValue::from_str(value).unwrap(),
            );
        }

        let received = SmartAutoHeaders::from_headers(&headers).unwrap();

        assert_eq!(received.policy, flags.policy);
        assert!(headers.contains_key(SmartAutoHeaders::MIN_CONTEXT));
    }

    #[test]
    fn no_flags_send_no_headers() {
        assert!(SmartAutoFlags::default().is_empty());
        assert!(SmartAutoFlags::default().header_lines().is_empty());
    }

    #[test]
    fn flags_map_to_the_built_in_preset_ids() {
        assert_eq!(
            SmartAutoFlags::default().preset_model().unwrap(),
            "hub/smart-auto-balance"
        );
        assert_eq!(
            flags_of(&["--smart-auto-tier=intelligence"])
                .preset_model()
                .unwrap(),
            "hub/smart-auto-intelligence"
        );
        assert_eq!(
            flags_of(&["--smart-auto-min-context=1m"])
                .preset_model()
                .unwrap(),
            "hub/smart-auto-long-balance"
        );
        assert_eq!(
            flags_of(&["--smart-auto-min-input", "1m", "--smart-auto-tier=fast"])
                .preset_model()
                .unwrap(),
            "hub/smart-auto-long-input-fast"
        );
    }

    #[test]
    fn flags_beyond_a_preset_are_refused() {
        assert!(
            flags_of(&["--smart-auto-min-context=200k"])
                .preset_model()
                .is_err()
        );
        assert!(
            flags_of(&["--smart-auto-role=planning"])
                .preset_model()
                .is_err()
        );
        assert!(
            flags_of(&["--smart-auto-exclude=openai/*"])
                .preset_model()
                .is_err()
        );
    }
}
