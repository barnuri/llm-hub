use std::collections::{BTreeMap, HashMap};
use std::time::Duration;

use super::copilot_auth::CopilotAuth;
use super::keep_warm_config::KeepWarmConfig;
use super::local_tuner_config::LocalTunerConfig;
use super::managed_command::ManagedCommand;
use super::media_fallback_config::MediaFallbackConfig;
use super::profile_config::ProfileConfig;
use super::usage_source::{BIFROST_KEY_PREFIX, UsageSource};
use crate::configs::NotifyConfig;
use crate::consts::{
    DEFAULT_BIND, DEFAULT_COPILOT_PRICES_REFRESH_SECS, DEFAULT_FIRST_BYTE_TIMEOUT_MS,
    DEFAULT_KEEP_WARM_ENABLED, DEFAULT_KEEP_WARM_IDLE_CAP_MINUTES, DEFAULT_MAX_REPLAY_BYTES,
    DEFAULT_MAX_REQUEST_BODY_BYTES, DEFAULT_PORT, DEFAULT_QUOTA_COOLDOWN_SECS,
    DEFAULT_SSE_KEEPALIVE_MS,
};
use crate::schemas::hub_selector::HubSelector;
use crate::schemas::smart_auto_policy::SmartAutoPolicy;

use crate::services::transforms::{
    CopilotModels, DEFAULT_COPILOT_GUARD_MODELS, DEFAULT_COPILOT_STRIP_SAMPLING_MODELS,
};

const PREFIX: &str = "LLM_HUB_";

#[allow(
    clippy::struct_excessive_bools,
    reason = "Each flag is an independent LLM_HUB_* switch; enums would only add an indirection"
)]
#[derive(Debug, Clone)]
pub struct HubConfig {
    pub profiles: Vec<ProfileConfig>,
    /// Quota source per non-Copilot profile name, from `LLM_HUB_<NAME>_USAGE` or the key prefix.
    #[allow(
        clippy::zero_sized_map_values,
        reason = "UsageSource is an enum meant to gain variants"
    )]
    pub usage_sources: HashMap<String, UsageSource>,
    /// Operator Smart Auto presets from `LLM_HUB_SMART_AUTO_PRESETS`, served as `hub/<name>`.
    pub smart_auto_presets: BTreeMap<String, SmartAutoPolicy>,
    /// Smart Auto include and exclude rules from `LLM_HUB_SMART_AUTO_RULES`; set, they replace the saved file.
    pub smart_auto_rules_env: Option<String>,
    /// Extension names and globs, and `rules:<glob>` for rules, from `LLM_HUB_EXTENSIONS_DISABLED`.
    pub extensions_disabled: Vec<String>,
    pub master_key: Option<String>,
    pub default_fallbacks: Vec<String>,
    pub max_replay_bytes: usize,
    /// Largest proxy request body, by `Content-Length`; over it the proxy answers 413.
    pub max_request_body_bytes: usize,
    pub config_readonly: bool,
    pub bind: String,
    pub port: u16,
    pub persistent: bool,
    pub store_kind: String,
    pub store_path: Option<String>,
    pub auto_update: bool,
    /// Inject `role:"assistant"` into the first streamed delta when the
    /// upstream omits it. Opt-out (`LLM_HUB_STREAM_ROLE=false`): the clients
    /// this repairs cannot set a request header, so it defaults on.
    pub stream_role_inject: bool,
    /// Models (`<model>`, `<profile>/<model>` or `*`) whose Copilot CLI
    /// requests get the tool-loop guard. `LLM_HUB_COPILOT_GUARD_MODELS`,
    /// comma-separated; empty disables it; unset uses
    /// [`DEFAULT_COPILOT_GUARD_MODELS`].
    pub copilot_guard_models: Vec<String>,
    /// Models whose Copilot CLI requests lose Copilot's hard-coded sampling
    /// (temperature 0, `top_p`, penalties) so the serving row's sampling applies.
    /// `LLM_HUB_COPILOT_STRIP_SAMPLING_MODELS`, comma-separated; empty disables
    /// it; unset uses [`DEFAULT_COPILOT_STRIP_SAMPLING_MODELS`].
    pub copilot_strip_sampling_models: Vec<String>,
    /// Idle gap after which an SSE response gets a keepalive comment frame.
    /// `None` when `LLM_HUB_SSE_KEEPALIVE_MS=0` — the knob's off switch.
    pub sse_keepalive: Option<Duration>,
    /// Longest wait for the first byte of a streamed Smart Auto attempt on a remote profile.
    /// `None` when `LLM_HUB_FIRST_BYTE_TIMEOUT_MS=0`. A model that misses it is cooled.
    pub first_byte_timeout: Option<Duration>,
    /// How long the hub skips a profile that is out of credits. `None` when
    /// `LLM_HUB_QUOTA_COOLDOWN_SECS=0`: the hub still tries the next fallback, but cools nothing.
    pub quota_cooldown: Option<Duration>,
    /// The default time limit of a whole fallback chain, `LLM_HUB_DEADLINE_MS`. `None`, the
    /// default, sets none. The `x-llm-hub-deadline-ms` header wins.
    pub deadline: Option<Duration>,
    /// How often the Copilot price table is fetched. `None` when
    /// `LLM_HUB_COPILOT_PRICES_REFRESH_SECS=0`: the hub never fetches it.
    pub copilot_prices_refresh: Option<Duration>,
    /// Cache keep-warm for idle Copilot Claude streams.
    pub keep_warm: KeepWarmConfig,
    /// Media parts converted to text for text-only models. Off by default.
    pub media_fallback: MediaFallbackConfig,
    /// Global USD/1M token defaults when a profile/model has no rates.
    pub pricing: crate::configs::TokenRates,
    /// `LLM_HUB_LOCAL_TUNER`, off by default: run the local model probe and its proposals.
    pub local_tuner_enabled: bool,
    /// The notification inbox, its sinks and the operator's mute rules.
    pub notify: NotifyConfig,
    /// `LLM_HUB_BUDGET_MONTHLY_USD`: the hub-wide monthly spend budget in USD. `None` sets none.
    pub budget_monthly_usd: Option<f64>,
}

