use std::path::PathBuf;
use std::time::Duration;

use super::managed_command::ManagedCommand;

/// Local model tuner settings of one profile. They come only from the operator's `.env`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LocalTunerConfig {
    /// `LLM_HUB_<P>_PREFILL_GUARD`, resolved to a budget in ms. `None` turns the guard off.
    pub prefill_guard: Option<u64>,
    /// `LLM_HUB_<P>_TUNER_REGISTRY`: the registry file that Approve rewrites, with `~/` expanded.
    pub registry: Option<PathBuf>,
    /// `LLM_HUB_<P>_TUNER_APPLY_COMMAND`: the argv that runs after the registry write.
    pub apply_command: Option<Vec<String>>,
    /// `LLM_HUB_<P>_TUNER_APPLY_TIMEOUT_MS`: the hub kills the apply command after this time.
    pub apply_timeout: Duration,
}

impl Default for LocalTunerConfig {
    fn default() -> Self {
        Self {
            prefill_guard: None,
            registry: None,
            apply_command: None,
            apply_timeout: Self::DEFAULT_APPLY_TIMEOUT,
        }
    }
}

impl LocalTunerConfig {
    const DEFAULT_APPLY_TIMEOUT: Duration = Duration::from_mins(2);

    /// Reads the tuner keys of profile `upper` through `get`, which takes a key without the
    /// `LLM_HUB_<P>_` prefix. `timeout_ms` and `first_byte_timeout_ms` are the profile's limits.
    ///
    /// # Errors
    /// A message naming the key when a value does not parse, when the apply command cannot be
    /// split, or when `PREFILL_GUARD=on` finds no profile timeout to use as the budget.
    pub fn from_env(
        upper: &str,
        home: Option<&str>,
        get: &dyn Fn(&str) -> Option<String>,
        timeout_ms: Option<u64>,
        first_byte_timeout_ms: Option<u64>,
    ) -> Result<Self, String> {
        let present = |key: &str| get(key).filter(|v| !v.is_empty());
        let apply_command = present("TUNER_APPLY_COMMAND")
            .map(|raw| {
                ManagedCommand::parse_argv(&raw, home)
                    .map_err(|e| format!("LLM_HUB_{upper}_TUNER_APPLY_COMMAND {e}"))
            })
            .transpose()?;
        let apply_timeout = match present("TUNER_APPLY_TIMEOUT_MS") {
            None => Self::DEFAULT_APPLY_TIMEOUT,
            Some(v) => match v.parse::<u64>() {
                Ok(ms) if ms > 0 => Duration::from_millis(ms),
                _ => {
                    return Err(format!(
                        "LLM_HUB_{upper}_TUNER_APPLY_TIMEOUT_MS must be a number of ms above 0: {v}"
                    ));
                }
            },
        };
        Ok(Self {
            prefill_guard: Self::prefill_guard(
                upper,
                present("PREFILL_GUARD"),
                timeout_ms,
                first_byte_timeout_ms,
            )?,
            registry: present("TUNER_REGISTRY")
                .map(|raw| PathBuf::from(ManagedCommand::expand_home(raw, home))),
            apply_command,
            apply_timeout,
        })
    }

    fn prefill_guard(
        upper: &str,
        raw: Option<String>,
        timeout_ms: Option<u64>,
        first_byte_timeout_ms: Option<u64>,
    ) -> Result<Option<u64>, String> {
        let Some(raw) = raw else {
            return Ok(None);
        };
        match raw.to_ascii_lowercase().as_str() {
            "off" | "false" | "no" | "0" => Ok(None),
            "on" | "true" | "yes" => first_byte_timeout_ms
                .filter(|ms| *ms > 0)
                .or(timeout_ms.filter(|ms| *ms > 0))
                .map(Some)
                .ok_or_else(|| {
                    format!(
                        "LLM_HUB_{upper}_PREFILL_GUARD=on needs LLM_HUB_{upper}_FIRST_BYTE_TIMEOUT_MS or LLM_HUB_{upper}_TIMEOUT_MS above 0. Set one of them, or give the budget in ms."
                    )
                }),
            _ => raw.parse::<u64>().map(Some).map_err(|_| {
                format!(
                    "LLM_HUB_{upper}_PREFILL_GUARD must be on, off or a budget in ms: {raw}"
                )
            }),
        }
    }
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;

