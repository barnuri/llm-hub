use serde_json::json;

use super::config_paths::ConfigPaths;
use super::json_config::JsonConfig;
use super::setup_plan::{SetupOutcome, SetupPlan};
use crate::services::launch::hub_endpoint::HubEndpoint;

/// `llm-hub setup claude`: the hub's Claude Code settings and the connection variables in
/// `~/.claude/settings.json`, which Claude Code reads on every start.
#[derive(Debug)]
pub struct ClaudeSetup;

impl ClaudeSetup {
    const FILE: &str = "settings.json";

    /// `hub_settings` is the hub's `/api/claude-code/settings` body, merged under the connection
    /// variables.
    ///
    /// # Errors
    /// Returns a message when the hub settings or the existing file are not a JSON object, or no
    /// config dir is known.
    pub fn plan(
        lookup: impl Fn(&str) -> Option<String>,
        endpoint: &HubEndpoint,
        hub_settings: Option<&str>,
    ) -> Result<SetupOutcome, String> {
        let path = ConfigPaths::dir(lookup, "CLAUDE_CONFIG_DIR", ".claude")?.join(Self::FILE);
        let existing = SetupPlan::read_existing(&path)?;
        let content = Self::render(existing.as_deref(), endpoint, hub_settings)?;
        Ok(SetupOutcome::Write(SetupPlan { path, content }))
    }

    fn render(
        existing: Option<&str>,
        endpoint: &HubEndpoint,
        hub_settings: Option<&str>,
    ) -> Result<String, String> {
        let mut root = JsonConfig::parse_object(existing, Self::FILE)?;
        let from_hub = JsonConfig::parse_object(hub_settings, "the hub's Claude Code settings")?;
        JsonConfig::merge(&mut root, from_hub);
        let env = JsonConfig::object_entry(&mut root, "env")?;
        let variables = [
            ("ENABLE_TOOL_SEARCH", "true"),
            ("CLAUDE_CODE_ENABLE_GATEWAY_MODEL_DISCOVERY", "1"),
            ("CLAUDE_CODE_DISABLE_UNKNOWN_MODEL_WINDOW_ENFORCEMENT", "1"),
            ("ANTHROPIC_SERVER_NAME", "llm-hub"),
            ("ANTHROPIC_BASE_URL", endpoint.url()),
            ("ANTHROPIC_AUTH_TOKEN", endpoint.api_key()),
        ];
        for (name, value) in variables {
            env.insert(name.to_string(), json!(value));
        }
        JsonConfig::render(root)
    }
}

#[cfg(test)]
mod tests {
    use serde_json::Value;

    use super::*;

    fn endpoint() -> HubEndpoint {
        HubEndpoint::from_lookup(|name| (name == "LLM_HUB_API_KEY").then(|| "k".to_string()))
    }

    fn render(existing: Option<&str>, hub: Option<&str>) -> Value {
        serde_json::from_str(&ClaudeSetup::render(existing, &endpoint(), hub).unwrap()).unwrap()
    }

    #[test]
    fn the_connection_variables_go_in_env() {
        let settings = render(None, None);

        assert_eq!(
            settings["env"],
            json!({
                "ENABLE_TOOL_SEARCH": "true",
                "CLAUDE_CODE_ENABLE_GATEWAY_MODEL_DISCOVERY": "1",
                "CLAUDE_CODE_DISABLE_UNKNOWN_MODEL_WINDOW_ENFORCEMENT": "1",
                "ANTHROPIC_SERVER_NAME": "llm-hub",
                "ANTHROPIC_BASE_URL": "http://localhost:8888",
                "ANTHROPIC_AUTH_TOKEN": "k",
            })
        );
    }

    #[test]
    fn existing_keys_and_the_hub_settings_are_merged() {
        let settings = render(
            Some(r#"{"theme": "dark", "env": {"MY_VAR": "1", "ANTHROPIC_BASE_URL": "old"}}"#),
            Some(r#"{"model": "hub/x", "env": {"FROM_HUB": "2"}}"#),
        );

        assert_eq!(settings["theme"], "dark");
        assert_eq!(settings["model"], "hub/x");
        assert_eq!(settings["env"]["MY_VAR"], "1");
        assert_eq!(settings["env"]["FROM_HUB"], "2");
        assert_eq!(
            settings["env"]["ANTHROPIC_BASE_URL"],
            "http://localhost:8888"
        );
    }

    #[test]
    fn rendering_its_own_output_is_stable() {
        let first = ClaudeSetup::render(None, &endpoint(), Some(r#"{"a": 1}"#)).unwrap();

        let second = ClaudeSetup::render(Some(&first), &endpoint(), Some(r#"{"a": 1}"#)).unwrap();

        assert_eq!(first, second);
    }

    #[test]
    fn hub_settings_that_are_not_an_object_are_an_error() {
        assert!(ClaudeSetup::render(None, &endpoint(), Some("[1]")).is_err());
    }

    #[test]
    fn plan_uses_the_claude_config_dir_override() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().to_string_lossy().into_owned();

        let outcome = ClaudeSetup::plan(
            |name| (name == "CLAUDE_CONFIG_DIR").then(|| root.clone()),
            &endpoint(),
            None,
        )
        .unwrap();

        let SetupOutcome::Write(plan) = outcome else {
            panic!("expected a file");
        };
        assert_eq!(plan.path, dir.path().join("settings.json"));
    }
}
