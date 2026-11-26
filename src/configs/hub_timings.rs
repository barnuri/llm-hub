use std::collections::HashMap;
use std::time::Duration;

use crate::consts::{
    ENV_WATCH_INTERVAL, MANAGED_IDLE_CHECK_INTERVAL, MANAGED_READY_POLL_INTERVAL,
    MANAGED_STOP_GRACE,
};

/// How often the local model registries are re-read for edits.
const DEFAULT_REGISTRY_POLL_INTERVAL: Duration = Duration::from_secs(2);
/// How long one local model health probe may take.
const DEFAULT_HEALTH_PROBE_TIMEOUT: Duration = Duration::from_secs(1);

/// Background intervals and waits that tests shorten. Each has a `LLM_HUB_<NAME>_MS` variable;
/// unset, it keeps the default. They are read at startup and not reloaded.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct HubTimings {
    /// `LLM_HUB_MANAGED_IDLE_CHECK_INTERVAL_MS`: how often idle managed and local processes are stopped.
    pub idle_check_interval: Duration,
    /// `LLM_HUB_ENV_WATCH_INTERVAL_MS`: how often `.env` and the rules file are checked for edits.
    pub env_watch_interval: Duration,
    /// `LLM_HUB_REGISTRY_POLL_INTERVAL_MS`: how often the local model registries are re-read.
    pub registry_poll_interval: Duration,
    /// `LLM_HUB_HEALTH_PROBE_TIMEOUT_MS`: the time limit of one local model health probe.
    pub health_probe_timeout: Duration,
    /// `LLM_HUB_MANAGED_STOP_GRACE_MS`: how long a process gets to exit after SIGTERM before it is killed.
    pub managed_stop_grace: Duration,
    /// `LLM_HUB_READY_POLL_INTERVAL_MS`: how often a starting process is probed until it answers.
    pub ready_poll_interval: Duration,
}

impl Default for HubTimings {
    fn default() -> Self {
        HubTimings {
            idle_check_interval: MANAGED_IDLE_CHECK_INTERVAL,
            env_watch_interval: ENV_WATCH_INTERVAL,
            registry_poll_interval: DEFAULT_REGISTRY_POLL_INTERVAL,
            health_probe_timeout: DEFAULT_HEALTH_PROBE_TIMEOUT,
            managed_stop_grace: MANAGED_STOP_GRACE,
            ready_poll_interval: MANAGED_READY_POLL_INTERVAL,
        }
    }
}

impl HubTimings {
    /// Reads the `_MS` variables from `vars` (keys with the `LLM_HUB_` prefix).
    ///
    /// # Errors
    /// A message naming the variable when a value is not a number of milliseconds, or is `0`.
    pub fn from_vars(vars: &HashMap<String, String>) -> Result<HubTimings, String> {
        let defaults = HubTimings::default();
        let read = |name: &str, default: Duration| -> Result<Duration, String> {
            let key = format!("LLM_HUB_{name}_MS");
            match vars.get(&key).map(|v| v.trim()).filter(|v| !v.is_empty()) {
                None => Ok(default),
                Some(raw) => match raw.parse::<u64>() {
                    Ok(ms) if ms > 0 => Ok(Duration::from_millis(ms)),
                    _ => Err(format!(
                        "{key} is not a positive number of milliseconds: {raw}"
                    )),
                },
            }
        };
        Ok(HubTimings {
            idle_check_interval: read("MANAGED_IDLE_CHECK_INTERVAL", defaults.idle_check_interval)?,
            env_watch_interval: read("ENV_WATCH_INTERVAL", defaults.env_watch_interval)?,
            registry_poll_interval: read(
                "REGISTRY_POLL_INTERVAL",
                defaults.registry_poll_interval,
            )?,
            health_probe_timeout: read("HEALTH_PROBE_TIMEOUT", defaults.health_probe_timeout)?,
            managed_stop_grace: read("MANAGED_STOP_GRACE", defaults.managed_stop_grace)?,
            ready_poll_interval: read("READY_POLL_INTERVAL", defaults.ready_poll_interval)?,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn vars(pairs: &[(&str, &str)]) -> HashMap<String, String> {
        pairs
            .iter()
            .map(|(k, v)| ((*k).to_string(), (*v).to_string()))
            .collect()
    }

    #[test]
    fn unset_variables_keep_the_defaults() {
        let timings = HubTimings::from_vars(&HashMap::new()).unwrap();
        assert_eq!(timings, HubTimings::default());
        assert_eq!(timings.registry_poll_interval, Duration::from_secs(2));
        assert_eq!(timings.health_probe_timeout, Duration::from_secs(1));
        assert_eq!(timings.idle_check_interval, Duration::from_secs(1));
        assert_eq!(timings.env_watch_interval, Duration::from_secs(1));
    }

    #[test]
    fn each_variable_sets_its_own_value_in_milliseconds() {
        let timings = HubTimings::from_vars(&vars(&[
            ("LLM_HUB_MANAGED_IDLE_CHECK_INTERVAL_MS", "20"),
            ("LLM_HUB_ENV_WATCH_INTERVAL_MS", "30"),
            ("LLM_HUB_REGISTRY_POLL_INTERVAL_MS", "40"),
            ("LLM_HUB_HEALTH_PROBE_TIMEOUT_MS", "50"),
            ("LLM_HUB_MANAGED_STOP_GRACE_MS", " 60 "),
            ("LLM_HUB_READY_POLL_INTERVAL_MS", "70"),
        ]))
        .unwrap();
        assert_eq!(timings.idle_check_interval, Duration::from_millis(20));
        assert_eq!(timings.env_watch_interval, Duration::from_millis(30));
        assert_eq!(timings.registry_poll_interval, Duration::from_millis(40));
        assert_eq!(timings.health_probe_timeout, Duration::from_millis(50));
        assert_eq!(timings.managed_stop_grace, Duration::from_millis(60));
        assert_eq!(timings.ready_poll_interval, Duration::from_millis(70));
    }

    #[test]
    fn an_empty_value_keeps_the_default() {
        let timings = HubTimings::from_vars(&vars(&[("LLM_HUB_ENV_WATCH_INTERVAL_MS", "")]));
        assert_eq!(timings.unwrap(), HubTimings::default());
    }

    #[test]
    fn a_bad_or_zero_value_names_the_variable() {
        for bad in ["soon", "0", "-5", "1.5"] {
            let err = HubTimings::from_vars(&vars(&[("LLM_HUB_HEALTH_PROBE_TIMEOUT_MS", bad)]))
                .unwrap_err();
            assert!(err.contains("LLM_HUB_HEALTH_PROBE_TIMEOUT_MS"), "{err}");
        }
    }
}
