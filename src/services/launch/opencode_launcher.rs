use serde_json::{Map, Value, json};

use super::hub_endpoint::HubEndpoint;
use crate::schemas::launch::hub_model::HubModel;
use crate::schemas::launch::launch_args::LaunchArgs;
use crate::schemas::launch::launch_spec::LaunchSpec;
use crate::schemas::launch::profile_filter::ProfileFilter;
use crate::schemas::launch::smart_auto_flags::SmartAutoFlags;

/// `llm-hub opencode`: opencode on the hub's `/v1/chat/completions`, with the
/// provider in `OPENCODE_CONFIG_CONTENT`, merged over the user's config. Only the
/// `llm-hub` provider stays enabled, so the model picker lists hub models alone.
#[derive(Debug)]
pub struct OpencodeLauncher;

impl OpencodeLauncher {
    const PROGRAM: &str = "opencode";
    const PROVIDER_ID: &str = "llm-hub";
    const CONFIG_VAR: &str = "OPENCODE_CONFIG_CONTENT";
    /// opencode 2 serves every TUI from one background service, which never reads this
    /// process's `OPENCODE_CONFIG_CONTENT`; a private server does.
    const STANDALONE_FLAG: &str = "--standalone";
    const SERVER_FLAG: &str = "--server";

    /// The opencode command, with every catalog model in the `llm-hub` provider, labelled and
    /// priced as the hub lists it. Profile and Smart Auto flags become provider headers.
    #[must_use]
    pub fn spec(
        endpoint: &HubEndpoint,
        args: LaunchArgs,
        catalog: &[HubModel],
        profiles: &ProfileFilter,
        smart_auto: &SmartAutoFlags,
    ) -> LaunchSpec {
        let headers = Self::headers(profiles, smart_auto);
        let config = Self::config(endpoint, args.model(), catalog, headers);
        let passthrough = args.into_passthrough();
        let mut spec = LaunchSpec::new(Self::PROGRAM)
            .env(HubEndpoint::api_key_var(), endpoint.api_key())
            .env(Self::CONFIG_VAR, config.to_string());
        if !Self::names_a_server(&passthrough) {
            spec = spec.arg(Self::STANDALONE_FLAG);
        }
        spec.args(passthrough)
    }

    fn names_a_server(args: &[String]) -> bool {
        args.iter().any(|arg| {
            arg == Self::STANDALONE_FLAG
                || arg == Self::SERVER_FLAG
                || arg.starts_with(&format!("{}=", Self::SERVER_FLAG))
        })
    }

    fn headers(profiles: &ProfileFilter, smart_auto: &SmartAutoFlags) -> Map<String, Value> {
        let profile_header =
            (!profiles.is_empty()).then(|| (ProfileFilter::HEADER, profiles.to_list()));
        profile_header
            .into_iter()
            .chain(smart_auto.headers())
            .map(|(name, value)| (name.to_string(), json!(value)))
            .collect()
    }

    fn config(
        endpoint: &HubEndpoint,
        model: Option<&str>,
        catalog: &[HubModel],
        headers: Map<String, Value>,
    ) -> Value {
        let models: Map<String, Value> = catalog
            .iter()
            .map(|entry| (entry.id().to_string(), Self::model_entry(entry)))
            .collect();
        let mut options = json!({
            "baseURL": endpoint.v1_url(),
            "apiKey": format!("{{env:{}}}", HubEndpoint::api_key_var()),
        });
        if !headers.is_empty() {
            options["headers"] = Value::Object(headers);
        }
        let mut config = json!({
            "enabled_providers": [Self::PROVIDER_ID],
            "provider": {
                Self::PROVIDER_ID: {
                    "npm": "@ai-sdk/openai-compatible",
                    "name": Self::PROVIDER_ID,
                    "options": options,
                    "models": models,
                }
            }
        });
        if let Some(model) = model {
            let qualified = format!("{}/{model}", Self::PROVIDER_ID);
            config["model"] = json!(qualified);
            config["small_model"] = json!(qualified);
        }
        config
    }