impl HubConfig {
    /// The Copilot CLI model lists, as the request transforms take them.
    #[must_use]
    pub fn copilot_models(&self) -> CopilotModels<'_> {
        CopilotModels {
            guard: &self.copilot_guard_models,
            strip_sampling: &self.copilot_strip_sampling_models,
        }
    }

    pub fn profile(&self, name: &str) -> Option<&ProfileConfig> {
        self.profiles.iter().find(|p| p.name == name)
    }

    pub fn usage_source(&self, profile: &str) -> Option<UsageSource> {
        self.usage_sources.get(profile).copied()
    }

    #[allow(
        clippy::too_many_lines,
        reason = "flat list of env keys; splitting adds no clarity"
    )]
    pub fn from_map(vars: &HashMap<String, String>) -> Result<HubConfig, String> {
        let get = |key: &str| {
            vars.get(&format!("{PREFIX}{key}"))
                .map(|v| v.trim().to_string())
        };

        let profile_names = match get("PROFILES") {
            Some(list) if !list.is_empty() => list
                .split(',')
                .map(|s| s.trim().to_string())
                .filter(|s| !s.is_empty())
                .collect(),
            // No index var: discover profiles from LLM_HUB_<NAME>_BASE_URL keys.
            _ => discover_profile_names(vars),
        };

        let mut profiles = Vec::with_capacity(profile_names.len());
        for name in &profile_names {
            profiles.push(Self::parse_profile(name, vars)?);
        }

        let usage_sources = usage_sources_from_map(&profiles, vars)?;

        let master_key = get("MASTER_KEY").filter(|k| !k.is_empty());
        let default_fallbacks: Vec<String> = get("DEFAULT_FALLBACKS")
            .unwrap_or_default()
            .split(',')
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty())
            .collect();
        let max_replay_bytes = match get("MAX_REPLAY_BYTES") {
            Some(v) => v
                .parse()
                .map_err(|_| format!("LLM_HUB_MAX_REPLAY_BYTES is not a number: {v}"))?,
            None => DEFAULT_MAX_REPLAY_BYTES,
        };
        let max_request_body_bytes = match get("MAX_REQUEST_BODY_BYTES") {
            Some(v) => v
                .parse()
                .map_err(|_| format!("LLM_HUB_MAX_REQUEST_BODY_BYTES is not a number: {v}"))?,
            None => DEFAULT_MAX_REQUEST_BODY_BYTES,
        };
        let port = match get("PORT") {
            Some(v) => v
                .parse()
                .map_err(|_| format!("LLM_HUB_PORT is not a valid port: {v}"))?,
            None => DEFAULT_PORT,
        };

        let sse_keepalive_ms = match get("SSE_KEEPALIVE_MS") {
            Some(v) if !v.is_empty() => v
                .parse()
                .map_err(|_| format!("LLM_HUB_SSE_KEEPALIVE_MS is not a number: {v}"))?,
            _ => DEFAULT_SSE_KEEPALIVE_MS,
        };

        let first_byte_timeout_ms: u64 = match get("FIRST_BYTE_TIMEOUT_MS") {
            Some(v) if !v.is_empty() => v
                .parse()
                .map_err(|_| format!("LLM_HUB_FIRST_BYTE_TIMEOUT_MS is not a number: {v}"))?,
            _ => DEFAULT_FIRST_BYTE_TIMEOUT_MS,
        };

        let quota_cooldown_secs: u64 = match get("QUOTA_COOLDOWN_SECS") {
            Some(v) if !v.is_empty() => v
                .parse()
                .map_err(|_| format!("LLM_HUB_QUOTA_COOLDOWN_SECS is not a number: {v}"))?,
            _ => DEFAULT_QUOTA_COOLDOWN_SECS,
        };
        let deadline_ms: u64 = match get("DEADLINE_MS") {
            Some(v) if !v.is_empty() => v
                .parse()
                .map_err(|_| format!("LLM_HUB_DEADLINE_MS is not a number: {v}"))?,
            _ => 0,
        };

        let copilot_prices_refresh_secs: u64 = match get("COPILOT_PRICES_REFRESH_SECS") {
            Some(v) if !v.is_empty() => v
                .parse()
                .map_err(|_| format!("LLM_HUB_COPILOT_PRICES_REFRESH_SECS is not a number: {v}"))?,
            _ => DEFAULT_COPILOT_PRICES_REFRESH_SECS,
        };

        let keep_warm = keep_warm_from_map(&get)?;
        let media_fallback = MediaFallbackConfig::from_get(&get, is_truthy)?;
        let notify = NotifyConfig::from_get(&get, home_dir(vars), is_falsy)?;

        let pricing = crate::configs::TokenRates {
            input_per_1m: crate::configs::token_rates::parse_optional_f64(
                get("INPUT_USD_PER_1M"),
                "LLM_HUB_INPUT_USD_PER_1M",
            )?,
            output_per_1m: crate::configs::token_rates::parse_optional_f64(
                get("OUTPUT_USD_PER_1M"),
                "LLM_HUB_OUTPUT_USD_PER_1M",
            )?,
            cache_read_per_1m: crate::configs::token_rates::parse_optional_f64(
                get("CACHE_READ_USD_PER_1M"),
                "LLM_HUB_CACHE_READ_USD_PER_1M",
            )?,
            cache_write_per_1m: crate::configs::token_rates::parse_optional_f64(
                get("CACHE_WRITE_USD_PER_1M"),
                "LLM_HUB_CACHE_WRITE_USD_PER_1M",
            )?,
        };

        Ok(HubConfig {
            smart_auto_presets: smart_auto_presets_from_map(vars)?,
            smart_auto_rules_env: get("SMART_AUTO_RULES").filter(|v| !v.trim().is_empty()),
            extensions_disabled: get("EXTENSIONS_DISABLED")
                .map(|list| split_list(&list))
                .unwrap_or_default(),
            profiles,
            usage_sources,
            master_key,
            default_fallbacks,
            max_replay_bytes,
            max_request_body_bytes,
            config_readonly: get("CONFIG_READONLY").is_some_and(|v| is_truthy(&v)),
            bind: get("BIND").unwrap_or_else(|| DEFAULT_BIND.to_string()),
            port,
            persistent: get("PERSISTENT").is_some_and(|v| is_truthy(&v)),
            store_kind: get("STORE").unwrap_or_else(|| "sqlite".to_string()),
            store_path: get("STORE_PATH").filter(|v| !v.is_empty()),
            auto_update: get("AUTO_UPDATE").is_none_or(|v| !is_falsy(&v)),
            stream_role_inject: get("STREAM_ROLE").is_none_or(|v| !is_falsy(&v)),
            copilot_guard_models: get("COPILOT_GUARD_MODELS").map_or_else(
                || DEFAULT_COPILOT_GUARD_MODELS.map(String::from).to_vec(),
                |list| split_list(&list),
            ),
            copilot_strip_sampling_models: get("COPILOT_STRIP_SAMPLING_MODELS").map_or_else(
                || {
                    DEFAULT_COPILOT_STRIP_SAMPLING_MODELS
                        .map(String::from)
                        .to_vec()
                },
                |list| split_list(&list),
            ),
            sse_keepalive: (sse_keepalive_ms > 0).then(|| Duration::from_millis(sse_keepalive_ms)),
            first_byte_timeout: (first_byte_timeout_ms > 0)
                .then(|| Duration::from_millis(first_byte_timeout_ms)),
            quota_cooldown: (quota_cooldown_secs > 0)
                .then(|| Duration::from_secs(quota_cooldown_secs)),
            deadline: (deadline_ms > 0).then(|| Duration::from_millis(deadline_ms)),
            copilot_prices_refresh: (copilot_prices_refresh_secs > 0)
                .then(|| Duration::from_secs(copilot_prices_refresh_secs)),
            keep_warm,
            media_fallback,
            pricing,
            local_tuner_enabled: get("LOCAL_TUNER").is_some_and(|v| is_truthy(&v)),
            notify,
            budget_monthly_usd: parse_budget_usd(get("BUDGET_MONTHLY_USD"), "BUDGET_MONTHLY_USD")?,
        })
    }

    fn parse_profile(name: &str, vars: &HashMap<String, String>) -> Result<ProfileConfig, String> {
        if name.contains('/') {
            return Err(format!("profile name may not contain '/': {name}"));
        }
        if name.eq_ignore_ascii_case(HubSelector::NAMESPACE) {
            return Err(format!(
                "profile name {name} is reserved: the hub serves its own virtual models under {}/",
                HubSelector::NAMESPACE
            ));
        }
        let upper = env_name(name);
        let get = |key: &str| {
            vars.get(&format!("{PREFIX}{upper}_{key}"))
                .map(|v| v.trim().to_string())
        };

        let base_url = get("BASE_URL")
            .filter(|v| !v.is_empty())
            .ok_or_else(|| format!("missing LLM_HUB_{upper}_BASE_URL for profile {name}"))?;
        if !base_url.starts_with("http://") && !base_url.starts_with("https://") {
            return Err(format!(
                "LLM_HUB_{upper}_BASE_URL must start with http:// or https://"
            ));
        }

        let extra_headers = match get("HEADERS") {
            None => Vec::new(),
            Some(raw) if raw.is_empty() => Vec::new(),
            Some(raw) => parse_headers_json(&upper, &raw)?,
        };

        let timeout_ms = match get("TIMEOUT_MS") {
            None => None,
            Some(v) => Some(
                v.parse()
                    .map_err(|_| format!("LLM_HUB_{upper}_TIMEOUT_MS is not a number: {v}"))?,
            ),
        };

        let display_name = get("DISPLAY_NAME").filter(|v| !v.is_empty());
        let home = home_dir(vars);
        let managed = ManagedCommand::from_env(&upper, &base_url, home, &get)?;
        let first_byte_timeout_ms = match get("FIRST_BYTE_TIMEOUT_MS").filter(|v| !v.is_empty()) {
            None => None,
            Some(v) => Some(v.parse().map_err(|_| {
                format!("LLM_HUB_{upper}_FIRST_BYTE_TIMEOUT_MS is not a number: {v}")
            })?),
        };
        let tuner =
            LocalTunerConfig::from_env(&upper, home, &get, timeout_ms, first_byte_timeout_ms)?;
        let (pricing, model_prices) =
            crate::configs::token_rates::profile_rates_from_env(name, &get)?;
        let api_key = get("API_KEY").unwrap_or_default();
        let copilot = CopilotAuth::from_env(&upper, &get)?;
        if copilot.is_some() && !api_key.is_empty() {
            return Err(format!(
                "LLM_HUB_{upper}_AUTH and LLM_HUB_{upper}_API_KEY cannot both be set: AUTH supplies the upstream credential"
            ));
        }

        Ok(ProfileConfig {
            name: name.to_string(),
            display_name,
            base_url: base_url.trim_end_matches('/').to_string(),
            api_key,
            extra_headers,
            timeout_ms,
            enabled: get("ENABLED").is_none_or(|v| is_truthy(&v)),
            static_models: get("MODELS")
                .unwrap_or_default()
                .split(',')
                .map(|s| s.trim().to_string())
                .filter(|s| !s.is_empty())
                .collect(),
            pricing,
            model_prices,
            managed,
            copilot,
            keep_warm: get("KEEP_WARM").is_none_or(|v| !is_falsy(&v)),
            first_byte_timeout_ms,
            tuner,
            budget_monthly_usd: parse_budget_usd(
                get("BUDGET_MONTHLY_USD"),
                &format!("{upper}_BUDGET_MONTHLY_USD"),
            )?,
        })
    }
}

