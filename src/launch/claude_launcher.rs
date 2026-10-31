use super::hub_endpoint::HubEndpoint;
use super::launch_args::LaunchArgs;
use super::launch_spec::LaunchSpec;

/// `llm-hub claude`: Claude Code on the hub's `/v1/messages`.
#[derive(Debug)]
pub struct ClaudeLauncher;

impl ClaudeLauncher {
    const PROGRAM: &str = "claude";
    const SERVER_NAME: &str = "llm-hub";
    const MODEL_VARS: [&str; 6] = [
        "ANTHROPIC_MODEL",
        "ANTHROPIC_DEFAULT_OPUS_MODEL",
        "ANTHROPIC_DEFAULT_SONNET_MODEL",
        "ANTHROPIC_DEFAULT_HAIKU_MODEL",
        "ANTHROPIC_DEFAULT_FABLE_MODEL",
        "CLAUDE_CODE_SUBAGENT_MODEL",
    ];

    /// `settings` is the hub's `/api/claude-code/settings` body, passed as
    /// `--settings` so `/model` lists every hub model.
    #[must_use]
    pub fn spec(endpoint: &HubEndpoint, args: LaunchArgs, settings: Option<String>) -> LaunchSpec {
        let mut spec = LaunchSpec::new(Self::PROGRAM)
            .env("ENABLE_TOOL_SEARCH", "true")
            .env("CLAUDE_CODE_ENABLE_GATEWAY_MODEL_DISCOVERY", "1")
            .env("CLAUDE_CODE_DISABLE_UNKNOWN_MODEL_WINDOW_ENFORCEMENT", "1")
            .env("ANTHROPIC_SERVER_NAME", Self::SERVER_NAME)
            .env("ANTHROPIC_BASE_URL", endpoint.url())
            .env("ANTHROPIC_AUTH_TOKEN", endpoint.api_key());
        if let Some(model) = args.model() {
            for name in Self::MODEL_VARS {
                spec = spec.env(name, model);
            }
        }
        if let Some(settings) = settings {
            spec = spec.arg("--settings").arg(settings);
        }
        spec.args(args.into_passthrough())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn endpoint() -> HubEndpoint {
        HubEndpoint::from_lookup(|_| None)
    }

    fn args(values: &[&str]) -> LaunchArgs {
        LaunchArgs::parse(values.iter().map(ToString::to_string).collect())
    }

    #[test]
    fn base_env_points_claude_at_the_hub() {
        let spec = ClaudeLauncher::spec(&endpoint(), args(&[]), None);

        assert_eq!(spec.program(), "claude");
        assert_eq!(
            spec.env_names(),
            [
                "ENABLE_TOOL_SEARCH",
                "CLAUDE_CODE_ENABLE_GATEWAY_MODEL_DISCOVERY",
                "CLAUDE_CODE_DISABLE_UNKNOWN_MODEL_WINDOW_ENFORCEMENT",
                "ANTHROPIC_SERVER_NAME",
                "ANTHROPIC_BASE_URL",
                "ANTHROPIC_AUTH_TOKEN",
            ]
        );
        assert_eq!(spec.env_value("ENABLE_TOOL_SEARCH"), Some("true"));
        assert_eq!(spec.env_value("ANTHROPIC_SERVER_NAME"), Some("llm-hub"));
        assert_eq!(
            spec.env_value("ANTHROPIC_BASE_URL"),
            Some("http://localhost:8888")
        );
        assert_eq!(spec.env_value("ANTHROPIC_AUTH_TOKEN"), Some("llm-hub"));
        assert!(spec.argv().is_empty());
    }

    #[test]
    fn model_sets_every_model_var() {
        let spec = ClaudeLauncher::spec(&endpoint(), args(&["copilot/claude-opus-5-5"]), None);

        for name in ClaudeLauncher::MODEL_VARS {
            assert_eq!(spec.env_value(name), Some("copilot/claude-opus-5-5"));
        }
        assert!(spec.argv().is_empty());
    }

    #[test]
    fn env_never_disables_betas_or_shrinks_the_compact_window() {
        let spec = ClaudeLauncher::spec(&endpoint(), args(&["m"]), None);

        assert_eq!(
            spec.env_value("CLAUDE_CODE_DISABLE_EXPERIMENTAL_BETAS"),
            None
        );
        assert_eq!(spec.env_value("CLAUDE_CODE_AUTO_COMPACT_WINDOW"), None);
    }

    #[test]
    fn settings_come_before_the_passed_arguments() {
        let spec = ClaudeLauncher::spec(
            &endpoint(),
            args(&["-p", "hi"]),
            Some("{\"modelPicker\":{}}".to_string()),
        );

        assert_eq!(spec.env_value("ANTHROPIC_MODEL"), None);
        assert_eq!(
            spec.argv(),
            ["--settings", "{\"modelPicker\":{}}", "-p", "hi"]
        );
    }

    #[test]
    fn no_settings_means_no_settings_flag() {
        let spec = ClaudeLauncher::spec(&endpoint(), args(&["m", "--resume"]), None);

        assert_eq!(spec.argv(), ["--resume"]);
    }
}
