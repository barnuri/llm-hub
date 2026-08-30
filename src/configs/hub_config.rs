use std::collections::HashMap;

use super::profile_config::ProfileConfig;
use crate::consts::{DEFAULT_BIND, DEFAULT_MAX_REPLAY_BYTES, DEFAULT_PORT};

const PREFIX: &str = "LLM_HUB_";

#[derive(Debug, Clone)]
pub struct HubConfig {
    pub profiles: Vec<ProfileConfig>,
    pub master_key: Option<String>,
    pub default_fallbacks: Vec<String>,
    pub max_replay_bytes: usize,
    pub config_readonly: bool,
    pub bind: String,
    pub port: u16,
    pub persistent: bool,
    pub store_kind: String,
    pub store_path: Option<String>,
}

impl HubConfig {
    pub fn profile(&self, name: &str) -> Option<&ProfileConfig> {
        self.profiles.iter().find(|p| p.name == name)
    }

    pub fn from_map(vars: &HashMap<String, String>) -> Result<HubConfig, String> {
        let get = |key: &str| {
            vars.get(&format!("{PREFIX}{key}"))
                .map(|v| v.trim().to_string())
        };

        let profile_names = match get("PROFILES") {
            Some(list) if !list.is_empty() => {
                list.split(',').map(|s| s.trim().to_string()).filter(|s| !s.is_empty()).collect()
            }
            // No index var: discover profiles from LLM_HUB_<NAME>_BASE_URL keys.
            _ => discover_profile_names(vars),
        };

        let mut profiles = Vec::with_capacity(profile_names.len());
        for name in &profile_names {
            profiles.push(Self::parse_profile(name, vars)?);
        }

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
        let port = match get("PORT") {
            Some(v) => v
                .parse()
                .map_err(|_| format!("LLM_HUB_PORT is not a valid port: {v}"))?,
            None => DEFAULT_PORT,
        };

        Ok(HubConfig {
            profiles,
            master_key,
            default_fallbacks,
            max_replay_bytes,
            config_readonly: get("CONFIG_READONLY").is_some_and(|v| is_truthy(&v)),
            bind: get("BIND").unwrap_or_else(|| DEFAULT_BIND.to_string()),
            port,
            persistent: get("PERSISTENT").is_some_and(|v| is_truthy(&v)),
            store_kind: get("STORE").unwrap_or_else(|| "sqlite".to_string()),
            store_path: get("STORE_PATH").filter(|v| !v.is_empty()),
        })
    }

    fn parse_profile(name: &str, vars: &HashMap<String, String>) -> Result<ProfileConfig, String> {
        if name.contains('/') {
            return Err(format!("profile name may not contain '/': {name}"));
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

        Ok(ProfileConfig {
            name: name.to_string(),
            base_url: base_url.trim_end_matches('/').to_string(),
            api_key: get("API_KEY").unwrap_or_default(),
            extra_headers,
            timeout_ms,
            enabled: get("ENABLED").is_none_or(|v| is_truthy(&v)),
            static_models: get("MODELS")
                .unwrap_or_default()
                .split(',')
                .map(|s| s.trim().to_string())
                .filter(|s| !s.is_empty())
                .collect(),
        })
    }
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
        .map(|segment| segment.to_lowercase())
        .collect();
    names.sort();
    names
}

/// Env-var segment for a profile name: uppercased, `-` mapped to `_`.
pub fn env_name(profile: &str) -> String {
    profile.to_uppercase().replace('-', "_")
}

fn is_truthy(v: &str) -> bool {
    matches!(v.to_lowercase().as_str(), "1" | "true" | "yes" | "on")
}

/// Masks an api key for display: `sk-a...wxyz`. Short keys mask entirely.
pub fn mask_key(key: &str) -> String {
    if key.is_empty() {
        return String::new();
    }
    if key.len() <= 8 {
        return "***".to_string();
    }
    format!("{}...{}", &key[..4], &key[key.len() - 4..])
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
        names.sort();
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
    fn rejects_slash_in_profile_name() {
        let mut vars = base_vars();
        vars.insert("LLM_HUB_PROFILES".into(), "bad/name".into());
        let err = HubConfig::from_map(&vars).unwrap_err();
        assert!(err.contains("may not contain '/'"));
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
        assert_eq!(cfg.master_key.as_deref(), Some("secret"));
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
}
