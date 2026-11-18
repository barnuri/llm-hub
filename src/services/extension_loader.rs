//! Builds the running extension set from built-ins, library extensions and the rules file, and
//! reloads the rules file when it changes.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use sha2::{Digest, Sha256};

use crate::consts::ENV_WATCH_INTERVAL;
use crate::dependencies::state::AppState;
use crate::ext::builtin::builtins;
use crate::ext::rules::{self, RulesExtension};
use crate::ext::{Extension, ExtensionSet};

/// The env var that names the rules file.
pub(crate) const EXTENSIONS_FILE_VAR: &str = "LLM_HUB_EXTENSIONS_FILE";

/// Where the hub's extensions come from.
pub(crate) struct ExtensionSources {
    library: Vec<Arc<dyn Extension>>,
    configured: Option<String>,
    dir: PathBuf,
}

impl ExtensionSources {
    pub(crate) fn new(
        library: Vec<Arc<dyn Extension>>,
        vars: &HashMap<String, String>,
        dir: PathBuf,
    ) -> ExtensionSources {
        ExtensionSources {
            library,
            configured: vars.get(EXTENSIONS_FILE_VAR).cloned(),
            dir,
        }
    }

    /// The rules file in use, if any.
    pub(crate) fn rules_path(&self) -> Option<PathBuf> {
        rules::locate(self.configured.as_deref(), &self.dir)
    }

    /// Loads the rules file. `None` when there is no file.
    pub(crate) fn load_rules(
        &self,
        vars: &HashMap<String, String>,
    ) -> Result<Option<(PathBuf, RulesExtension)>, String> {
        let Some(path) = self.rules_path() else {
            return Ok(None);
        };
        let env = |name: &str| vars.get(name).cloned();
        rules::load(&path, &env).map(|loaded| Some((path, loaded)))
    }

    /// Built-ins first, then library extensions in registration order, then the rules.
    pub(crate) fn compose(&self, rules: Option<RulesExtension>) -> ExtensionSet {
        let mut all = builtins();
        all.extend(self.library.iter().cloned());
        if let Some(rules) = rules {
            all.push(Arc::new(rules));
        }
        ExtensionSet::new(all)
    }
}

/// Polls the rules file. A valid change replaces the rules; an invalid one keeps the old rules
/// and logs why.
pub(crate) fn spawn_rules_watcher(
    state: AppState,
    sources: Arc<ExtensionSources>,
    load_vars: fn() -> HashMap<String, String>,
) {
    tokio::spawn(async move {
        let mut last = fingerprint(sources.rules_path().as_deref());
        let mut interval = tokio::time::interval(ENV_WATCH_INTERVAL);
        interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        loop {
            interval.tick().await;
            let current = fingerprint(sources.rules_path().as_deref());
            if current == last {
                continue;
            }
            last = current;
            if let Some(message) = reload(&state, &sources, &load_vars()) {
                tracing::info!("{message}");
            }
        }
    });
}

/// Reloads the rules into `state`. Returns what happened, for the log.
pub(crate) fn reload(
    state: &AppState,
    sources: &ExtensionSources,
    vars: &HashMap<String, String>,
) -> Option<String> {
    match sources.load_rules(vars) {
        Ok(Some((path, rules))) => {
            let count = rules.len();
            state.set_extensions(sources.compose(Some(rules)));
            Some(format!(
                "loaded {count} extension rules from {}",
                path.display()
            ))
        }
        Ok(None) => {
            state.set_extensions(sources.compose(None));
            Some("no extension rules file, rules cleared".to_owned())
        }
        Err(e) => {
            tracing::error!("kept the previous extension rules: {e}");
            None
        }
    }
}