/// A monthly budget in USD from `LLM_HUB_<key>`: a finite number above 0. Unset or empty sets none.
fn parse_budget_usd(raw: Option<String>, key: &str) -> Result<Option<f64>, String> {
    let Some(raw) = raw.filter(|v| !v.is_empty()) else {
        return Ok(None);
    };
    match raw.parse::<f64>() {
        Ok(usd) if usd.is_finite() && usd > 0.0 => Ok(Some(usd)),
        _ => Err(format!(
            "{PREFIX}{key} must be a number of USD above 0: {raw}"
        )),
    }
}

/// Quota source per profile: `LLM_HUB_<NAME>_USAGE` (`bifrost` or `none`) wins, and an unset var
/// falls back to the key prefix. Copilot profiles are skipped, their auth is the source.
#[allow(
    clippy::zero_sized_map_values,
    reason = "UsageSource is an enum meant to gain variants"
)]
fn usage_sources_from_map(
    profiles: &[ProfileConfig],
    vars: &HashMap<String, String>,
) -> Result<HashMap<String, UsageSource>, String> {
    let mut sources = HashMap::new();
    for profile in profiles.iter().filter(|p| p.copilot.is_none()) {
        let upper = env_name(&profile.name);
        let explicit = vars
            .get(&format!("{PREFIX}{upper}_USAGE"))
            .map(|v| v.trim().to_ascii_lowercase())
            .filter(|v| !v.is_empty());
        let source = match explicit.as_deref() {
            Some("bifrost") => Some(UsageSource::Bifrost),
            Some("none") => None,
            Some(other) => {
                return Err(format!(
                    "LLM_HUB_{upper}_USAGE must be bifrost or none, not {other:?}"
                ));
            }
            None => profile
                .api_key
                .starts_with(BIFROST_KEY_PREFIX)
                .then_some(UsageSource::Bifrost),
        };
        if let Some(source) = source {
            sources.insert(profile.name.clone(), source);
        }
    }
    Ok(sources)
}

/// Operator Smart Auto presets from `LLM_HUB_SMART_AUTO_PRESETS`, a JSON object of name to policy.
/// An unset or empty var yields no presets. A preset only adds constraints to a request.
pub fn smart_auto_presets_from_map(
    vars: &HashMap<String, String>,
) -> Result<BTreeMap<String, SmartAutoPolicy>, String> {
    let key = format!("{PREFIX}SMART_AUTO_PRESETS");
    let Some(raw) = vars.get(&key).map(|value| value.trim()) else {
        return Ok(BTreeMap::new());
    };
    if raw.is_empty() {
        return Ok(BTreeMap::new());
    }
    let presets: BTreeMap<String, SmartAutoPolicy> =
        serde_json::from_str(raw).map_err(|e| format!("{key} is not a valid preset map: {e}"))?;
    for name in presets.keys() {
        HubSelector::validate_operator_preset_name(name).map_err(|e| format!("{key}: {e}"))?;
    }
    Ok(presets)
}

