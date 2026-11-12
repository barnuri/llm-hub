use super::hub_endpoint::HubEndpoint;
use crate::schemas::launch::hub_model::HubModel;
use crate::schemas::launch::launch_args::LaunchArgs;
use crate::schemas::launch::launch_spec::LaunchSpec;

/// `llm-hub copilot-cli`: GitHub Copilot CLI in BYOK mode on the hub's
/// `/v1/chat/completions`. `llm-hub copilot` stays the upstream login command.
#[derive(Debug)]
pub struct CopilotCliLauncher;

impl CopilotCliLauncher {
    const PROGRAM: &str = "copilot";
    const PROVIDER_TYPE: &str = "openai";

    /// The copilot command, sized to the model's catalog limits when it has them.
    #[must_use]
    pub fn spec(endpoint: &HubEndpoint, args: LaunchArgs, catalog: &[HubModel]) -> LaunchSpec {
        let mut spec = LaunchSpec::new(Self::PROGRAM)
            .env("COPILOT_PROVIDER_BASE_URL", endpoint.v1_url())
            .env("COPILOT_PROVIDER_TYPE", Self::PROVIDER_TYPE)
            .env("COPILOT_PROVIDER_API_KEY", endpoint.api_key());
        let model = args.model().unwrap_or("copilot/claude-opus-5-5");
        spec = spec.env("COPILOT_MODEL", model);
        if let Some(known) = HubModel::find(catalog, model) {
            spec = Self::with_limits(spec, known);
        }
        spec.args(args.into_passthrough())
    }

    fn with_limits(mut spec: LaunchSpec, model: &HubModel) -> LaunchSpec {
        if let (Some(prompt), Some(output)) = (model.max_prompt_tokens(), model.max_output_tokens())
        {
            spec = spec
                .env("COPILOT_PROVIDER_MAX_PROMPT_TOKENS", prompt.to_string())
                .env("COPILOT_PROVIDER_MAX_OUTPUT_TOKENS", output.to_string());
        }
        spec
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn endpoint() -> HubEndpoint {
        HubEndpoint::from_lookup(|name| (name == "LLM_HUB_API_KEY").then(|| "k".to_string()))
    }

    #[test]
    fn without_a_model_only_the_provider_is_set() {
        let spec = CopilotCliLauncher::spec(&endpoint(), LaunchArgs::from_strs(&["--yolo"]), &[]);

        assert_eq!(spec.program(), "copilot");
        assert_eq!(spec.env_value("COPILOT_MODEL"), Some("copilot/claude-opus-5-5"));
        assert_eq!(
            spec.env_names(),
            [
                "COPILOT_PROVIDER_BASE_URL",
                "COPILOT_PROVIDER_TYPE",
                "COPILOT_PROVIDER_API_KEY",
                "COPILOT_MODEL"
            ]
        );
        assert_eq!(
            spec.env_value("COPILOT_PROVIDER_BASE_URL"),
            Some("http://localhost:8888/v1")
        );
        assert_eq!(spec.env_value("COPILOT_PROVIDER_TYPE"), Some("openai"));
        assert_eq!(spec.env_value("COPILOT_PROVIDER_API_KEY"), Some("k"));
        assert_eq!(spec.argv(), ["--yolo"]);
    }

    #[test]
    fn known_model_gets_prompt_and_output_budgets() {
        let spec = CopilotCliLauncher::spec(&endpoint(), LaunchArgs::from_strs(&["other"]), &[]);

        assert_eq!(spec.env_value("COPILOT_MODEL"), Some("other"));
        assert_eq!(spec.env_value("COPILOT_PROVIDER_MAX_PROMPT_TOKENS"), None);
    }
}
