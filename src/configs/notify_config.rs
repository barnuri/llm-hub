use std::time::Duration;

use super::env_mute::EnvMute;
use super::managed_command::ManagedCommand;
use crate::schemas::notification::{MuteMatch, NotificationSeverity};

/// Notification settings from the `LLM_HUB_NOTIFICATIONS` and `LLM_HUB_NOTIFY_*` keys.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NotifyConfig {
    /// `LLM_HUB_NOTIFICATIONS`, on by default. `false` stops inbox writes and every sink.
    pub enabled: bool,
    /// `LLM_HUB_NOTIFY_DESKTOP`, on by default: show a desktop popup.
    pub desktop: bool,
    /// `LLM_HUB_NOTIFY_WEBHOOK_URL`: an http or https URL that receives each notification as JSON.
    pub webhook_url: Option<String>,
    /// `LLM_HUB_NOTIFY_COMMAND`: the argv that receives each notification as JSON on stdin.
    pub command: Option<Vec<String>>,
    /// `LLM_HUB_NOTIFY_MIN_SEVERITY`: `info`, `warning` or `critical`. Lower severities skip the sinks.
    pub min_severity: NotificationSeverity,
    /// `LLM_HUB_NOTIFY_TIMEOUT_MS`: the longest wait for one webhook call or command run.
    pub timeout: Duration,
    /// `LLM_HUB_NOTIFY_MUTE`: mute rules from the operator. The UI shows them read-only.
    pub mutes: Vec<MuteMatch>,
    /// `LLM_HUB_NOTIFY_BUDGET_STEPS`: the budget percents that raise a notification, sorted and unique.
    pub budget_steps: Vec<u32>,
}

impl Default for NotifyConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            desktop: true,
            webhook_url: None,
            command: None,
            min_severity: NotificationSeverity::Warning,
            timeout: Self::DEFAULT_TIMEOUT,
            mutes: Vec::new(),
            budget_steps: Self::DEFAULT_BUDGET_STEPS.to_vec(),
        }
    }
}

impl NotifyConfig {
    const DEFAULT_TIMEOUT: Duration = Duration::from_secs(10);
    const DEFAULT_BUDGET_STEPS: [u32; 7] = [25, 50, 75, 80, 85, 95, 100];
    const MAX_BUDGET_STEP: u32 = 1000;
    const MAX_BUDGET_STEPS: usize = 20;

    /// Reads the settings through `get`, which takes a key without the `LLM_HUB_` prefix.
    /// `is_falsy` decides when an on-by-default switch is off.
    ///
    /// # Errors
    /// A message naming the key when the webhook URL is not http or https, the command cannot be
    /// split, the severity is unknown, the timeout is not a number above 0, or a mute rule is bad.
    pub fn from_get(
        get: &impl Fn(&str) -> Option<String>,
        home: Option<&str>,
        is_falsy: impl Fn(&str) -> bool,
    ) -> Result<Self, String> {
        let present = |key: &str| get(key).filter(|v| !v.is_empty());
        let command = present("NOTIFY_COMMAND")
            .map(|raw| {
                ManagedCommand::parse_argv(&raw, home)
                    .map_err(|e| format!("LLM_HUB_NOTIFY_COMMAND {e}"))
            })
            .transpose()?;
        Ok(Self {
            enabled: get("NOTIFICATIONS").is_none_or(|v| !is_falsy(&v)),
            desktop: get("NOTIFY_DESKTOP").is_none_or(|v| !is_falsy(&v)),
            webhook_url: present("NOTIFY_WEBHOOK_URL")
                .map(Self::check_webhook_url)
                .transpose()?,
            command,
            min_severity: present("NOTIFY_MIN_SEVERITY")
                .map_or(Ok(NotificationSeverity::Warning), |raw| {
                    Self::parse_severity(&raw)
                })?,
            timeout: present("NOTIFY_TIMEOUT_MS")
                .map_or(Ok(Self::DEFAULT_TIMEOUT), |raw| Self::parse_timeout(&raw))?,
            mutes: present("NOTIFY_MUTE")
                .map_or_else(|| Ok(Vec::new()), |raw| EnvMute::parse_list(&raw))?,
            budget_steps: present("NOTIFY_BUDGET_STEPS").map_or_else(
                || Ok(Self::DEFAULT_BUDGET_STEPS.to_vec()),
                |raw| Self::parse_budget_steps(&raw),
            )?,
        })
    }

