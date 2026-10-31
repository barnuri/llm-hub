use std::path::{Path, PathBuf};

use super::hub_endpoint::HubEndpoint;
use crate::schemas::launch::launch_args::LaunchArgs;
use crate::schemas::launch::launch_spec::LaunchSpec;
use crate::schemas::launch::profile_filter::ProfileFilter;

/// `llm-hub pi`: pi on the hub's `/v1/chat/completions`. pi has no base-URL
/// setting, so a `-e` extension registers the provider and `~/.pi/agent` stays.
#[derive(Debug)]
pub struct PiLauncher;

impl PiLauncher {
    const PROGRAM: &str = "pi";
    const PROVIDER_ID: &str = "llm-hub";
    const EXTENSION_SOURCE: &str = include_str!("pi_extension.ts");
    const EXTENSION_FILE: &str = "pi-extension.ts";
    const CACHE_SUBDIR: &str = "llm-hub";
    /// The extension appends it to its `/v1/models` fetch as `?profiles=`.
    const PROFILES_VAR: &str = "LLM_HUB_MODEL_PROFILES";

    /// The pi command, loading `extension` and naming the model under `llm-hub/`.
    /// A non-empty `profiles` limits the extension's model list to them.
    #[must_use]
    pub fn spec(
        endpoint: &HubEndpoint,
        args: LaunchArgs,
        extension: &Path,
        profiles: &ProfileFilter,
    ) -> LaunchSpec {
        let mut spec = LaunchSpec::new(Self::PROGRAM)
            .env(HubEndpoint::url_var(), endpoint.url())
            .env(HubEndpoint::api_key_var(), endpoint.api_key());
        if !profiles.is_empty() {
            spec = spec.env(Self::PROFILES_VAR, profiles.to_list());
        }
        spec = spec.arg("-e").arg(extension.to_string_lossy());
        if let Some(model) = args.model() {
            spec = spec
                .arg("--model")
                .arg(format!("{}/{model}", Self::PROVIDER_ID));
        }
        spec.args(args.into_passthrough())
    }

    /// Writes the embedded extension into the user cache dir and returns its path.
    ///
    /// # Errors
    /// Returns a message when no cache dir is known or the file cannot be written.
    pub fn write_extension(lookup: impl Fn(&str) -> Option<String>) -> Result<PathBuf, String> {
        let dir = Self::cache_dir(lookup)
            .ok_or("cannot find a cache dir: set HOME or XDG_CACHE_HOME")?
            .join(Self::CACHE_SUBDIR);
        std::fs::create_dir_all(&dir)
            .map_err(|e| format!("cannot create {}: {e}", dir.display()))?;
        let path = dir.join(Self::EXTENSION_FILE);
        std::fs::write(&path, Self::EXTENSION_SOURCE)
            .map_err(|e| format!("cannot write {}: {e}", path.display()))?;
        Ok(path)
    }

    fn cache_dir(lookup: impl Fn(&str) -> Option<String>) -> Option<PathBuf> {
        let read = |name: &str| lookup(name).filter(|value| !value.is_empty());
        if let Some(xdg) = read("XDG_CACHE_HOME") {
            return Some(PathBuf::from(xdg));
        }
        if cfg!(windows)
            && let Some(local) = read("LOCALAPPDATA")
        {
            return Some(PathBuf::from(local));
        }
        read("HOME").map(|home| PathBuf::from(home).join(".cache"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn extension_is_loaded_and_the_model_is_prefixed() {
        let endpoint = HubEndpoint::from_lookup(|_| None);

        let spec = PiLauncher::spec(
            &endpoint,
            LaunchArgs::from_strs(&["llama_swap/glm-4.7-flash", "--offline"]),
            Path::new("/cache/llm-hub/pi-extension.ts"),
            &ProfileFilter::default(),
        );

        assert_eq!(spec.program(), "pi");
        assert_eq!(spec.env_value("LLM_HUB_URL"), Some("http://localhost:8888"));
        assert_eq!(spec.env_value("LLM_HUB_API_KEY"), Some("llm-hub"));
        assert_eq!(spec.env_value("LLM_HUB_MODEL_PROFILES"), None);
        assert_eq!(
            spec.argv(),
            [
                "-e",
                "/cache/llm-hub/pi-extension.ts",
                "--model",
                "llm-hub/llama_swap/glm-4.7-flash",
                "--offline",
            ]
        );
    }

    #[test]
    fn without_a_model_pi_keeps_its_default_model() {
        let endpoint = HubEndpoint::from_lookup(|_| None);

        let spec = PiLauncher::spec(
            &endpoint,
            LaunchArgs::from_strs(&["-c"]),
            Path::new("/x.ts"),
            &ProfileFilter::default(),
        );

        assert_eq!(spec.argv(), ["-e", "/x.ts", "-c"]);
    }

    #[test]
    fn profiles_reach_the_extension_by_env() {
        let endpoint = HubEndpoint::from_lookup(|_| None);

        let spec = PiLauncher::spec(
            &endpoint,
            LaunchArgs::from_strs(&[]),
            Path::new("/x.ts"),
            &ProfileFilter::from_list("copilot,ivrit"),
        );

        assert_eq!(
            spec.env_value("LLM_HUB_MODEL_PROFILES"),
            Some("copilot,ivrit")
        );
        assert_eq!(spec.argv(), ["-e", "/x.ts"]);
    }

    #[test]
    fn extension_is_written_under_the_xdg_cache_dir() {
        let cache = tempfile::tempdir().unwrap();
        let cache_path = cache.path().to_string_lossy().to_string();

        let path = PiLauncher::write_extension(|name| {
            (name == "XDG_CACHE_HOME").then(|| cache_path.clone())
        })
        .unwrap();

        assert_eq!(path, cache.path().join("llm-hub/pi-extension.ts"));
        let written = std::fs::read_to_string(&path).unwrap();
        assert!(written.contains("pi.registerProvider(\"llm-hub\""));
        assert!(written.contains("LLM_HUB_MODEL_PROFILES"));
    }

    #[test]
    fn home_cache_is_the_fallback() {
        let dir = PiLauncher::cache_dir(|name| (name == "HOME").then(|| "/home/u".to_string()));

        assert_eq!(dir, Some(PathBuf::from("/home/u/.cache")));
    }

    #[test]
    fn no_home_means_no_cache_dir() {
        assert_eq!(PiLauncher::cache_dir(|_| None), None);
    }
}
