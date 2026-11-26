use serde_json::{Value, json};

use super::config_paths::ConfigPaths;
use super::json_config::JsonConfig;
use super::setup_plan::{SetupOutcome, SetupPlan};
use crate::schemas::launch::hub_model::HubModel;
use crate::services::launch::hub_endpoint::HubEndpoint;

/// `llm-hub setup pi`: the `llm-hub` provider in `~/.pi/agent/models.json`, with the same
/// settings the launcher's extension registers.
#[derive(Debug)]
pub struct PiSetup;

impl PiSetup {
    const FILE: &str = "models.json";
    const PROVIDER_ID: &str = "llm-hub";
    const DEFAULT_CONTEXT_WINDOW: u64 = 131_072;
    const MAX_OUTPUT_TOKENS: u64 = 32_768;

    /// # Errors
    /// Returns a message when the catalog is empty, no config dir is known, or the existing file
    /// cannot be read or parsed.
    pub fn plan(
        lookup: impl Fn(&str) -> Option<String>,
        endpoint: &HubEndpoint,
        catalog: &[HubModel],
    ) -> Result<SetupOutcome, String> {
        if catalog.is_empty() {
            return Err("the hub lists no models, so there is nothing to configure".to_string());
        }
        let path = ConfigPaths::dir(lookup, "PI_CODING_AGENT_DIR", ".pi/agent")?.join(Self::FILE);
        let existing = SetupPlan::read_existing(&path)?;
        let content = Self::render(existing.as_deref(), endpoint, catalog)?;
        Ok(SetupOutcome::Write(SetupPlan { path, content }))
    }

    fn render(
        existing: Option<&str>,
        endpoint: &HubEndpoint,
        catalog: &[HubModel],
    ) -> Result<String, String> {
        let mut root = JsonConfig::parse_object(existing, Self::FILE)?;
        let providers = JsonConfig::object_entry(&mut root, "providers")?;
        providers.insert(
            Self::PROVIDER_ID.to_string(),
            Self::provider(endpoint, catalog),
        );
        JsonConfig::render(root)
    }

    fn provider(endpoint: &HubEndpoint, catalog: &[HubModel]) -> Value {
        let models: Vec<Value> = catalog.iter().map(Self::model_entry).collect();
        json!({
            "baseUrl": endpoint.v1_url(),
            "apiKey": endpoint.api_key(),
            "api": "openai-completions",
            "compat": {
                "supportsDeveloperRole": false,
                "supportsReasoningEffort": false,
                "maxTokensField": "max_tokens",
            },
            "models": models,
        })
    }

    fn model_entry(model: &HubModel) -> Value {
        let window = model
            .context_window()
            .unwrap_or(Self::DEFAULT_CONTEXT_WINDOW);
        json!({
            "id": model.id(),
            "name": model.id(),
            "reasoning": false,
            "input": ["text"],
            "cost": {"input": 0, "output": 0, "cacheRead": 0, "cacheWrite": 0},
            "contextWindow": window,
            "maxTokens": Self::MAX_OUTPUT_TOKENS.min(window / 4),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn endpoint() -> HubEndpoint {
        HubEndpoint::from_lookup(|_| None)
    }

    fn render(existing: Option<&str>, catalog: &[HubModel]) -> Value {
        serde_json::from_str(&PiSetup::render(existing, &endpoint(), catalog).unwrap()).unwrap()
    }

    #[test]
    fn the_provider_lists_the_catalog_with_the_extension_limits() {
        let catalog = [
            HubModel::new("llama_swap/small", Some(8_192)),
            HubModel::new("copilot/auto", None),
        ];

        let config = render(None, &catalog);

        let provider = &config["providers"]["llm-hub"];
        assert_eq!(provider["baseUrl"], "http://localhost:8888/v1");
        assert_eq!(provider["apiKey"], "llm-hub");
        assert_eq!(provider["api"], "openai-completions");
        assert_eq!(provider["models"][0]["id"], "llama_swap/small");
        assert_eq!(provider["models"][0]["contextWindow"], 8_192);
        assert_eq!(provider["models"][0]["maxTokens"], 2_048);
        assert_eq!(provider["models"][1]["contextWindow"], 131_072);
        assert_eq!(provider["models"][1]["maxTokens"], 32_768);
    }

    #[test]
    fn other_providers_and_keys_stay() {
        let catalog = [HubModel::new("copilot/auto", None)];

        let config = render(
            Some(r#"{"extra": 1, "providers": {"mine": {"baseUrl": "x"}, "llm-hub": {"old": 1}}}"#),
            &catalog,
        );

        assert_eq!(config["extra"], 1);
        assert_eq!(config["providers"]["mine"], json!({"baseUrl": "x"}));
        assert!(config["providers"]["llm-hub"].get("old").is_none());
    }

    #[test]
    fn a_rerun_gives_the_same_file() {
        let catalog = [HubModel::new("copilot/auto", None)];
        let first = PiSetup::render(None, &endpoint(), &catalog).unwrap();

        let second = PiSetup::render(Some(&first), &endpoint(), &catalog).unwrap();

        assert_eq!(first, second);
    }

    #[test]
    fn an_empty_catalog_is_an_error() {
        assert!(PiSetup::plan(|_| Some("/x".to_string()), &endpoint(), &[]).is_err());
    }

    #[test]
    fn plan_uses_the_pi_agent_dir_override() {
        let catalog = [HubModel::new("copilot/auto", None)];

        let outcome = PiSetup::plan(
            |name| (name == "PI_CODING_AGENT_DIR").then(|| "/nonexistent-pi".to_string()),
            &endpoint(),
            &catalog,
        )
        .unwrap();

        let SetupOutcome::Write(plan) = outcome else {
            panic!("expected a file");
        };
        assert_eq!(
            plan.path,
            std::path::PathBuf::from("/nonexistent-pi/models.json")
        );
    }
}
