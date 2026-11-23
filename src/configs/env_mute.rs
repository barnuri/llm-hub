use crate::schemas::notification::MuteMatch;
use crate::services::notifications::mute::{MAX_GLOB_CHARS, MAX_MUTE_RULES};

/// The parser of `LLM_HUB_NOTIFY_MUTE`. Each rule becomes a [`MuteMatch`] whose set fields hold
/// globs, and a notification is muted when every set field matches it.
#[derive(Debug, Clone, Copy)]
pub struct EnvMute;

impl EnvMute {
    const KEY: &str = "LLM_HUB_NOTIFY_MUTE";

    /// Parses `;`-separated rules, each a `,`-separated list of `field=glob`. Empty rules are
    /// skipped.
    ///
    /// # Errors
    /// A message naming the key when a field is unknown or repeated, a glob is empty or too long,
    /// or the list holds more rules than the cap.
    pub fn parse_list(raw: &str) -> Result<Vec<MuteMatch>, String> {
        let mutes = raw
            .split(';')
            .map(str::trim)
            .filter(|rule| !rule.is_empty())
            .map(Self::parse_rule)
            .collect::<Result<Vec<_>, _>>()?;
        if mutes.len() > MAX_MUTE_RULES {
            return Err(format!(
                "{} holds {} rules. The limit is {MAX_MUTE_RULES} rules.",
                Self::KEY,
                mutes.len(),
            ));
        }
        Ok(mutes)
    }

    fn parse_rule(rule: &str) -> Result<MuteMatch, String> {
        let mut mute = MuteMatch::default();
        for part in rule.split(',').map(str::trim) {
            let (field, glob) = part.split_once('=').ok_or_else(|| {
                format!(
                    "{} rule {rule:?} has a part without field=glob: {part:?}",
                    Self::KEY
                )
            })?;
            let (field, glob) = (field.trim(), glob.trim());
            if glob.is_empty() {
                return Err(format!(
                    "{} rule {rule:?} has an empty glob for {field:?}",
                    Self::KEY
                ));
            }
            if glob.chars().count() > MAX_GLOB_CHARS {
                return Err(format!(
                    "{} rule {rule:?} has a glob for {field:?} longer than {MAX_GLOB_CHARS} characters",
                    Self::KEY,
                ));
            }
            let slot = Self::field_mut(&mut mute, field).ok_or_else(|| {
                format!(
                    "{} rule {rule:?} has the unknown field {field:?}. Use source, kind, rule, profile, model or title.",
                    Self::KEY
                )
            })?;
            if slot.replace(glob.to_owned()).is_some() {
                return Err(format!(
                    "{} rule {rule:?} sets {field:?} more than once",
                    Self::KEY
                ));
            }
        }
        Ok(mute)
    }

    fn field_mut<'a>(mute: &'a mut MuteMatch, field: &str) -> Option<&'a mut Option<String>> {
        match field {
            "source" => Some(&mut mute.source),
            "kind" => Some(&mut mute.kind),
            "rule" => Some(&mut mute.rule),
            "profile" => Some(&mut mute.profile),
            "model" => Some(&mut mute.model),
            "title" => Some(&mut mute.title),
            _ => None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_rules_with_every_field_and_skips_empty_rules() {
        let mutes = EnvMute::parse_list(
            " source=errors ;; source=local_tuner, model=*/gemma-* ;kind=custom,rule=r1,profile=local,title=Disk *;",
        )
        .unwrap();
        assert_eq!(
            mutes,
            vec![
                MuteMatch {
                    source: Some("errors".into()),
                    ..MuteMatch::default()
                },
                MuteMatch {
                    source: Some("local_tuner".into()),
                    model: Some("*/gemma-*".into()),
                    ..MuteMatch::default()
                },
                MuteMatch {
                    kind: Some("custom".into()),
                    rule: Some("r1".into()),
                    profile: Some("local".into()),
                    title: Some("Disk *".into()),
                    ..MuteMatch::default()
                },
            ]
        );
        assert!(EnvMute::parse_list("  ").unwrap().is_empty());
    }

    #[test]
    fn malformed_rules_are_rejected_with_the_key_named() {
        for (raw, expected) in [
            ("colour=red", "unknown field \"colour\""),
            ("source=", "empty glob for \"source\""),
            ("source=a,,model=b", "without field=glob"),
            ("errors", "without field=glob"),
            ("source=a,source=b", "more than once"),
        ] {
            let err = EnvMute::parse_list(raw).unwrap_err();
            assert!(err.contains("LLM_HUB_NOTIFY_MUTE"), "{raw}: {err}");
            assert!(err.contains(expected), "{raw}: {err}");
        }
    }

    #[test]
    fn rule_and_glob_caps_are_enforced() {
        let at_cap = vec!["source=a"; 200].join(";");
        assert_eq!(EnvMute::parse_list(&at_cap).unwrap().len(), 200);
        let over_cap = vec!["source=a"; 201].join(";");
        let err = EnvMute::parse_list(&over_cap).unwrap_err();
        assert!(err.contains("201 rules"), "{err}");

        let longest = format!("title={}", "x".repeat(256));
        assert!(EnvMute::parse_list(&longest).is_ok());
        let too_long = format!("title={}", "x".repeat(257));
        let err = EnvMute::parse_list(&too_long).unwrap_err();
        assert!(err.contains("longer than 256"), "{err}");
    }
}
