use serde_json::{Map, Value, json};

use super::hub_endpoint::HubEndpoint;
use super::hub_model::HubModel;
use super::launch_args::LaunchArgs;
use super::launch_spec::LaunchSpec;

/// `llm-hub opencode`: opencode on the hub's `/v1/chat/completions`. The
/// provider arrives in `OPENCODE_CONFIG_CONTENT`, which opencode merges over
/// the user's own config.
#[derive(Debug)]
pub struct OpencodeLauncher;

impl OpencodeLauncher {
    const PROGRAM: &str = "opencode";
    const PROVIDER_ID: &str = "llm-hub";
    const CONFIG_VAR: &str = "OPENCODE_CONFIG_CONTENT";

    #[must_use]
    pub fn spec(endpoint: &HubEndpoint, args: LaunchArgs, catalog: &[HubModel]) -> LaunchSpec {
        let config = Self::config(endpoint, args.model(), catalog);
        LaunchSpec::new(Self::PROGRAM)
            .env(HubEndpoint::api_key_var(), endpoint.api_key())
            .env(Self::CONFIG_VAR, config.to_string())
            .args(args.into_passthrough())
    }

    fn config(endpoint: &HubEndpoint, model: Option<&str>, catalog: &[HubModel]) -> Value {
        let models: Map<String, Value> = catalog
            .iter()
            .map(|entry| (entry.id().to_string(), Self::model_entry(entry)))
            .collect();
        let mut config = json!({
            "provider": {
                Self::PROVIDER_ID: {
                    "npm": "@ai-sdk/openai-compatible",
                    "name": Self::PROVIDER_ID,
                    "options": {
                        "baseURL": endpoint.v1_url(),
                        "apiKey": format!("{{env:{}}}", HubEndpoint::api_key_var()),
                    },
                    "models": models,
                }
            }
        });
        if let Some(model) = model {
            config["model"] = json!(format!("{}/{model}", Self::PROVIDER_ID));
        }
        config
    }

    fn model_entry(model: &HubModel) -> Value {
        match (model.context_window(), model.max_output_tokens()) {
            (Some(context), Some(output)) => {
                json!({ "limit": { "context": context, "output": output } })
            }
            _ => json!({}),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn args(values: &[&str]) -> LaunchArgs {
        LaunchArgs::parse(values.iter().map(ToString::to_string).collect())
    }

    fn config_of(spec: &LaunchSpec) -> Value {
        serde_json::from_str(spec.env_value("OPENCODE_CONFIG_CONTENT").unwrap()).unwrap()
    }

    #[test]
    fn provider_lists_the_catalog_with_known_limits() {
        let catalog = [
            HubModel::new("llama_swap/glm-4.7-flash", Some(65_536)),
            HubModel::new("copilot/auto", None),
        ];

        let spec = OpencodeLauncher::spec(
            &HubEndpoint::from_lookup(|_| None),
            args(&["run", "hi"]),
            &catalog,
        );

        assert_eq!(spec.program(), "opencode");
        assert_eq!(spec.env_value("LLM_HUB_API_KEY"), Some("llm-hub"));
        assert_eq!(
            config_of(&spec),
            json!({
                "provider": {"llm-hub": {
                    "npm": "@ai-sdk/openai-compatible",
                    "name": "llm-hub",
                    "options": {
                        "baseURL": "http://localhost:8888/v1",
                        "apiKey": "{env:LLM_HUB_API_KEY}",
                    },
                    "models": {
                        "llama_swap/glm-4.7-flash": {"limit": {"context": 65_536, "output": 16_384}},
                        "copilot/auto": {},
                    },
                }},
                "model": "llm-hub/run",
            })
        );
        assert_eq!(spec.argv(), ["hi"]);
    }

    #[test]
    fn model_selects_the_hub_provider_entry() {
        let spec = OpencodeLauncher::spec(
            &HubEndpoint::from_lookup(|_| None),
            args(&["copilot/auto", "--continue"]),
            &[],
        );

        assert_eq!(config_of(&spec)["model"], "llm-hub/copilot/auto");
        assert_eq!(spec.argv(), ["--continue"]);
    }

    #[test]
    fn without_a_model_the_user_default_model_stays() {
        let spec = OpencodeLauncher::spec(
            &HubEndpoint::from_lookup(|_| None),
            args(&["--continue"]),
            &[],
        );

        assert!(config_of(&spec).get("model").is_none());
    }
}