    fn model_entry(model: &HubModel) -> Value {
        let mut entry = json!({});
        if let Some(label) = model.picker_label() {
            entry["name"] = json!(label);
        }
        if let (Some(context), Some(output)) = (model.context_window(), model.max_output_tokens()) {
            entry["limit"] = json!({ "context": context, "output": output });
        }
        if let Some(price) = model.price() {
            entry["cost"] = price.to_opencode_cost();
        }
        if model.takes_attachments() {
            entry["attachment"] = json!(true);
        }
        entry
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::schemas::launch::hub_model_price::HubModelPrice;

    fn config_of(spec: &LaunchSpec) -> Value {
        serde_json::from_str(spec.env_value("OPENCODE_CONFIG_CONTENT").unwrap()).unwrap()
    }

    fn spec_for(args: &[&str], catalog: &[HubModel]) -> LaunchSpec {
        OpencodeLauncher::spec(
            &HubEndpoint::from_lookup(|_| None),
            LaunchArgs::from_strs(args),
            catalog,
            &ProfileFilter::default(),
            &SmartAutoFlags::default(),
        )
    }

    fn smart_auto(flags: &[&str]) -> SmartAutoFlags {
        let mut args: Vec<String> = flags.iter().map(ToString::to_string).collect();
        let mut parsed = SmartAutoFlags::default();
        parsed.take_flags(&mut args).unwrap();
        parsed
    }

    #[test]
    fn provider_lists_the_catalog_with_known_limits() {
        let catalog = [
            HubModel::new("llama_swap/glm-4.7-flash", Some(65_536)),
            HubModel::new("copilot/auto", None),
        ];

        let spec = spec_for(&["run", "hi"], &catalog);

        assert_eq!(spec.program(), "opencode");
        assert_eq!(spec.env_value("LLM_HUB_API_KEY"), Some("llm-hub"));
        assert_eq!(
            config_of(&spec),
            json!({
                "enabled_providers": ["llm-hub"],
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
                "small_model": "llm-hub/run",
            })
        );
        assert_eq!(spec.argv(), ["--standalone", "hi"]);
    }

    #[test]
    fn models_carry_the_hub_picker_label_price_and_attachments() {
        let catalog = [
            HubModel::new("hub/smart-auto-balance", None)
                .with_display_name("Smart Auto (Balance)", true),
            HubModel::new("copilot/claude-opus-5-5", None)
                .with_display_name("claude-opus-5-5 · copilot · $4/$20", false)
                .with_vision()
                .with_price(HubModelPrice::new(4.0, 20.0)),
        ];

        let models = config_of(&spec_for(&[], &catalog))["provider"]["llm-hub"]["models"].clone();

        assert_eq!(
            models,
            json!({
                "hub/smart-auto-balance": {"name": "Smart Auto (Balance) (recommended)"},
                "copilot/claude-opus-5-5": {
                    "name": "claude-opus-5-5 · copilot · $4/$20",
                    "cost": {"input": 4.0, "output": 20.0},
                    "attachment": true,
                },
            })
        );
    }

    #[test]
    fn profiles_and_smart_auto_flags_become_provider_headers() {
        let spec = OpencodeLauncher::spec(
            &HubEndpoint::from_lookup(|_| None),
            LaunchArgs::from_strs(&["hub/smart-auto-intelligence"]),
            &[],
            &ProfileFilter::from_list("copilot,ivrit"),
            &smart_auto(&[
                "--smart-auto-tier",
                "intelligence",
                "--smart-auto-role",
                "planning",
            ]),
        );

        assert_eq!(
            config_of(&spec)["provider"]["llm-hub"]["options"]["headers"],
            json!({
                "x-llm-hub-profiles": "copilot,ivrit",
                "x-llm-hub-smart-auto-tier": "intelligence",
                "x-llm-hub-smart-auto-role": "planning",
            })
        );
    }

    #[test]
    fn repeated_include_flags_send_one_merged_header() {
        let spec = OpencodeLauncher::spec(
            &HubEndpoint::from_lookup(|_| None),
            LaunchArgs::from_strs(&[]),
            &[],
            &ProfileFilter::default(),
            &smart_auto(&[
                "--smart-auto-include",
                "copilot/*",
                "--smart-auto-include",
                "openai/*",
            ]),
        );

        assert_eq!(
            config_of(&spec)["provider"]["llm-hub"]["options"]["headers"],
            json!({"x-llm-hub-smart-auto-include": "copilot/*,openai/*"})
        );
    }

    #[test]
    fn model_selects_the_hub_provider_entry() {
        let spec = spec_for(&["copilot/auto", "--continue"], &[]);

        assert_eq!(config_of(&spec)["model"], "llm-hub/copilot/auto");
        assert_eq!(spec.argv(), ["--standalone", "--continue"]);
    }

    #[test]
    fn a_named_server_replaces_the_private_server() {
        assert_eq!(
            spec_for(&["m", "--server", "http://h:1"], &[]).argv(),
            ["--server", "http://h:1"]
        );
        assert_eq!(
            spec_for(&["m", "--server=http://h:1"], &[]).argv(),
            ["--server=http://h:1"]
        );
        assert_eq!(
            spec_for(&["m", "--standalone"], &[]).argv(),
            ["--standalone"]
        );
    }

    #[test]
    fn without_a_model_the_user_default_model_stays() {
        let spec = spec_for(&["--continue"], &[]);

        assert!(config_of(&spec).get("model").is_none());
    }
}
