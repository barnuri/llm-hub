use std::time::Duration;

use crate::consts::{
    DEFAULT_KEEP_WARM_ENABLED, DEFAULT_KEEP_WARM_IDLE_CAP_MINUTES, KEEP_WARM_MAX_IDLE_CAP_MINUTES,
    KEEP_WARM_MIN_IDLE_CAP_MINUTES,
};

/// Cache keep-warm settings from `LLM_HUB_KEEP_WARM` and `LLM_HUB_KEEP_WARM_IDLE_CAP_MINUTES`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct KeepWarmConfig {
    pub enabled: bool,
    /// Time after the last user request at which a stream stops being pinged.
    pub idle_cap: Duration,
}

impl KeepWarmConfig {
    /// Returns `minutes` when it lies within the allowed idle-cap range.
    ///
    /// # Errors
    /// Returns a message naming the allowed range when `minutes` is outside it.
    pub fn check_idle_cap_minutes(minutes: u64) -> Result<u64, String> {
        if (KEEP_WARM_MIN_IDLE_CAP_MINUTES..=KEEP_WARM_MAX_IDLE_CAP_MINUTES).contains(&minutes) {
            Ok(minutes)
        } else {
            Err(format!(
                "idle cap must be between {KEEP_WARM_MIN_IDLE_CAP_MINUTES} and {KEEP_WARM_MAX_IDLE_CAP_MINUTES} minutes"
            ))
        }
    }
}

impl Default for KeepWarmConfig {
    fn default() -> Self {
        Self {
            enabled: DEFAULT_KEEP_WARM_ENABLED,
            idle_cap: Duration::from_mins(DEFAULT_KEEP_WARM_IDLE_CAP_MINUTES),
        }
    }
}