    /// A comma list of whole percents from 1 to 1000, at most 20 of them, sorted and deduplicated.
    fn parse_budget_steps(raw: &str) -> Result<Vec<u32>, String> {
        let mut steps = Vec::new();
        for item in raw
            .split(',')
            .map(str::trim)
            .filter(|item| !item.is_empty())
        {
            match item.parse::<u32>() {
                Ok(step) if (1..=Self::MAX_BUDGET_STEP).contains(&step) => steps.push(step),
                _ => {
                    return Err(format!(
                        "LLM_HUB_NOTIFY_BUDGET_STEPS takes whole percents from 1 to {}: {item}",
                        Self::MAX_BUDGET_STEP
                    ));
                }
            }
        }
        steps.sort_unstable();
        steps.dedup();
        if steps.is_empty() {
            return Err(format!(
                "LLM_HUB_NOTIFY_BUDGET_STEPS needs at least one percent: {raw}"
            ));
        }
        if steps.len() > Self::MAX_BUDGET_STEPS {
            return Err(format!(
                "LLM_HUB_NOTIFY_BUDGET_STEPS takes at most {} steps, got {}",
                Self::MAX_BUDGET_STEPS,
                steps.len()
            ));
        }
        Ok(steps)
    }

    fn check_webhook_url(raw: String) -> Result<String, String> {
        match reqwest::Url::parse(&raw) {
            Ok(url) if matches!(url.scheme(), "http" | "https") => Ok(raw),
            _ => Err(format!(
                "LLM_HUB_NOTIFY_WEBHOOK_URL must be an http:// or https:// URL: {raw}"
            )),
        }
    }

    fn parse_severity(raw: &str) -> Result<NotificationSeverity, String> {
        NotificationSeverity::parse(&raw.to_ascii_lowercase()).ok_or_else(|| {
            format!("LLM_HUB_NOTIFY_MIN_SEVERITY must be info, warning or critical: {raw}")
        })
    }

    fn parse_timeout(raw: &str) -> Result<Duration, String> {
        match raw.parse::<u64>() {
            Ok(ms) if ms > 0 => Ok(Duration::from_millis(ms)),
            _ => Err(format!(
                "LLM_HUB_NOTIFY_TIMEOUT_MS must be a number of ms above 0: {raw}"
            )),
        }
    }
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;

    use super::*;

    fn parse(pairs: &[(&str, &str)]) -> Result<NotifyConfig, String> {
        let vars: HashMap<String, String> = pairs
            .iter()
            .map(|(k, v)| ((*k).to_owned(), (*v).to_owned()))
            .collect();
        NotifyConfig::from_get(&|key: &str| vars.get(key).cloned(), Some("/home/op"), |v| {
            v == "false"
        })
    }

    #[test]
    fn defaults_turn_the_inbox_and_desktop_on_with_warning_and_ten_seconds() {
        let config = parse(&[]).unwrap();
        assert_eq!(config, NotifyConfig::default());
        assert!(config.enabled);
        assert!(config.desktop);
        assert_eq!(config.webhook_url, None);
        assert_eq!(config.command, None);
        assert_eq!(config.min_severity, NotificationSeverity::Warning);
        assert_eq!(config.timeout, Duration::from_secs(10));
        assert!(config.mutes.is_empty());
        assert_eq!(config.budget_steps, vec![25, 50, 75, 80, 85, 95, 100]);
    }

    #[test]
    fn empty_values_keep_the_defaults() {
        let keys = [
            "NOTIFICATIONS",
            "NOTIFY_DESKTOP",
            "NOTIFY_WEBHOOK_URL",
            "NOTIFY_COMMAND",
            "NOTIFY_MIN_SEVERITY",
            "NOTIFY_TIMEOUT_MS",
            "NOTIFY_MUTE",
            "NOTIFY_BUDGET_STEPS",
        ];
        let pairs: Vec<(&str, &str)> = keys.iter().map(|key| (*key, "")).collect();
        assert_eq!(parse(&pairs).unwrap(), NotifyConfig::default());
    }

