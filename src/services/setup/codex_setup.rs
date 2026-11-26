use super::config_paths::ConfigPaths;
use super::setup_plan::{SetupOutcome, SetupPlan};
use crate::services::launch::hub_endpoint::HubEndpoint;

/// `llm-hub setup codex`: the `llm_hub` provider in `~/.codex/config.toml`. Two marked blocks
/// hold the settings, so a rerun replaces them and the rest of the file stays.
#[derive(Debug)]
pub struct CodexSetup;

impl CodexSetup {
    const FILE: &str = "config.toml";
    const BLOCK_START: &str = "# >>> llm-hub";
    const BLOCK_END: &str = "# <<< llm-hub";
    const SELECT_KEY: &str = "model_provider";

    /// # Errors
    /// Returns a message when no config dir is known or the existing file cannot be read.
    pub fn plan(
        lookup: impl Fn(&str) -> Option<String>,
        endpoint: &HubEndpoint,
    ) -> Result<SetupOutcome, String> {
        let path = ConfigPaths::dir(lookup, "CODEX_HOME", ".codex")?.join(Self::FILE);
        let existing = SetupPlan::read_existing(&path)?;
        let content = Self::render(existing.as_deref().unwrap_or_default(), endpoint);
        Ok(SetupOutcome::Write(SetupPlan { path, content }))
    }

    /// The selector block goes first, because a top-level key after a table would belong to
    /// that table. The provider table goes last.
    fn render(existing: &str, endpoint: &HubEndpoint) -> String {
        let rest = Self::without_hub_settings(existing);
        let head = format!(
            "{start} (written by `llm-hub setup codex`)\n{key} = \"llm_hub\"\n{end}\n",
            start = Self::BLOCK_START,
            key = Self::SELECT_KEY,
            end = Self::BLOCK_END,
        );
        let tail = format!(
            "{start} provider (written by `llm-hub setup codex`)\n[model_providers.llm_hub]\nname = \"llm-hub\"\nbase_url = {url}\nexperimental_bearer_token = {key}\nwire_api = \"responses\"\n{end} provider\n",
            start = Self::BLOCK_START,
            url = Self::toml_string(&endpoint.v1_url()),
            key = Self::toml_string(endpoint.api_key()),
            end = Self::BLOCK_END,
        );
        let middle = if rest.is_empty() {
            String::new()
        } else {
            format!("\n{rest}\n")
        };
        format!("{head}{middle}\n{tail}")
    }

    /// The file without the marked blocks and without a top-level `model_provider` key.
    fn without_hub_settings(text: &str) -> String {
        let mut kept: Vec<&str> = Vec::new();
        let mut in_block = false;
        let mut in_table = false;
        for line in text.lines() {
            let trimmed = line.trim_start();
            if trimmed.starts_with(Self::BLOCK_START) {
                in_block = true;
            }
            if in_block {
                in_block = !trimmed.starts_with(Self::BLOCK_END);
                continue;
            }
            if trimmed.starts_with('[') {
                in_table = true;
            }
            if !in_table && Self::is_select_key(trimmed) {
                continue;
            }
            kept.push(line);
        }
        kept.join("\n").trim().to_string()
    }

    fn is_select_key(line: &str) -> bool {
        line.strip_prefix(Self::SELECT_KEY)
            .is_some_and(|after| after.trim_start().starts_with('='))
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

    #[test]
    fn a_new_file_holds_the_selector_and_the_provider_table() {
        let text = CodexSetup::render("", &endpoint());

        assert_eq!(
            text,
            "# >>> llm-hub (written by `llm-hub setup codex`)\nmodel_provider = \"llm_hub\"\n# <<< llm-hub\n\n# >>> llm-hub provider (written by `llm-hub setup codex`)\n[model_providers.llm_hub]\nname = \"llm-hub\"\nbase_url = \"http://hub:8410/v1\"\nexperimental_bearer_token = \"llm-hub\"\nwire_api = \"responses\"\n# <<< llm-hub provider\n"
        );
    }

    #[test]
    fn the_rest_of_the_file_stays_between_the_blocks() {
        let existing = "model = \"gpt-5\"\n\n[projects.\"/x\"]\ntrust_level = \"trusted\"\n";

        let text = CodexSetup::render(existing, &endpoint());

        assert!(text.starts_with("# >>> llm-hub"));
        assert!(text.contains("# <<< llm-hub\n\nmodel = \"gpt-5\"\n\n[projects.\"/x\"]\ntrust_level = \"trusted\"\n\n# >>> llm-hub provider"));
    }

    #[test]
    fn a_top_level_model_provider_is_replaced_but_a_table_key_stays() {
        let existing = "model_provider = \"openai\"\nmodel_providers_x = 1\n\n[profiles.p]\nmodel_provider = \"keep\"\n";

        let text = CodexSetup::render(existing, &endpoint());

        assert!(!text.contains("\"openai\""));
        assert!(text.contains("model_providers_x = 1"));
        assert!(text.contains("[profiles.p]\nmodel_provider = \"keep\""));
        assert_eq!(text.matches("model_provider = \"llm_hub\"").count(), 1);
    }

    #[test]
    fn a_rerun_gives_the_same_file() {
        let first = CodexSetup::render("model = \"m\"\n", &endpoint());

        let second = CodexSetup::render(&first, &endpoint());

        assert_eq!(first, second);
    }

    #[test]
    fn a_rerun_picks_up_a_new_url() {
        let first = CodexSetup::render("", &endpoint());
        let other = HubEndpoint::from_lookup(|_| None);

        let second = CodexSetup::render(&first, &other);

        assert!(second.contains("http://localhost:8888/v1"));
        assert!(!second.contains("hub:8410"));
    }

    #[test]
    fn plan_reads_the_codex_home_file() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().to_string_lossy().into_owned();
        std::fs::write(dir.path().join("config.toml"), "model = \"m\"\n").unwrap();

        let outcome = CodexSetup::plan(
            |name| (name == "CODEX_HOME").then(|| root.clone()),
            &endpoint(),
        )
        .unwrap();

        let SetupOutcome::Write(plan) = outcome else {
            panic!("expected a file");
        };
        assert_eq!(plan.path, dir.path().join("config.toml"));
        assert!(plan.content.contains("model = \"m\""));
    }
}
