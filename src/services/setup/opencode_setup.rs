use serde_json::{Value, json};

use super::json_config::JsonConfig;
use super::setup_plan::{SetupOutcome, SetupPlan};
use crate::configs::OpencodeContextCaps;
use crate::schemas::launch::hub_model::HubModel;
use crate::services::launch::hub_endpoint::HubEndpoint;
use crate::services::launch::opencode_launcher::OpencodeLauncher;

/// `llm-hub setup opencode`: the `llm-hub` provider in the global `opencode.json`. The shared
/// opencode service reads only that file, never the launcher's `OPENCODE_CONFIG_CONTENT`.
#[derive(Debug)]
pub struct OpencodeSetup;

impl OpencodeSetup {
    const SCHEMA: &str = "https://opencode.ai/config.json";
    const FILE: &str = "opencode.json";
    const PROVIDER_ID: &str = "llm-hub";

    /// # Errors
    /// Returns a message when the catalog is empty, a context cap is invalid, or the existing
    /// config cannot be read or parsed.
    pub fn plan(
        lookup: impl Fn(&str) -> Option<String>,
        endpoint: &HubEndpoint,
        catalog: &[HubModel],
    ) -> Result<SetupOutcome, String> {
        if catalog.is_empty() {
            return Err("the hub lists no models, so there is nothing to configure".to_string());
        }
        let caps = OpencodeContextCaps::from_lookup(&lookup)?;
        let path = OpencodeLauncher::config_dir(&lookup)
            .ok_or("cannot find an opencode config dir: set HOME or XDG_CONFIG_HOME")?
            .join(Self::FILE);
        let existing = SetupPlan::read_existing(&path)?;
        let config = OpencodeLauncher::persistent_config(endpoint, catalog, &caps);
        let content = Self::render(existing.as_deref(), &config)?;
        Ok(SetupOutcome::Write(SetupPlan { path, content }))
    }

    fn render(existing: Option<&str>, config: &Value) -> Result<String, String> {
        let mut root = JsonConfig::parse_object(existing, Self::FILE)?;
        root.entry("$schema").or_insert_with(|| json!(Self::SCHEMA));
        for key in ["enabled_providers", "small_model"] {
            root.insert(key.to_string(), config[key].clone());
        }
        let providers = JsonConfig::object_entry(&mut root, "provider")?;
        providers.insert(
            Self::PROVIDER_ID.to_string(),
            config["provider"][Self::PROVIDER_ID].clone(),
        );
        JsonConfig::render(root)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn endpoint() -> HubEndpoint {
        HubEndpoint::from_lookup(|_| None)
    }

    fn lookup(dir: &std::path::Path) -> impl Fn(&str) -> Option<String> {
        let dir = dir.to_string_lossy().into_owned();
        move |name| (name == "XDG_CONFIG_HOME").then(|| dir.clone())
    }

    fn written(outcome: SetupOutcome) -> SetupPlan {
        match outcome {
            SetupOutcome::Write(plan) => plan,
            SetupOutcome::Manual(_) => panic!("expected a file"),
        }
    }

    #[test]
    fn a_new_config_holds_the_provider_and_the_schema() {
        let dir = tempfile::tempdir().unwrap();
        let catalog = [HubModel::new("copilot/auto", None)];

        let plan = written(OpencodeSetup::plan(lookup(dir.path()), &endpoint(), &catalog).unwrap());

        assert_eq!(plan.path, dir.path().join("opencode/opencode.json"));
        let config: Value = serde_json::from_str(&plan.content).unwrap();
        assert_eq!(config["$schema"], "https://opencode.ai/config.json");
        assert_eq!(config["enabled_providers"], json!(["llm-hub"]));
        assert_eq!(config["small_model"], "llm-hub/hub/smart-auto-fast");
        assert_eq!(
            config["provider"]["llm-hub"]["options"],
            json!({"baseURL": "http://localhost:8888/v1", "apiKey": "llm-hub"})
        );
        assert_eq!(
            config["provider"]["llm-hub"]["models"],
            json!({"copilot/auto": {}})
        );
    }

    #[test]
    fn unrelated_keys_and_other_providers_stay() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("opencode/opencode.json");
        std::fs::create_dir_all(file.parent().unwrap()).unwrap();
        std::fs::write(
            &file,
            r#"{"theme": "tokyo", "provider": {"other": {"name": "o"}, "llm-hub": {"stale": 1}}}"#,
        )
        .unwrap();
        let catalog = [HubModel::new("copilot/auto", None)];

        let plan = written(OpencodeSetup::plan(lookup(dir.path()), &endpoint(), &catalog).unwrap());

        let config: Value = serde_json::from_str(&plan.content).unwrap();
        assert_eq!(config["theme"], "tokyo");
        assert_eq!(config["provider"]["other"], json!({"name": "o"}));
        assert!(config["provider"]["llm-hub"].get("stale").is_none());
    }

    #[test]
    fn running_on_its_own_output_changes_nothing() {
        let dir = tempfile::tempdir().unwrap();
        let catalog = [HubModel::new("copilot/auto", None)];
        let first =
            written(OpencodeSetup::plan(lookup(dir.path()), &endpoint(), &catalog).unwrap());
        first.apply(false).unwrap();

        let second =
            written(OpencodeSetup::plan(lookup(dir.path()), &endpoint(), &catalog).unwrap());

        assert_eq!(first.content, second.content);
    }

    #[test]
    fn an_empty_catalog_is_an_error() {
        let dir = tempfile::tempdir().unwrap();

        assert!(OpencodeSetup::plan(lookup(dir.path()), &endpoint(), &[]).is_err());
    }

    #[test]
    fn a_broken_existing_config_is_an_error_and_stays_untouched() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("opencode/opencode.json");
        std::fs::create_dir_all(file.parent().unwrap()).unwrap();
        std::fs::write(&file, "{ // comment").unwrap();
        let catalog = [HubModel::new("copilot/auto", None)];

        let error = OpencodeSetup::plan(lookup(dir.path()), &endpoint(), &catalog).unwrap_err();

        assert!(error.starts_with("opencode.json is not valid JSON"));
        assert_eq!(std::fs::read_to_string(&file).unwrap(), "{ // comment");
    }

    #[test]
    fn no_config_dir_is_an_error() {
        let catalog = [HubModel::new("copilot/auto", None)];

        let error = OpencodeSetup::plan(|_| None, &endpoint(), &catalog).unwrap_err();

        assert!(error.starts_with("cannot find an opencode config dir"));
    }

    #[test]
    fn an_invalid_context_cap_setting_is_an_error() {
        let dir = tempfile::tempdir().unwrap();
        let base = lookup(dir.path());
        let catalog = [HubModel::new("copilot/auto", None)];

        let error = OpencodeSetup::plan(
            |name| {
                if name == "LLM_HUB_OPENCODE_CONTEXT_CAPS" {
                    Some("copilot/auto".to_string())
                } else {
                    base(name)
                }
            },
            &endpoint(),
            &catalog,
        )
        .unwrap_err();

        assert!(error.starts_with("LLM_HUB_OPENCODE_CONTEXT_CAPS entry"));
    }
}
