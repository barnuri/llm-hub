use std::path::PathBuf;

use serde_json::{Map, Value, json};

use super::hub_endpoint::HubEndpoint;
use crate::configs::OpencodeContextCaps;
use crate::schemas::launch::hub_model::HubModel;
use crate::schemas::launch::launch_args::LaunchArgs;
use crate::schemas::launch::launch_spec::LaunchSpec;
use crate::schemas::launch::profile_filter::ProfileFilter;
use crate::schemas::launch::smart_auto_flags::SmartAutoFlags;
use crate::schemas::smart_auto_headers::SmartAutoHeaders;

/// `llm-hub opencode`: opencode on the hub's `/v1/chat/completions`, with the
/// provider in `OPENCODE_CONFIG_CONTENT`, merged over the user's config. Only the
/// `llm-hub` provider stays enabled, so the model picker lists hub models alone.
#[derive(Debug)]
pub struct OpencodeLauncher;

impl OpencodeLauncher {
    pub(super) const PROGRAM: &str = "opencode";
    /// Names the opencode binary; tests point it at a fake.
    pub(super) const BIN_VAR: &str = "LLM_HUB_OPENCODE_BIN";
    const PROVIDER_ID: &str = "llm-hub";
    const CONFIG_VAR: &str = "OPENCODE_CONFIG_CONTENT";
    const SMALL_MODEL: &str = "llm-hub/hub/smart-auto-fast";
    /// opencode 2 serves every TUI from one background service, which never reads this
    /// process's `OPENCODE_CONFIG_CONTENT`; a private server does.
    const STANDALONE_FLAG: &str = "--standalone";
    const SERVER_FLAG: &str = "--server";
    const PLUGIN_SOURCE: &str = include_str!("opencode_session.ts");
    const PLUGIN_FILE: &str = "llm-hub-session.ts";

    /// The opencode command, with every catalog model in the `llm-hub` provider, labelled and
    /// priced as the hub lists it, its limits lowered by `caps`. Profile and Smart Auto flags
    /// become provider headers.
    #[must_use]
    pub fn spec(
        endpoint: &HubEndpoint,
        args: LaunchArgs,
        catalog: &[HubModel],
        profiles: &ProfileFilter,
        smart_auto: &SmartAutoFlags,
        caps: &OpencodeContextCaps,
    ) -> LaunchSpec {
        let headers = Self::headers(profiles, smart_auto);
        let config = Self::config(endpoint, args.model(), catalog, headers, caps);
        let passthrough = args.into_passthrough();
        let mut spec = LaunchSpec::new(Self::PROGRAM)
            .env(HubEndpoint::api_key_var(), endpoint.api_key())
            .env(Self::CONFIG_VAR, config.to_string());
        if !Self::names_a_server(&passthrough) {
            spec = spec.arg(Self::STANDALONE_FLAG);
        }
        spec.args(passthrough)
    }

    /// The config `llm-hub setup opencode` stores in the user's opencode config: the `llm-hub`
    /// provider with the catalog and the literal hub key, as the shared service never reads
    /// `OPENCODE_CONFIG_CONTENT`. Smart Auto Fast is the small model.
    #[must_use]
    pub fn persistent_config(
        endpoint: &HubEndpoint,
        catalog: &[HubModel],
        caps: &OpencodeContextCaps,
    ) -> Value {
        let mut config = Self::config(endpoint, None, catalog, Map::new(), caps);
        config["provider"][Self::PROVIDER_ID]["options"]["apiKey"] = json!(endpoint.api_key());
        config["small_model"] = json!(Self::SMALL_MODEL);
        config
    }

    /// Plain `opencode -m <provider>/<model>` on the shared service, which already serves a provider
    /// for this hub (`shared_provider`). `None` keeps the private-server path: no such provider, a
    /// server the user named, or a flag only a provider header carries (profiles and every Smart
    /// Auto flag but the tier, which the `hub/smart-auto-<tier>` model id carries).
    #[must_use]
    pub fn shared_spec(
        shared_provider: Option<&str>,
        args: &LaunchArgs,
        profiles: &ProfileFilter,
        smart_auto: &SmartAutoFlags,
    ) -> Option<LaunchSpec> {
        let provider = shared_provider?;
        let model = args.model()?;
        let tier_only = smart_auto
            .headers()
            .iter()
            .all(|(name, _)| *name == SmartAutoHeaders::TIER);
        if !profiles.is_empty() || !tier_only || Self::names_a_server(args.passthrough()) {
            return None;
        }
        Some(
            LaunchSpec::new(Self::PROGRAM)
                .arg("-m")
                .arg(format!("{provider}/{model}"))
                .args(args.passthrough().iter().cloned()),
        )
    }

    fn names_a_server(args: &[String]) -> bool {
        args.iter().any(|arg| {
            arg == Self::STANDALONE_FLAG
                || arg == Self::SERVER_FLAG
                || arg.starts_with(&format!("{}=", Self::SERVER_FLAG))
        })
    }