    use super::*;

    fn parse(
        pairs: &[(&str, &str)],
        timeout_ms: Option<u64>,
        first_byte_timeout_ms: Option<u64>,
    ) -> Result<LocalTunerConfig, String> {
        let vars: HashMap<String, String> = pairs
            .iter()
            .map(|(k, v)| ((*k).to_owned(), (*v).to_owned()))
            .collect();
        LocalTunerConfig::from_env(
            "LOCAL",
            Some("/home/op"),
            &|key: &str| vars.get(key).cloned(),
            timeout_ms,
            first_byte_timeout_ms,
        )
    }

    #[test]
    fn defaults_turn_everything_off_with_a_two_minute_apply_timeout() {
        let config = parse(&[], Some(1000), Some(500)).unwrap();
        assert_eq!(config, LocalTunerConfig::default());
        assert_eq!(config.apply_timeout, Duration::from_mins(2));
    }

    #[test]
    fn every_key_overrides_its_default() {
        let config = parse(
            &[
                ("PREFILL_GUARD", "240000"),
                ("TUNER_REGISTRY", "~/models/registry.txt"),
                ("TUNER_APPLY_COMMAND", r#"~/bin/sync --restart "two words""#),
                ("TUNER_APPLY_TIMEOUT_MS", "5000"),
            ],
            None,
            None,
        )
        .unwrap();
        assert_eq!(config.prefill_guard, Some(240_000));
        assert_eq!(
            config.registry,
            Some(PathBuf::from("/home/op/models/registry.txt"))
        );
        assert_eq!(
            config.apply_command,
            Some(vec![
                "/home/op/bin/sync".to_owned(),
                "--restart".to_owned(),
                "two words".to_owned()
            ])
        );
        assert_eq!(config.apply_timeout, Duration::from_secs(5));
    }

    #[test]
    fn prefill_guard_off_values_disable_it() {
        for value in ["off", "FALSE", "no", "0", ""] {
            let config = parse(&[("PREFILL_GUARD", value)], Some(1000), None).unwrap();
            assert_eq!(config.prefill_guard, None, "{value:?}");
        }
    }

    #[test]
    fn prefill_guard_on_prefers_the_first_byte_timeout_then_the_total_timeout() {
        for (first_byte, total, expected) in [
            (Some(300_000), Some(600_000), 300_000),
            (Some(0), Some(600_000), 600_000),
            (None, Some(600_000), 600_000),
        ] {
            for value in ["on", "True", "yes"] {
                let config = parse(&[("PREFILL_GUARD", value)], total, first_byte).unwrap();
                assert_eq!(
                    config.prefill_guard,
                    Some(expected),
                    "{value} {first_byte:?}"
                );
            }
        }
    }

    #[test]
    fn prefill_guard_on_without_a_profile_timeout_is_rejected() {
        for (first_byte, total) in [(None, None), (Some(0), Some(0))] {
            let err = parse(&[("PREFILL_GUARD", "on")], total, first_byte).unwrap_err();
            assert!(err.contains("LLM_HUB_LOCAL_PREFILL_GUARD=on"), "{err}");
            assert!(err.contains("LLM_HUB_LOCAL_TIMEOUT_MS"), "{err}");
        }
    }

    #[test]
    fn prefill_guard_text_is_rejected() {
        let err = parse(&[("PREFILL_GUARD", "soon")], None, None).unwrap_err();
        assert!(err.contains("LLM_HUB_LOCAL_PREFILL_GUARD"), "{err}");
        assert!(err.contains("soon"), "{err}");
    }

    #[test]
    fn unbalanced_apply_command_is_rejected() {
        let err = parse(&[("TUNER_APPLY_COMMAND", r#"sync "open"#)], None, None).unwrap_err();
        assert!(err.contains("LLM_HUB_LOCAL_TUNER_APPLY_COMMAND"), "{err}");
    }

    #[test]
    fn apply_timeout_zero_or_text_is_rejected() {
        for value in ["0", "soon"] {
            let err = parse(&[("TUNER_APPLY_TIMEOUT_MS", value)], None, None).unwrap_err();
            assert!(
                err.contains("LLM_HUB_LOCAL_TUNER_APPLY_TIMEOUT_MS"),
                "{err}"
            );
        }
    }
}
