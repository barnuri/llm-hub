use super::setup_plan::SetupOutcome;
use crate::services::launch::hub_endpoint::HubEndpoint;

/// `llm-hub setup copilot-cli`: Copilot CLI takes its BYOK provider from environment variables
/// only, so no file can hold it. The command prints the lines for a shell profile.
#[derive(Debug)]
pub struct CopilotCliSetup;

impl CopilotCliSetup {
    const DEFAULT_MODEL: &str = "copilot/claude-opus-5-5";

    #[must_use]
    pub fn outcome(endpoint: &HubEndpoint) -> SetupOutcome {
        SetupOutcome::Manual(format!(
            "GitHub Copilot CLI reads its provider only from the environment, so nothing was written.\nAdd these lines to your shell profile:\nexport COPILOT_PROVIDER_BASE_URL={url}\nexport COPILOT_PROVIDER_TYPE=openai\nexport COPILOT_PROVIDER_API_KEY={key}\nexport COPILOT_MODEL={model}",
            url = endpoint.v1_url(),
            key = endpoint.api_key(),
            model = Self::DEFAULT_MODEL,
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_outcome_lists_the_environment_the_launcher_sets() {
        let outcome = CopilotCliSetup::outcome(&HubEndpoint::from_lookup(|_| None));

        let SetupOutcome::Manual(text) = outcome else {
            panic!("expected instructions");
        };
        assert!(text.contains("export COPILOT_PROVIDER_BASE_URL=http://localhost:8888/v1\n"));
        assert!(text.contains("export COPILOT_PROVIDER_TYPE=openai\n"));
        assert!(text.contains("export COPILOT_PROVIDER_API_KEY=llm-hub\n"));
        assert!(text.ends_with("export COPILOT_MODEL=copilot/claude-opus-5-5"));
    }
}