    /// Writes the `OpenCode` session header plugin into the user config directory.
    ///
    /// # Errors
    /// Returns a message when no config directory is known or the plugin cannot be written.
    pub fn write_plugin(lookup: impl Fn(&str) -> Option<String>) -> Result<PathBuf, String> {
        let dir = Self::config_dir(lookup)
            .ok_or("cannot find an OpenCode config dir: set HOME or XDG_CONFIG_HOME")?
            .join("plugins");
        std::fs::create_dir_all(&dir)
            .map_err(|e| format!("cannot create {}: {e}", dir.display()))?;
        let path = dir.join(Self::PLUGIN_FILE);
        std::fs::write(&path, Self::PLUGIN_SOURCE)
            .map_err(|e| format!("cannot write {}: {e}", path.display()))?;
        Ok(path)
    }

    pub(crate) fn config_dir(lookup: impl Fn(&str) -> Option<String>) -> Option<PathBuf> {
        let read = |name: &str| lookup(name).filter(|value| !value.is_empty());
        if let Some(xdg) = read("XDG_CONFIG_HOME") {
            return Some(PathBuf::from(xdg).join("opencode"));
        }
        if cfg!(windows)
            && let Some(app_data) = read("APPDATA")
        {
            return Some(PathBuf::from(app_data).join("opencode"));
        }
        read("HOME").map(|home| PathBuf::from(home).join(".config/opencode"))
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
        caps: &OpencodeContextCaps,
    ) -> Value {
        let models: Map<String, Value> = catalog
            .iter()
            .map(|entry| (entry.id().to_string(), Self::model_entry(entry, caps)))
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

    fn model_entry(model: &HubModel, caps: &OpencodeContextCaps) -> Value {
        let mut entry = json!({});
        if let Some(label) = model.picker_label() {
            entry["name"] = json!(label);
        }
        if let (Some(context), Some(output)) = (model.context_window(), model.max_output_tokens()) {
            let (context, output) = caps.limits(model.id(), context, output);
            entry["limit"] = json!({ "context": context, "output": output });
        }
        // opencode counts a model with a zero input rate as free and sorts it first, or into a
        // "Free" group. Such a model gets no cost, so it keeps its place in the hub order. Its
        // name still shows the hub price.
        if let Some(price) = model.price().filter(|price| !price.has_free_input()) {
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
            &OpencodeContextCaps::default(),
        )
    }

    #[test]
    fn plugin_is_written_under_the_xdg_config_dir() {
        let config = tempfile::tempdir().unwrap();
        let config_path = config.path().to_string_lossy().to_string();

        let path = OpencodeLauncher::write_plugin(|name| {
            (name == "XDG_CONFIG_HOME").then(|| config_path.clone())
        })
        .unwrap();

        assert_eq!(
            path,
            config.path().join("opencode/plugins/llm-hub-session.ts")
        );
        let written = std::fs::read_to_string(&path).unwrap();
        assert!(written.contains("x-llm-hub-session-id"));
        assert!(written.contains("event.sessionID"));
    }

    #[test]
    fn plugin_uses_the_opencode_home_config_as_a_fallback() {
        let dir =
            OpencodeLauncher::config_dir(|name| (name == "HOME").then(|| "/home/u".to_string()));

        assert_eq!(dir, Some(PathBuf::from("/home/u/.config/opencode")));
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
    fn a_1m_model_lists_its_bare_id_at_200k_and_its_suffixed_id_at_1m() {
        let catalog = [
            HubModel::new("copilot/claude-sonnet-5-5", Some(200_000))
                .with_max_output_tokens(64_000),
            HubModel::new("copilot/claude-sonnet-5-5[1m]", Some(1_000_000))
                .with_max_output_tokens(64_000),
        ];

        let spec = spec_for(&["copilot/claude-sonnet-5-5[1m]"], &catalog);
        let config = config_of(&spec);
        let models = &config["provider"]["llm-hub"]["models"];

        assert_eq!(
            models["copilot/claude-sonnet-5-5"]["limit"]["context"],
            200_000
        );
        assert_eq!(
            models["copilot/claude-sonnet-5-5[1m]"]["limit"]["context"],
            1_000_000
        );
    }

    #[test]
    fn a_context_cap_lowers_only_the_named_model_limit() {
        let catalog = [
            HubModel::new("copilot/claude-opus-5-5[1m]", Some(1_000_000))
                .with_max_output_tokens(128_000),
            HubModel::new("copilot/wide-output", Some(1_000_000)).with_max_output_tokens(128_000),
            HubModel::new("copilot/other", Some(1_000_000)),
            HubModel::new("local/small", Some(65_536)),
        ];
        let mut args: Vec<String> = [
            "--context-cap=copilot/claude-opus-5-5[1m]=200000,local/small=1000000",
            "--context-cap",
            "copilot/wide-output=100000",
        ]
        .iter()
        .map(ToString::to_string)
        .collect();
        let caps = OpencodeContextCaps::take_flags(&mut args).unwrap();

        let spec = OpencodeLauncher::spec(
            &HubEndpoint::from_lookup(|_| None),
            LaunchArgs::from_strs(&[]),
            &catalog,
            &ProfileFilter::default(),
            &SmartAutoFlags::default(),
            &caps,
        );
        let models = config_of(&spec)["provider"]["llm-hub"]["models"].clone();

        assert_eq!(
            models,
            json!({
                "copilot/claude-opus-5-5[1m]": {"limit": {"context": 200_000, "output": 128_000}},
                "copilot/wide-output": {"limit": {"context": 100_000, "output": 25_000}},
                "copilot/other": {"limit": {"context": 1_000_000, "output": 32_768}},
                "local/small": {"limit": {"context": 65_536, "output": 16_384}},
            })
        );
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
    fn models_keep_the_hub_catalog_order() {
        let catalog = [
            HubModel::new("hub/smart-auto-balance", None),
            HubModel::new("copilot/auto", None),
            HubModel::new("anthropic/a-model", None),
        ];

        let models = config_of(&spec_for(&[], &catalog))["provider"]["llm-hub"]["models"].clone();
        let ids: Vec<&String> = models.as_object().unwrap().keys().collect();

        assert_eq!(
            ids,
            [
                "hub/smart-auto-balance",
                "copilot/auto",
                "anthropic/a-model"
            ]
        );
    }

    #[test]
    fn a_model_with_free_input_gets_no_cost_so_it_stays_in_place() {
        let catalog = [
            HubModel::new("llama_swap/qwen", None)
                .with_display_name("qwen · llama-swap · free", false)
                .with_price(HubModelPrice::new(0.0, 0.0)),
            HubModel::new("llmgw/pegasus", None).with_price(HubModelPrice::new(0.0, 7.5)),
        ];

        let models = config_of(&spec_for(&[], &catalog))["provider"]["llm-hub"]["models"].clone();

        assert_eq!(
            models,
            json!({"llama_swap/qwen": {"name": "qwen · llama-swap · free"}, "llmgw/pegasus": {}})
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
            &OpencodeContextCaps::default(),
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
            &OpencodeContextCaps::default(),
        );

        assert_eq!(
            config_of(&spec)["provider"]["llm-hub"]["options"]["headers"],
            json!({"x-llm-hub-smart-auto-include": "copilot/*,openai/*"})
        );
    }

    #[test]
    fn persistent_config_holds_the_literal_key_and_the_small_model() {
        let catalog = [HubModel::new("copilot/auto", None)];

        let config = OpencodeLauncher::persistent_config(
            &HubEndpoint::from_lookup(|name| (name == "LLM_HUB_API_KEY").then(|| "k".to_string())),
            &catalog,
            &OpencodeContextCaps::default(),
        );

        assert_eq!(
            config,
            json!({
                "enabled_providers": ["llm-hub"],
                "provider": {"llm-hub": {
                    "npm": "@ai-sdk/openai-compatible",
                    "name": "llm-hub",
                    "options": {"baseURL": "http://localhost:8888/v1", "apiKey": "k"},
                    "models": {"copilot/auto": {}},
                }},
                "small_model": "llm-hub/hub/smart-auto-fast",
            })
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

    fn shared(
        provider: Option<&str>,
        args: &[&str],
        profiles: &ProfileFilter,
        flags: &SmartAutoFlags,
    ) -> Option<LaunchSpec> {
        OpencodeLauncher::shared_spec(provider, &LaunchArgs::from_strs(args), profiles, flags)
    }

    #[test]
    fn the_shared_service_runs_plain_opencode_with_a_qualified_model() {
        let spec = shared(
            Some("hub-provider"),
            &["hub/smart-auto-intelligence", "--continue"],
            &ProfileFilter::default(),
            &smart_auto(&["--smart-auto-tier", "intelligence"]),
        )
        .unwrap();

        assert_eq!(spec.program(), "opencode");
        assert_eq!(
            spec.argv(),
            [
                "-m",
                "hub-provider/hub/smart-auto-intelligence",
                "--continue"
            ]
        );
        assert!(spec.env_names().is_empty());
    }

    #[test]
    fn without_a_shared_provider_the_private_server_stays() {
        let none = shared(
            None,
            &["m"],
            &ProfileFilter::default(),
            &SmartAutoFlags::default(),
        );

        assert!(none.is_none());
    }

    #[test]
    fn flags_only_a_header_carries_keep_the_private_server() {
        let off = SmartAutoFlags::default();
        let profiles = ProfileFilter::from_list("copilot");
        let role = smart_auto(&["--smart-auto-role", "planning"]);

        assert!(shared(Some("p"), &["m"], &profiles, &off).is_none());
        assert!(shared(Some("p"), &["m"], &ProfileFilter::default(), &role).is_none());
    }

    #[test]
    fn a_named_server_or_no_model_keeps_the_private_server() {
        let (profiles, off) = (ProfileFilter::default(), SmartAutoFlags::default());

        assert!(shared(Some("p"), &["m", "--standalone"], &profiles, &off).is_none());
        assert!(shared(Some("p"), &["m", "--server", "http://h:1"], &profiles, &off).is_none());
        assert!(shared(Some("p"), &["--continue"], &profiles, &off).is_none());
    }
}