    #[test]
    fn every_key_overrides_its_default() {
        let config = parse(&[
            ("NOTIFICATIONS", "false"),
            ("NOTIFY_DESKTOP", "false"),
            ("NOTIFY_WEBHOOK_URL", "https://hooks.example.com/x"),
            ("NOTIFY_COMMAND", "~/bin/notify --json"),
            ("NOTIFY_MIN_SEVERITY", "Critical"),
            ("NOTIFY_TIMEOUT_MS", "2500"),
            ("NOTIFY_MUTE", "source=errors;model=*/gemma-*"),
            ("NOTIFY_BUDGET_STEPS", "90, 50,50 ,1000,1"),
        ])
        .unwrap();
        assert!(!config.enabled);
        assert!(!config.desktop);
        assert_eq!(
            config.webhook_url.as_deref(),
            Some("https://hooks.example.com/x")
        );
        assert_eq!(
            config.command,
            Some(vec!["/home/op/bin/notify".to_owned(), "--json".to_owned()])
        );
        assert_eq!(config.min_severity, NotificationSeverity::Critical);
        assert_eq!(config.timeout, Duration::from_millis(2500));
        assert_eq!(config.mutes.len(), 2);
        assert_eq!(config.mutes[1].model.as_deref(), Some("*/gemma-*"));
        assert_eq!(config.budget_steps, vec![1, 50, 90, 1000]);
    }

    #[test]
    fn budget_steps_allow_twenty_steps_and_skip_empty_items() {
        let twenty: Vec<String> = (1..=20).map(|step| (step * 10).to_string()).collect();
        let config = parse(&[("NOTIFY_BUDGET_STEPS", &twenty.join(","))]).unwrap();
        assert_eq!(config.budget_steps.len(), 20);
        let config = parse(&[("NOTIFY_BUDGET_STEPS", "50,,100,")]).unwrap();
        assert_eq!(config.budget_steps, vec![50, 100]);
    }

    #[test]
    fn budget_steps_reject_more_than_twenty_after_dedup() {
        let many: Vec<String> = (1..=21).map(|step| step.to_string()).collect();
        let err = parse(&[("NOTIFY_BUDGET_STEPS", &many.join(","))]).unwrap_err();
        assert!(err.contains("at most 20 steps, got 21"), "{err}");
        let dupes = format!("{},20", many[..20].join(","));
        assert_eq!(
            parse(&[("NOTIFY_BUDGET_STEPS", &dupes)])
                .unwrap()
                .budget_steps
                .len(),
            20
        );
    }

    #[test]
    fn info_severity_and_an_http_webhook_are_accepted() {
        let config = parse(&[
            ("NOTIFY_MIN_SEVERITY", "info"),
            ("NOTIFY_WEBHOOK_URL", "http://127.0.0.1:9000/hook"),
        ])
        .unwrap();
        assert_eq!(config.min_severity, NotificationSeverity::Info);
        assert!(config.webhook_url.is_some());
    }

    #[test]
    fn bad_values_are_rejected_with_the_key_named() {
        for (key, value) in [
            ("NOTIFY_WEBHOOK_URL", "ftp://example.com/x"),
            ("NOTIFY_WEBHOOK_URL", "not a url"),
            ("NOTIFY_COMMAND", "notify 'open"),
            ("NOTIFY_MIN_SEVERITY", "debug"),
            ("NOTIFY_TIMEOUT_MS", "0"),
            ("NOTIFY_TIMEOUT_MS", "soon"),
            ("NOTIFY_MUTE", "colour=red"),
            ("NOTIFY_BUDGET_STEPS", "0"),
            ("NOTIFY_BUDGET_STEPS", "1001"),
            ("NOTIFY_BUDGET_STEPS", "50,eighty"),
            ("NOTIFY_BUDGET_STEPS", "-5"),
            ("NOTIFY_BUDGET_STEPS", "12.5"),
            ("NOTIFY_BUDGET_STEPS", ",,"),
        ] {
            let err = parse(&[(key, value)]).unwrap_err();
            assert!(
                err.contains(&format!("LLM_HUB_{key}")),
                "{key}={value}: {err}"
            );
        }
    }
}