fn parse_headers_json(upper: &str, raw: &str) -> Result<Vec<(String, String)>, String> {
    let parsed: serde_json::Value = serde_json::from_str(raw)
        .map_err(|e| format!("LLM_HUB_{upper}_HEADERS is not valid JSON: {e}"))?;
    let obj = parsed
        .as_object()
        .ok_or_else(|| format!("LLM_HUB_{upper}_HEADERS must be a JSON object"))?;
    let mut headers = Vec::with_capacity(obj.len());
    for (k, v) in obj {
        let value = v
            .as_str()
            .ok_or_else(|| format!("LLM_HUB_{upper}_HEADERS values must be strings (key {k})"))?;
        headers.push((k.clone(), value.to_string()));
    }
    Ok(headers)
}

/// Profiles found by scanning for `LLM_HUB_<NAME>_BASE_URL` vars — the one
/// key every profile must have. Names come back lowercased (env segments
/// lose the original case/dashes); sorted for a stable order.
fn discover_profile_names(vars: &HashMap<String, String>) -> Vec<String> {
    let mut names: Vec<String> = vars
        .keys()
        .filter_map(|key| key.strip_prefix(PREFIX)?.strip_suffix("_BASE_URL"))
        .filter(|segment| !segment.is_empty())
        .map(str::to_lowercase)
        .collect();
    names.sort_unstable();
    names
}

/// The user's home directory from `HOME`, or `USERPROFILE` on Windows.
fn home_dir(vars: &HashMap<String, String>) -> Option<&str> {
    vars.get("HOME")
        .or_else(|| vars.get("USERPROFILE"))
        .map(String::as_str)
}

/// Env-var segment for a profile name: uppercased, `-` mapped to `_`.
pub fn env_name(profile: &str) -> String {
    profile.to_uppercase().replace('-', "_")
}

fn split_list(list: &str) -> Vec<String> {
    list.split(',')
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .collect()
}

fn keep_warm_from_map(get: &impl Fn(&str) -> Option<String>) -> Result<KeepWarmConfig, String> {
    let enabled = get("KEEP_WARM").map_or(DEFAULT_KEEP_WARM_ENABLED, |v| !is_falsy(&v));
    let idle_cap_minutes = match get("KEEP_WARM_IDLE_CAP_MINUTES") {
        Some(v) if !v.is_empty() => {
            let minutes: u64 = v
                .parse()
                .map_err(|_| format!("LLM_HUB_KEEP_WARM_IDLE_CAP_MINUTES is not a number: {v}"))?;
            KeepWarmConfig::check_idle_cap_minutes(minutes)
                .map_err(|e| format!("LLM_HUB_KEEP_WARM_IDLE_CAP_MINUTES: {e}: {v}"))?
        }
        _ => DEFAULT_KEEP_WARM_IDLE_CAP_MINUTES,
    };
    Ok(KeepWarmConfig {
        enabled,
        idle_cap: Duration::from_mins(idle_cap_minutes),
    })
}

fn is_truthy(v: &str) -> bool {
    matches!(v.to_lowercase().as_str(), "1" | "true" | "yes" | "on")
}

/// Opt-out flags default to true: only an explicit negative turns them off.
fn is_falsy(v: &str) -> bool {
    matches!(v.to_lowercase().as_str(), "0" | "false" | "no" | "off")
}

