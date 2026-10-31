use super::hub_endpoint::HubEndpoint;
use crate::schemas::launch::launch_args::LaunchArgs;
use crate::schemas::launch::launch_spec::LaunchSpec;
use crate::schemas::launch::profile_filter::ProfileFilter;

/// `llm-hub claude`: Claude Code on the hub's `/v1/messages`.
#[derive(Debug)]
pub struct ClaudeLauncher;

impl ClaudeLauncher {
    const PROGRAM: &str = "claude";
    const SERVER_NAME: &str = "llm-hub";
    const CUSTOM_HEADERS_VAR: &str = "ANTHROPIC_CUSTOM_HEADERS";
    const MODEL_VARS: [&str; 6] = [
        "ANTHROPIC_MODEL",
        "ANTHROPIC_DEFAULT_OPUS_MODEL",
        "ANTHROPIC_DEFAULT_SONNET_MODEL",
        "ANTHROPIC_DEFAULT_HAIKU_MODEL",
        "ANTHROPIC_DEFAULT_FABLE_MODEL",
        "CLAUDE_CODE_SUBAGENT_MODEL",
    ];

    /// `settings` is the hub's `/api/claude-code/settings` body, passed as
    /// `--settings` so `/model` lists every hub model. A non-empty `profiles`
    /// adds `x-llm-hub-profiles` to `ANTHROPIC_CUSTOM_HEADERS`, after the
    /// caller's own `inherited_headers`, so gateway discovery is limited too.
    #[must_use]
    pub fn spec(
        endpoint: &HubEndpoint,
        args: LaunchArgs,
        settings: Option<String>,
        profiles: &ProfileFilter,
        inherited_headers: Option<&str>,
    ) -> LaunchSpec {
        let mut spec = LaunchSpec::new(Self::PROGRAM)
            .env("ENABLE_TOOL_SEARCH", "true")
            .env("CLAUDE_CODE_ENABLE_GATEWAY_MODEL_DISCOVERY", "1")
            .env("CLAUDE_CODE_DISABLE_UNKNOWN_MODEL_WINDOW_ENFORCEMENT", "1")
            .env("ANTHROPIC_SERVER_NAME", Self::SERVER_NAME)
            .env("ANTHROPIC_BASE_URL", endpoint.url())
            .env("ANTHROPIC_AUTH_TOKEN", endpoint.api_key());
        if let Some(headers) = Self::custom_headers(profiles, inherited_headers) {
            spec = spec.env(Self::CUSTOM_HEADERS_VAR, headers);
        }
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

    /// The env var that carries the caller's own Claude Code headers.
    #[must_use]
    pub fn custom_headers_var() -> &'static str {
        Self::CUSTOM_HEADERS_VAR
    }

    /// `ANTHROPIC_CUSTOM_HEADERS` is one `Name: value` per line.
    fn custom_headers(profiles: &ProfileFilter, inherited: Option<&str>) -> Option<String> {
        if profiles.is_empty() {
            return None;
        }
        let header = format!("{}: {}", ProfileFilter::HEADER, profiles.to_list());
        Some(
            match inherited.map(str::trim_end).filter(|v| !v.is_empty()) {
                Some(existing) => format!("{existing}\n{header}"),
                None => header,
            },
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn endpoint() -> HubEndpoint {
        HubEndpoint::from_lookup(|_| None)
    }

    fn spec_of(args: LaunchArgs, settings: Option<String>) -> LaunchSpec {
        ClaudeLauncher::spec(&endpoint(), args, settings, &ProfileFilter::default(), None)
    }

    fn profiled(inherited: Option<&str>) -> LaunchSpec {
        ClaudeLauncher::spec(
            &endpoint(),
            LaunchArgs::from_strs(&[]),
            None,
            &ProfileFilter::from_list("copilot,ivrit"),
            inherited,
        )
    }

    #[test]
    fn profiles_set_the_custom_header() {
        let spec = profiled(None);

        assert_eq!(
            spec.env_value("ANTHROPIC_CUSTOM_HEADERS"),
            Some("x-llm-hub-profiles: copilot,ivrit")
        );
    }

    #[test]
    fn profiles_header_is_appended_to_existing_custom_headers() {
        let spec = profiled(Some("X-Team: core\nX-Trace: 1\n"));

        assert_eq!(
            spec.env_value("ANTHROPIC_CUSTOM_HEADERS"),
            Some("X-Team: core\nX-Trace: 1\nx-llm-hub-profiles: copilot,ivrit")
        );
    }

    #[test]
    fn empty_existing_custom_headers_count_as_unset() {
        let spec = profiled(Some(""));

        assert_eq!(
            spec.env_value("ANTHROPIC_CUSTOM_HEADERS"),
            Some("x-llm-hub-profiles: copilot,ivrit")
        );
    }

    #[test]
    fn no_profiles_leave_custom_headers_alone() {
        let spec = ClaudeLauncher::spec(
            &endpoint(),
            LaunchArgs::from_strs(&[]),
            None,
            &ProfileFilter::default(),
            Some("X-Team: core"),
        );

        assert_eq!(spec.env_value("ANTHROPIC_CUSTOM_HEADERS"), None);
    }

    #[test]
    fn base_env_points_claude_at_the_hub() {
        let spec = spec_of(LaunchArgs::from_strs(&[]), None);

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
        let spec = spec_of(LaunchArgs::from_strs(&["copilot/claude-opus-5-5"]), None);

        for name in ClaudeLauncher::MODEL_VARS {
            assert_eq!(spec.env_value(name), Some("copilot/claude-opus-5-5"));
        }
        assert!(spec.argv().is_empty());
    }

    #[test]
    fn env_never_disables_betas_or_shrinks_the_compact_window() {
        let spec = spec_of(LaunchArgs::from_strs(&["m"]), None);

        assert_eq!(
            spec.env_value("CLAUDE_CODE_DISABLE_EXPERIMENTAL_BETAS"),
            None
        );
        assert_eq!(spec.env_value("CLAUDE_CODE_AUTO_COMPACT_WINDOW"), None);
    }

    #[test]
    fn settings_come_before_the_passed_arguments() {
        let spec = spec_of(
            LaunchArgs::from_strs(&["-p", "hi"]),
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
        let spec = spec_of(LaunchArgs::from_strs(&["m", "--resume"]), None);

        assert_eq!(spec.argv(), ["--resume"]);
    }
}