fn fingerprint(path: Option<&Path>) -> Option<(PathBuf, [u8; 32])> {
    let path = path?;
    let bytes = std::fs::read(path).ok()?;
    Some((path.to_path_buf(), Sha256::digest(bytes).into()))
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use super::*;
    use crate::configs::HubConfig;
    use crate::services::fake_gh_cli::FakeGhCli;
    use crate::services::gh_cli::GhOutcome;
    use crate::services::memory_credential_store::MemoryCredentialStore;

    struct Library;

    impl Extension for Library {
        fn name(&self) -> &'static str {
            "library"
        }
    }

    fn sources(dir: &Path, configured: Option<&str>) -> ExtensionSources {
        let mut vars = HashMap::new();
        if let Some(path) = configured {
            vars.insert(EXTENSIONS_FILE_VAR.to_owned(), path.to_owned());
        }
        ExtensionSources::new(vec![Arc::new(Library)], &vars, dir.to_path_buf())
    }

    fn state() -> AppState {
        AppState::with_seams(
            HubConfig::from_map(&HashMap::new()).unwrap(),
            None,
            Arc::new(MemoryCredentialStore::default()),
            Arc::new(FakeGhCli::new(GhOutcome::NotInstalled)),
        )
        .unwrap()
    }

    const RULE: &str = "rules:\n  - name: team\n    phase: attempt\n    actions:\n      - set_header: {x-team: '${TEAM}'}\n";

    #[test]
    fn without_a_file_the_set_is_builtins_then_library() {
        let dir = tempfile::tempdir().unwrap();
        let sources = sources(dir.path(), None);

        assert!(sources.rules_path().is_none());
        assert!(sources.load_rules(&HashMap::new()).unwrap().is_none());
        assert_eq!(
            sources.compose(None).names(),
            ["copilot-context-code", "library"]
        );
    }

    #[test]
    fn a_default_file_is_found_and_its_rules_come_last() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("extensions.yaml"), RULE).unwrap();
        let sources = sources(dir.path(), None);
        let vars = HashMap::from([("TEAM".to_owned(), "blue".to_owned())]);

        let (path, rules) = sources.load_rules(&vars).unwrap().unwrap();

        assert_eq!(path, dir.path().join("extensions.yaml"));
        assert_eq!(rules.len(), 1);
        assert_eq!(
            sources.compose(Some(rules)).names(),
            ["copilot-context-code", "library", "rules"]
        );
    }

    #[test]
    fn a_configured_json_file_wins_and_errors_name_the_file() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("extensions.yaml"), RULE).unwrap();
        let json = dir.path().join("mine.json");
        std::fs::write(
            &json,
            r#"{"rules": [{"name": "x", "phase": "nope", "actions": []}]}"#,
        )
        .unwrap();
        let sources = sources(dir.path(), Some(json.to_str().unwrap()));

        let error = sources.load_rules(&HashMap::new()).unwrap_err();

        assert!(error.starts_with(&json.display().to_string()), "{error}");
    }

    #[tokio::test]
    async fn reload_swaps_valid_rules_keeps_old_ones_on_error_and_clears_on_delete() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("extensions.yaml");
        let sources = sources(dir.path(), None);
        let state = state();
        let vars = HashMap::from([("TEAM".to_owned(), "blue".to_owned())]);

        std::fs::write(&file, RULE).unwrap();
        let loaded = reload(&state, &sources, &vars).unwrap();
        assert!(loaded.starts_with("loaded 1 extension rules"), "{loaded}");
        assert_eq!(state.extensions().names().last(), Some(&"rules"));

        std::fs::write(&file, "rules: [{name: x}]").unwrap();
        assert!(reload(&state, &sources, &vars).is_none());
        assert_eq!(state.extensions().names().last(), Some(&"rules"));

        std::fs::remove_file(&file).unwrap();
        assert!(reload(&state, &sources, &vars).unwrap().contains("cleared"));
        assert_eq!(
            state.extensions().names(),
            ["copilot-context-code", "library"]
        );
    }

    #[tokio::test]
    async fn the_watcher_loads_a_file_created_after_start() {
        fn vars() -> HashMap<String, String> {
            HashMap::from([("TEAM".to_owned(), "blue".to_owned())])
        }
        let dir = tempfile::tempdir().unwrap();
        let sources = Arc::new(sources(dir.path(), None));
        let state = state();

        spawn_rules_watcher(state.clone(), Arc::clone(&sources), vars);
        tokio::time::sleep(Duration::from_millis(100)).await;
        std::fs::write(dir.path().join("extensions.yaml"), RULE).unwrap();
        for _ in 0..50 {
            if state.extensions().names().last() == Some(&"rules") {
                break;
            }
            tokio::time::sleep(Duration::from_millis(100)).await;
        }

        assert_eq!(state.extensions().names().last(), Some(&"rules"));
    }
}
