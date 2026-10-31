use super::hub_endpoint::HubEndpoint;
use super::hub_model::HubModel;
use super::launch_args::LaunchArgs;
use super::launch_spec::LaunchSpec;

/// `llm-hub codex`: Codex CLI on the hub's `/v1/responses`, through `-c`
/// overrides, so `~/.codex/config.toml` stays untouched.
#[derive(Debug)]
pub struct CodexLauncher;

impl CodexLauncher {
    const PROGRAM: &str = "codex";
    const PROVIDER_ID: &str = "llm_hub";
    const PROVIDER_NAME: &str = "llm-hub";

    #[must_use]
    pub fn spec(endpoint: &HubEndpoint, args: LaunchArgs, catalog: &[HubModel]) -> LaunchSpec {
        let provider = format!(
            "model_providers.{}={{name={},base_url={},env_key={},wire_api=\"responses\"}}",
            Self::PROVIDER_ID,
            Self::toml_string(Self::PROVIDER_NAME),
            Self::toml_string(&endpoint.v1_url()),
            Self::toml_string(HubEndpoint::api_key_var()),
        );
        let mut spec = LaunchSpec::new(Self::PROGRAM)
            .env(HubEndpoint::api_key_var(), endpoint.api_key())
            .arg("-c")
            .arg(provider)
            .arg("-c")
            .arg(format!(
                "model_provider={}",
                Self::toml_string(Self::PROVIDER_ID)
            ));
        if let Some(model) = args.model() {
            spec = spec
                .arg("-c")
                .arg(format!("model={}", Self::toml_string(model)));
            if let Some(known) = HubModel::find(catalog, model) {
                spec = Self::with_limits(spec, known);
            }
        }
        spec.args(args.into_passthrough())
    }

    fn with_limits(mut spec: LaunchSpec, model: &HubModel) -> LaunchSpec {
        if let Some(window) = model.context_window() {
            spec = spec.arg("-c").arg(format!("model_context_window={window}"));
        }
        if let Some(output) = model.max_output_tokens() {
            spec = spec
                .arg("-c")
                .arg(format!("model_max_output_tokens={output}"));
        }
        spec
    }

    /// A JSON string literal is also a valid TOML basic string.
    fn toml_string(value: &str) -> String {
        serde_json::Value::from(value).to_string()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn endpoint() -> HubEndpoint {
        HubEndpoint::from_lookup(|name| {
            (name == "LLM_HUB_URL").then(|| "http://hub:8410".to_string())
        })
    }

    fn args(values: &[&str]) -> LaunchArgs {
        LaunchArgs::parse(values.iter().map(ToString::to_string).collect())
    }

    const PROVIDER: &str = "model_providers.llm_hub={name=\"llm-hub\",base_url=\"http://hub:8410/v1\",env_key=\"LLM_HUB_API_KEY\",wire_api=\"responses\"}";

    #[test]
    fn without_a_model_only_the_provider_is_selected() {
        let spec = CodexLauncher::spec(&endpoint(), args(&["--full-auto"]), &[]);

        assert_eq!(spec.program(), "codex");
        assert_eq!(spec.env_value("LLM_HUB_API_KEY"), Some("llm-hub"));
        assert_eq!(
            spec.argv(),
            [
                "-c",
                PROVIDER,
                "-c",
                "model_provider=\"llm_hub\"",
                "--full-auto"
            ]
        );
    }

    #[test]
    fn known_model_gets_its_window_and_output_budget() {
        let catalog = [HubModel::new("llama_swap/glm-4.7-flash", Some(65_536))];

        let spec = CodexLauncher::spec(
            &endpoint(),
            args(&["llama_swap/glm-4.7-flash", "exec", "hi"]),
            &catalog,
        );

        assert_eq!(
            &spec.argv()[4..],
            [
                "-c",
                "model=\"llama_swap/glm-4.7-flash\"",
                "-c",
                "model_context_window=65536",
                "-c",
                "model_max_output_tokens=16384",
                "exec",
                "hi",
            ]
        );
    }

    #[test]
    fn unknown_model_keeps_codex_default_limits() {
        let spec = CodexLauncher::spec(&endpoint(), args(&["other"]), &[]);

        assert_eq!(&spec.argv()[4..], ["-c", "model=\"other\""]);
    }

    #[test]
    fn quotes_in_the_model_id_stay_inside_the_toml_string() {
        let spec = CodexLauncher::spec(&endpoint(), args(&["a\"b"]), &[]);

        assert_eq!(spec.argv()[5], "model=\"a\\\"b\"");
    }
}