/// Masks an api key for display: `sk-a...wxyz`. Short keys mask entirely.
pub fn mask_key(key: &str) -> String {
    if key.is_empty() {
        return String::new();
    }
    let chars: Vec<char> = key.chars().collect();
    if chars.len() <= 8 {
        return "***".to_string();
    }
    let head: String = chars[..4].iter().collect();
    let tail: String = chars[chars.len() - 4..].iter().collect();
    format!("{head}...{tail}")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn base_vars() -> HashMap<String, String> {
        HashMap::from([
            ("LLM_HUB_PROFILES".into(), "openai, groq".into()),
            (
                "LLM_HUB_OPENAI_BASE_URL".into(),
                "https://api.openai.com/v1/".into(),
            ),
            ("LLM_HUB_OPENAI_API_KEY".into(), "sk-test-1234567890".into()),
            (
                "LLM_HUB_GROQ_BASE_URL".into(),
                "https://api.groq.com/openai/v1".into(),
            ),
            ("LLM_HUB_GROQ_API_KEY".into(), "gsk-abc".into()),
        ])
    }

    #[test]
    fn parses_profiles_and_defaults() {
        let cfg = HubConfig::from_map(&base_vars()).unwrap();
        assert_eq!(cfg.profiles.len(), 2);
        assert_eq!(cfg.profiles[0].base_url, "https://api.openai.com/v1");
        assert!(cfg.profiles[0].enabled);
        assert_eq!(cfg.max_replay_bytes, DEFAULT_MAX_REPLAY_BYTES);
        assert_eq!(cfg.max_request_body_bytes, DEFAULT_MAX_REQUEST_BODY_BYTES);
        assert_eq!(cfg.master_key, None);
        assert!(!cfg.config_readonly);
        assert_eq!(cfg.bind, DEFAULT_BIND);
        assert!(!cfg.persistent);
        assert_eq!(cfg.store_kind, "sqlite");
    }

    #[test]
    fn discovers_profiles_without_index_var() {
        let mut vars = base_vars();
        vars.remove("LLM_HUB_PROFILES");
        let cfg = HubConfig::from_map(&vars).unwrap();
        let mut names: Vec<&str> = cfg.profiles.iter().map(|p| p.name.as_str()).collect();
        names.sort_unstable();
        assert_eq!(names, vec!["groq", "openai"]);
    }

    #[test]
    fn index_var_still_controls_selection_when_present() {
        // groq has a BASE_URL but is not listed -> not loaded.
        let mut vars = base_vars();
        vars.insert("LLM_HUB_PROFILES".into(), "openai".into());
        let cfg = HubConfig::from_map(&vars).unwrap();
        assert_eq!(cfg.profiles.len(), 1);
        assert_eq!(cfg.profiles[0].name, "openai");
    }

    #[test]
    fn max_request_body_limit_defaults_and_parses() {
        let mut vars = base_vars();
        assert_eq!(
            HubConfig::from_map(&vars).unwrap().max_request_body_bytes,
            32 * 1024 * 1024
        );
        vars.insert("LLM_HUB_MAX_REQUEST_BODY_BYTES".into(), "1024".into());
        assert_eq!(
            HubConfig::from_map(&vars).unwrap().max_request_body_bytes,
            1024
        );
        vars.insert("LLM_HUB_MAX_REQUEST_BODY_BYTES".into(), "lots".into());
        assert!(HubConfig::from_map(&vars).is_err());
    }

    #[test]
    fn rejects_slash_in_profile_name() {
        let mut vars = base_vars();
        vars.insert("LLM_HUB_PROFILES".into(), "bad/name".into());
        let err = HubConfig::from_map(&vars).unwrap_err();
        assert!(err.contains("may not contain '/'"));
    }

    #[test]
    fn rejects_the_reserved_hub_profile_name() {
        for name in ["hub", "HUB"] {
            let mut vars = base_vars();
            vars.insert("LLM_HUB_PROFILES".into(), name.into());
            vars.insert(
                "LLM_HUB_HUB_BASE_URL".into(),
                "https://example.com/v1".into(),
            );
            let err = HubConfig::from_map(&vars).unwrap_err();
            assert!(err.contains("reserved"), "{name}: {err}");
        }
    }

    #[test]
    fn config_without_smart_auto_presets_parses_with_none() {
        assert!(HubConfig::from_map(&base_vars()).is_ok());
        assert!(
            smart_auto_presets_from_map(&base_vars())
                .unwrap()
                .is_empty()
        );
        let mut vars = base_vars();
        vars.insert("LLM_HUB_SMART_AUTO_PRESETS".into(), "  ".into());
        assert!(smart_auto_presets_from_map(&vars).unwrap().is_empty());
    }

    #[test]
    fn smart_auto_presets_parse_into_policies() {
        let mut vars = base_vars();
        vars.insert(
            "LLM_HUB_SMART_AUTO_PRESETS".into(),
            r#"{"team":{"tier":"fast","include":[["copilot/*"]],"min_context_tokens":1000000}}"#
                .into(),
        );
        let presets = smart_auto_presets_from_map(&vars).unwrap();
        let team = &presets["team"];
        assert_eq!(
            team.tier,
            Some(crate::schemas::smart_auto_tier::SmartAutoTier::Fast)
        );
        assert_eq!(team.include, vec![["copilot/*".to_string()].into()]);
        assert_eq!(team.min_context_tokens, Some(1_000_000));
    }

    #[test]
    fn smart_auto_presets_reject_bad_json_unknown_fields_and_reserved_names() {
        for (raw, expected) in [
            ("[]", "not a valid preset map"),
            (r#"{"team":{"min_ctx":1}}"#, "not a valid preset map"),
            (r#"{"smart-auto-balance":{}}"#, "reserved"),
            (r#"{"a/b":{}}"#, "malformed"),
        ] {
            let mut vars = base_vars();
            vars.insert("LLM_HUB_SMART_AUTO_PRESETS".into(), raw.into());
            let err = smart_auto_presets_from_map(&vars).unwrap_err();
            assert!(err.contains("LLM_HUB_SMART_AUTO_PRESETS"), "{raw}: {err}");
            assert!(err.contains(expected), "{raw}: {err}");
        }
    }

    #[test]
    fn rejects_missing_base_url() {
        let mut vars = base_vars();
        vars.remove("LLM_HUB_GROQ_BASE_URL");
        assert!(HubConfig::from_map(&vars).is_err());
    }

    #[test]
    fn parses_optional_fields() {
        let mut vars = base_vars();
        vars.insert("LLM_HUB_GROQ_HEADERS".into(), r#"{"x-org":"me"}"#.into());
        vars.insert("LLM_HUB_GROQ_TIMEOUT_MS".into(), "30000".into());
        vars.insert("LLM_HUB_GROQ_ENABLED".into(), "false".into());
        vars.insert("LLM_HUB_GROQ_MODELS".into(), "a, b".into());
        vars.insert("LLM_HUB_GROQ_DISPLAY_NAME".into(), "Groq Cloud".into());
        vars.insert("LLM_HUB_MASTER_KEY".into(), "secret".into());
        let cfg = HubConfig::from_map(&vars).unwrap();
        let groq = cfg.profile("groq").unwrap();
        assert_eq!(
            groq.extra_headers,
            vec![("x-org".to_string(), "me".to_string())]
        );
        assert_eq!(groq.timeout_ms, Some(30000));
        assert!(!groq.enabled);
        assert_eq!(groq.static_models, vec!["a", "b"]);
        assert_eq!(groq.display_name.as_deref(), Some("Groq Cloud"));
        assert_eq!(groq.label(), "Groq Cloud");
        assert_eq!(cfg.master_key.as_deref(), Some("secret"));
        assert_eq!(cfg.profile("openai").unwrap().label(), "openai");
    }

    #[test]
    fn managed_command_parsed_from_profile_env() {
        let mut vars = base_vars();
        vars.insert("HOME".into(), "/home/op".into());
        vars.insert(
            "LLM_HUB_GROQ_COMMAND".into(),
            "~/bin/whisper-server --port 443".into(),
        );
        vars.insert("LLM_HUB_GROQ_IDLE_SECS".into(), "0".into());
        let cfg = HubConfig::from_map(&vars).unwrap();
        let managed = cfg.profile("groq").unwrap().managed.clone().unwrap();
        assert_eq!(
            managed.argv,
            vec!["/home/op/bin/whisper-server", "--port", "443"]
        );
        assert_eq!(managed.address, "api.groq.com:443");
        assert_eq!(managed.idle_timeout, None);
        assert_eq!(cfg.profile("openai").unwrap().managed, None);
    }

    #[test]
    fn managed_command_home_falls_back_to_userprofile() {
        let mut vars = base_vars();
        vars.insert("USERPROFILE".into(), r"C:\Users\op".into());
        vars.insert("LLM_HUB_GROQ_COMMAND".into(), "~/bin/whisper-server".into());
        let cfg = HubConfig::from_map(&vars).unwrap();
        let managed = cfg.profile("groq").unwrap().managed.clone().unwrap();
        assert_eq!(managed.argv, vec![r"C:\Users\op/bin/whisper-server"]);
    }

    #[test]
    fn managed_command_bad_start_timeout_rejected() {
        let mut vars = base_vars();
        vars.insert("LLM_HUB_GROQ_COMMAND".into(), "srv".into());
        vars.insert("LLM_HUB_GROQ_START_TIMEOUT_SECS".into(), "x".into());
        let err = HubConfig::from_map(&vars).unwrap_err();
        assert!(err.contains("LLM_HUB_GROQ_START_TIMEOUT_SECS"), "{err}");
    }

    #[test]
    fn copilot_auth_parsed_for_both_flows() {
        let mut vars = base_vars();
        vars.insert("LLM_HUB_PROFILES".into(), "openai, copilot, vscode".into());
        vars.insert(
            "LLM_HUB_COPILOT_BASE_URL".into(),
            "https://api.githubcopilot.com".into(),
        );
        vars.insert("LLM_HUB_COPILOT_AUTH".into(), "github-copilot".into());
        vars.insert("LLM_HUB_COPILOT_GH_PATH".into(), "/opt/bin/ghx".into());
        vars.insert(
            "LLM_HUB_VSCODE_BASE_URL".into(),
            "https://api.githubcopilot.com".into(),
        );
        vars.insert("LLM_HUB_VSCODE_AUTH".into(), "github-copilot-vscode".into());
        let cfg = HubConfig::from_map(&vars).unwrap();

        let copilot = cfg.profile("copilot").unwrap().copilot.clone().unwrap();
        assert_eq!(copilot.flow, crate::configs::CopilotFlow::Opencode);
        assert_eq!(copilot.gh_path, "/opt/bin/ghx");
        let vscode = cfg.profile("vscode").unwrap().copilot.clone().unwrap();
        assert_eq!(vscode.flow, crate::configs::CopilotFlow::Vscode);
        assert_eq!(cfg.profile("openai").unwrap().copilot, None);
    }

    #[test]
    fn copilot_auth_with_api_key_rejected() {
        let mut vars = base_vars();
        vars.insert("LLM_HUB_GROQ_AUTH".into(), "github-copilot".into());
        let err = HubConfig::from_map(&vars).unwrap_err();
        assert!(err.contains("LLM_HUB_GROQ_AUTH"), "{err}");
        assert!(err.contains("LLM_HUB_GROQ_API_KEY"), "{err}");
    }

    #[test]
    fn copilot_auth_error_propagates() {
        let mut vars = base_vars();
        vars.remove("LLM_HUB_GROQ_API_KEY");
        vars.insert("LLM_HUB_GROQ_AUTH".into(), "copilot".into());
        let err = HubConfig::from_map(&vars).unwrap_err();
        assert!(err.contains("LLM_HUB_GROQ_AUTH"), "{err}");
    }

    #[test]
    fn auto_update_defaults_on() {
        let cfg = HubConfig::from_map(&base_vars()).unwrap();
        assert!(cfg.auto_update);
    }

    #[test]
    fn auto_update_parses_explicit_values() {
        for (value, expected) in [("false", false), ("0", false), ("true", true)] {
            let mut vars = base_vars();
            vars.insert("LLM_HUB_AUTO_UPDATE".into(), value.into());
            let cfg = HubConfig::from_map(&vars).unwrap();
            assert_eq!(cfg.auto_update, expected, "LLM_HUB_AUTO_UPDATE={value}");
        }
    }

    #[test]
    fn stream_role_defaults_on_and_parses_explicit_values() {
        assert!(
            HubConfig::from_map(&base_vars())
                .unwrap()
                .stream_role_inject
        );
        for (value, expected) in [("false", false), ("off", false), ("true", true)] {
            let mut vars = base_vars();
            vars.insert("LLM_HUB_STREAM_ROLE".into(), value.into());
            let cfg = HubConfig::from_map(&vars).unwrap();
            assert_eq!(
                cfg.stream_role_inject, expected,
                "LLM_HUB_STREAM_ROLE={value}"
            );
        }
    }

    #[test]
    fn copilot_guard_models_default_and_parse() {
        assert_eq!(
            HubConfig::from_map(&base_vars())
                .unwrap()
                .copilot_guard_models,
            DEFAULT_COPILOT_GUARD_MODELS.map(String::from).to_vec()
        );
        for (value, expected) in [
            ("", vec![]),
            ("*", vec!["*"]),
            (" a , llama_swap/b ,", vec!["a", "llama_swap/b"]),
        ] {
            let mut vars = base_vars();
            vars.insert("LLM_HUB_COPILOT_GUARD_MODELS".into(), value.into());
            let cfg = HubConfig::from_map(&vars).unwrap();
            assert_eq!(cfg.copilot_guard_models, expected, "value={value:?}");
        }
    }

    #[test]
    fn copilot_strip_sampling_models_default_and_parse() {
        assert_eq!(
            HubConfig::from_map(&base_vars())
                .unwrap()
                .copilot_strip_sampling_models,
            DEFAULT_COPILOT_STRIP_SAMPLING_MODELS
                .map(String::from)
                .to_vec()
        );
        let mut vars = base_vars();
        vars.insert(
            "LLM_HUB_COPILOT_STRIP_SAMPLING_MODELS".into(),
            String::new(),
        );
        assert!(
            HubConfig::from_map(&vars)
                .unwrap()
                .copilot_strip_sampling_models
                .is_empty()
        );
    }

    #[test]
    fn sse_keepalive_defaults_to_the_documented_interval() {
        let cfg = HubConfig::from_map(&base_vars()).unwrap();
        assert_eq!(
            cfg.sse_keepalive,
            Some(Duration::from_millis(DEFAULT_SSE_KEEPALIVE_MS))
        );
    }

    #[test]
    fn sse_keepalive_zero_disables_and_a_value_overrides() {
        for (value, expected) in [
            ("0", None),
            ("2500", Some(Duration::from_millis(2500))),
            ("", Some(Duration::from_millis(DEFAULT_SSE_KEEPALIVE_MS))),
        ] {
            let mut vars = base_vars();
            vars.insert("LLM_HUB_SSE_KEEPALIVE_MS".into(), value.into());
            let cfg = HubConfig::from_map(&vars).unwrap();
            assert_eq!(
                cfg.sse_keepalive, expected,
                "LLM_HUB_SSE_KEEPALIVE_MS={value}"
            );
        }
    }

    #[test]
    fn first_byte_timeout_defaults_and_zero_disables() {
        for (value, expected) in [
            (
                None,
                Some(Duration::from_millis(DEFAULT_FIRST_BYTE_TIMEOUT_MS)),
            ),
            (Some("0"), None),
            (Some("5000"), Some(Duration::from_secs(5))),
            (
                Some(""),
                Some(Duration::from_millis(DEFAULT_FIRST_BYTE_TIMEOUT_MS)),
            ),
        ] {
            let mut vars = base_vars();
            if let Some(value) = value {
                vars.insert("LLM_HUB_FIRST_BYTE_TIMEOUT_MS".into(), value.into());
            }
            let cfg = HubConfig::from_map(&vars).unwrap();
            assert_eq!(cfg.first_byte_timeout, expected, "{value:?}");
        }
    }

    #[test]
    fn quota_cooldown_and_deadline_default_parse_and_reject_text() {
        let cfg = HubConfig::from_map(&HashMap::new()).unwrap();
        assert_eq!(
            cfg.quota_cooldown,
            Some(Duration::from_secs(DEFAULT_QUOTA_COOLDOWN_SECS))
        );
        assert_eq!(cfg.deadline, None);

        let mut vars = HashMap::new();
        vars.insert("LLM_HUB_QUOTA_COOLDOWN_SECS".into(), "0".into());
        vars.insert("LLM_HUB_DEADLINE_MS".into(), "45000".into());
        let cfg = HubConfig::from_map(&vars).unwrap();
        assert_eq!(cfg.quota_cooldown, None);
        assert_eq!(cfg.deadline, Some(Duration::from_secs(45)));

        for key in ["LLM_HUB_QUOTA_COOLDOWN_SECS", "LLM_HUB_DEADLINE_MS"] {
            let vars = HashMap::from([(key.to_owned(), "soon".to_owned())]);
            let err = HubConfig::from_map(&vars).unwrap_err();
            assert!(err.contains(key), "{err}");
        }
    }

    #[test]
    fn first_byte_timeout_rejects_a_non_numeric_value() {
        let mut vars = base_vars();
        vars.insert("LLM_HUB_FIRST_BYTE_TIMEOUT_MS".into(), "soon".into());
        let err = HubConfig::from_map(&vars).unwrap_err();
        assert!(err.contains("LLM_HUB_FIRST_BYTE_TIMEOUT_MS"), "{err}");
    }

    #[test]
    fn a_profile_first_byte_timeout_parses_and_rejects_text() {
        let mut vars = base_vars();
        let limit = |vars: &HashMap<String, String>| {
            HubConfig::from_map(vars)
                .unwrap()
                .profile("groq")
                .unwrap()
                .first_byte_timeout_ms
        };
        assert_eq!(limit(&vars), None);
        vars.insert("LLM_HUB_GROQ_FIRST_BYTE_TIMEOUT_MS".into(), "1500".into());
        assert_eq!(limit(&vars), Some(1500));
        vars.insert("LLM_HUB_GROQ_FIRST_BYTE_TIMEOUT_MS".into(), "soon".into());
        let err = HubConfig::from_map(&vars).unwrap_err();
        assert!(err.contains("GROQ_FIRST_BYTE_TIMEOUT_MS"), "{err}");
    }

    #[test]
    fn a_profile_tuner_parses_its_keys_and_resolves_the_guard_budget() {
        let mut vars = base_vars();
        vars.insert("HOME".into(), "/home/op".into());
        vars.insert("LLM_HUB_GROQ_TIMEOUT_MS".into(), "600000".into());
        vars.insert("LLM_HUB_GROQ_FIRST_BYTE_TIMEOUT_MS".into(), "300000".into());
        vars.insert("LLM_HUB_GROQ_PREFILL_GUARD".into(), "on".into());
        vars.insert("LLM_HUB_GROQ_TUNER_REGISTRY".into(), "~/reg.txt".into());
        vars.insert("LLM_HUB_GROQ_TUNER_APPLY_COMMAND".into(), "sync now".into());
        vars.insert("LLM_HUB_GROQ_TUNER_APPLY_TIMEOUT_MS".into(), "9000".into());
        let cfg = HubConfig::from_map(&vars).unwrap();
        let tuner = &cfg.profile("groq").unwrap().tuner;
        assert_eq!(tuner.prefill_guard, Some(300_000));
        assert_eq!(
            tuner.registry,
            Some(std::path::PathBuf::from("/home/op/reg.txt"))
        );
        assert_eq!(
            tuner.apply_command,
            Some(vec!["sync".to_owned(), "now".to_owned()])
        );
        assert_eq!(tuner.apply_timeout, Duration::from_secs(9));
        assert_eq!(
            cfg.profile("openai").unwrap().tuner,
            LocalTunerConfig::default()
        );
    }

    #[test]
    fn a_profile_prefill_guard_on_without_a_timeout_fails_the_load() {
        let mut vars = base_vars();
        vars.insert("LLM_HUB_GROQ_PREFILL_GUARD".into(), "on".into());
        let err = HubConfig::from_map(&vars).unwrap_err();
        assert!(err.contains("LLM_HUB_GROQ_PREFILL_GUARD"), "{err}");
    }

    #[test]
    fn local_tuner_is_opt_in() {
        assert!(
            !HubConfig::from_map(&base_vars())
                .unwrap()
                .local_tuner_enabled
        );
        for (value, expected) in [("true", true), ("on", true), ("false", false)] {
            let mut vars = base_vars();
            vars.insert("LLM_HUB_LOCAL_TUNER".into(), value.into());
            let cfg = HubConfig::from_map(&vars).unwrap();
            assert_eq!(cfg.local_tuner_enabled, expected, "{value}");
        }
    }

    #[test]
    fn notify_defaults_on_and_reads_its_keys() {
        let cfg = HubConfig::from_map(&base_vars()).unwrap();
        assert_eq!(cfg.notify, NotifyConfig::default());
        assert!(cfg.notify.enabled);
        assert!(cfg.notify.desktop);

        let mut vars = base_vars();
        vars.insert("HOME".into(), "/home/op".into());
        vars.insert("LLM_HUB_NOTIFICATIONS".into(), "off".into());
        vars.insert("LLM_HUB_NOTIFY_DESKTOP".into(), "false".into());
        vars.insert("LLM_HUB_NOTIFY_COMMAND".into(), "~/bin/notify".into());
        vars.insert("LLM_HUB_NOTIFY_MUTE".into(), "source=errors".into());
        let cfg = HubConfig::from_map(&vars).unwrap();
        assert!(!cfg.notify.enabled);
        assert!(!cfg.notify.desktop);
        assert_eq!(
            cfg.notify.command,
            Some(vec!["/home/op/bin/notify".to_owned()])
        );
        assert_eq!(cfg.notify.mutes.len(), 1);

        vars.insert("LLM_HUB_NOTIFY_MIN_SEVERITY".into(), "loud".into());
        let err = HubConfig::from_map(&vars).unwrap_err();
        assert!(err.contains("LLM_HUB_NOTIFY_MIN_SEVERITY"), "{err}");
    }

    #[test]
    fn budgets_are_unset_by_default_and_read_the_global_and_profile_keys() {
        let cfg = HubConfig::from_map(&base_vars()).unwrap();
        assert_eq!(cfg.budget_monthly_usd, None);
        assert!(cfg.profiles.iter().all(|p| p.budget_monthly_usd.is_none()));
        assert_eq!(cfg.notify.budget_steps, vec![25, 50, 75, 80, 85, 95, 100]);

        let mut vars = base_vars();
        vars.insert("LLM_HUB_BUDGET_MONTHLY_USD".into(), " 200 ".into());
        vars.insert("LLM_HUB_GROQ_BUDGET_MONTHLY_USD".into(), "12.5".into());
        vars.insert("LLM_HUB_OPENAI_BUDGET_MONTHLY_USD".into(), String::new());
        vars.insert("LLM_HUB_NOTIFY_BUDGET_STEPS".into(), "100,50".into());
        let cfg = HubConfig::from_map(&vars).unwrap();
        assert_eq!(cfg.budget_monthly_usd, Some(200.0));
        assert_eq!(cfg.profile("groq").unwrap().budget_monthly_usd, Some(12.5));
        assert_eq!(cfg.profile("openai").unwrap().budget_monthly_usd, None);
        assert_eq!(cfg.notify.budget_steps, vec![50, 100]);
    }

    #[test]
    fn budgets_must_be_positive_finite_numbers() {
        for (key, value) in [
            ("LLM_HUB_BUDGET_MONTHLY_USD", "0"),
            ("LLM_HUB_BUDGET_MONTHLY_USD", "-3"),
            ("LLM_HUB_BUDGET_MONTHLY_USD", "lots"),
            ("LLM_HUB_BUDGET_MONTHLY_USD", "inf"),
            ("LLM_HUB_GROQ_BUDGET_MONTHLY_USD", "NaN"),
            ("LLM_HUB_GROQ_BUDGET_MONTHLY_USD", "0.0"),
        ] {
            let mut vars = base_vars();
            vars.insert(key.into(), value.into());
            let err = HubConfig::from_map(&vars).unwrap_err();
            assert!(
                err.contains(key) && err.contains("above 0"),
                "{key}={value}: {err}"
            );
        }
    }

    #[test]
    fn a_profile_keeps_its_cache_warm_unless_it_opts_out() {
        let mut vars = base_vars();
        let cfg = HubConfig::from_map(&vars).unwrap();
        assert!(cfg.profiles.iter().all(|profile| profile.keep_warm));

        vars.insert("LLM_HUB_GROQ_KEEP_WARM".into(), "false".into());
        let cfg = HubConfig::from_map(&vars).unwrap();
        let warm = |name: &str| cfg.profile(name).unwrap().keep_warm;
        assert!(warm("openai"));
        assert!(!warm("groq"));
    }

    #[test]
    fn sse_keepalive_rejects_a_non_numeric_value() {
        let mut vars = base_vars();
        vars.insert("LLM_HUB_SSE_KEEPALIVE_MS".into(), "soon".into());
        let err = HubConfig::from_map(&vars).unwrap_err();
        assert!(err.contains("LLM_HUB_SSE_KEEPALIVE_MS"), "{err}");
    }

    #[test]
    fn copilot_prices_refresh_defaults_to_a_day_zero_disables_and_bad_value_is_named() {
        for (value, expected) in [
            ("", Some(Duration::from_hours(24))),
            ("0", None),
            ("3600", Some(Duration::from_hours(1))),
        ] {
            let mut vars = base_vars();
            vars.insert("LLM_HUB_COPILOT_PRICES_REFRESH_SECS".into(), value.into());
            let cfg = HubConfig::from_map(&vars).unwrap();
            assert_eq!(cfg.copilot_prices_refresh, expected, "{value:?}");
        }
        let mut vars = base_vars();
        vars.insert("LLM_HUB_COPILOT_PRICES_REFRESH_SECS".into(), "daily".into());
        let err = HubConfig::from_map(&vars).unwrap_err();
        assert!(err.contains("LLM_HUB_COPILOT_PRICES_REFRESH_SECS"), "{err}");
    }

    #[test]
    fn keep_warm_defaults_on_with_a_thirty_minute_cap() {
        let cfg = HubConfig::from_map(&base_vars()).unwrap();
        assert_eq!(cfg.keep_warm, KeepWarmConfig::default());
        assert!(cfg.keep_warm.enabled);
        assert_eq!(cfg.keep_warm.idle_cap, Duration::from_mins(30));
    }

    #[test]
    fn keep_warm_accepts_an_override() {
        let mut vars = base_vars();
        vars.insert("LLM_HUB_KEEP_WARM".into(), "false".into());
        vars.insert("LLM_HUB_KEEP_WARM_IDLE_CAP_MINUTES".into(), "45".into());
        let cfg = HubConfig::from_map(&vars).unwrap();
        assert!(!cfg.keep_warm.enabled);
        assert_eq!(cfg.keep_warm.idle_cap, Duration::from_mins(45));
    }

    #[test]
    fn keep_warm_cap_below_five_minutes_is_rejected() {
        let mut vars = base_vars();
        vars.insert("LLM_HUB_KEEP_WARM_IDLE_CAP_MINUTES".into(), "4".into());
        let err = HubConfig::from_map(&vars).unwrap_err();
        assert!(err.contains("LLM_HUB_KEEP_WARM_IDLE_CAP_MINUTES"), "{err}");
    }

    #[test]
    fn keep_warm_cap_above_one_week_is_rejected_instead_of_panicking() {
        for minutes in ["10081", "18446744073709551615"] {
            let mut vars = base_vars();
            vars.insert("LLM_HUB_KEEP_WARM_IDLE_CAP_MINUTES".into(), minutes.into());
            let err = HubConfig::from_map(&vars).unwrap_err();
            assert!(err.contains("LLM_HUB_KEEP_WARM_IDLE_CAP_MINUTES"), "{err}");
        }
    }

    #[test]
    fn keep_warm_cap_that_is_not_a_number_is_rejected() {
        let mut vars = base_vars();
        vars.insert("LLM_HUB_KEEP_WARM_IDLE_CAP_MINUTES".into(), "half".into());
        let err = HubConfig::from_map(&vars).unwrap_err();
        assert!(err.contains("LLM_HUB_KEEP_WARM_IDLE_CAP_MINUTES"), "{err}");
    }

    #[test]
    fn dashed_profile_env_name() {
        assert_eq!(env_name("my-proxy"), "MY_PROXY");
    }

    #[test]
    fn masks_keys() {
        assert_eq!(mask_key(""), "");
        assert_eq!(mask_key("short"), "***");
        assert_eq!(mask_key("sk-abcdefgh1234"), "sk-a...1234");
    }

    fn usage_vars(profiles: &[(&str, &str, Option<&str>)]) -> HashMap<String, String> {
        let mut vars = HashMap::new();
        let names: Vec<&str> = profiles.iter().map(|(name, _, _)| *name).collect();
        vars.insert("LLM_HUB_PROFILES".into(), names.join(","));
        for (name, key, usage) in profiles {
            let upper = env_name(name);
            vars.insert(
                format!("LLM_HUB_{upper}_BASE_URL"),
                "http://127.0.0.1:9/v1".into(),
            );
            vars.insert(format!("LLM_HUB_{upper}_API_KEY"), (*key).into());
            if let Some(usage) = usage {
                vars.insert(format!("LLM_HUB_{upper}_USAGE"), (*usage).into());
            }
        }
        vars
    }

    #[test]
    fn usage_source_follows_the_env_var_then_the_key_prefix() {
        let cfg = HubConfig::from_map(&usage_vars(&[
            ("prefixed", "sk-bf-abc", None),
            ("plain", "sk-other", None),
            ("opted_out", "sk-bf-abc", Some("none")),
            ("opted_in", "sk-other", Some("Bifrost")),
        ]))
        .unwrap();

        assert_eq!(cfg.usage_source("prefixed"), Some(UsageSource::Bifrost));
        assert_eq!(cfg.usage_source("plain"), None);
        assert_eq!(cfg.usage_source("opted_out"), None);
        assert_eq!(cfg.usage_source("opted_in"), Some(UsageSource::Bifrost));
    }

    #[test]
    fn unknown_usage_value_is_rejected() {
        let err = HubConfig::from_map(&usage_vars(&[("gw", "k", Some("datadog"))])).unwrap_err();

        assert!(err.contains("LLM_HUB_GW_USAGE"), "{err}");
    }
}
