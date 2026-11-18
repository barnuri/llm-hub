//! Builds the running extension set from built-ins, library extensions and the rules file, and
//! reloads the rules file when it, or a variable it names, changes.

use std::collections::{BTreeSet, HashMap};
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use sha2::{Digest, Sha256};

use crate::consts::ENV_WATCH_INTERVAL;
use crate::dependencies::state::AppState;
use crate::ext::builtin::builtins;
use crate::ext::rules::{self, RulesExtension};
use crate::ext::{Extension, ExtensionSet};

/// The env var that names the rules file.
pub(crate) const EXTENSIONS_FILE_VAR: &str = "LLM_HUB_EXTENSIONS_FILE";

/// How long a changed rules file must stay unchanged before the hub reads it, so a save in
/// progress is never loaded half written.
const RULES_SETTLE: Duration = Duration::from_millis(150);

/// A hash of everything a rules reload depends on.
type Fingerprint = [u8; 32];

/// Where the hub's extensions come from.
pub(crate) struct ExtensionSources {
    library: Vec<Arc<dyn Extension>>,
    dir: PathBuf,
}

impl ExtensionSources {
    pub(crate) fn new(library: Vec<Arc<dyn Extension>>, dir: PathBuf) -> ExtensionSources {
        ExtensionSources { library, dir }
    }

    /// The rules file named by `vars`, else the first default file in the directory.
    pub(crate) fn rules_path(&self, vars: &HashMap<String, String>) -> Option<PathBuf> {
        rules::locate(vars.get(EXTENSIONS_FILE_VAR).map(String::as_str), &self.dir)
    }

    /// Loads the rules file. `None` when there is no file.
    pub(crate) fn load_rules(
        &self,
        vars: &HashMap<String, String>,
    ) -> Result<Option<(PathBuf, RulesExtension)>, String> {
        let Some(path) = self.rules_path(vars) else {
            return Ok(None);
        };
        let env = |name: &str| vars.get(name).cloned();
        rules::load(&path, &env).map(|loaded| Some((path, loaded)))
    }

    /// Built-ins first, then library extensions in registration order, then the rules.
    pub(crate) fn compose(&self, rules: Option<RulesExtension>) -> ExtensionSet {
        let mut all = builtins();
        all.extend(self.library.iter().cloned());
        let mut hooks = None;
        if let Some(mut rules) = rules {
            hooks = rules.take_hooks();
            all.push(Arc::new(rules));
        }
        ExtensionSet::new(all).with_hooks(hooks)
    }

    /// A hash of the rules file path, its bytes, and the value of each `${NAME}` it names.
    /// `None` when there is no readable rules file.
    fn fingerprint(&self, vars: &HashMap<String, String>) -> Option<Fingerprint> {
        let path = self.rules_path(vars)?;
        let bytes = std::fs::read(&path).ok()?;
        let mut hash = Sha256::new();
        hash.update(path.to_string_lossy().as_bytes());
        hash.update(&bytes);
        for name in referenced_vars(&String::from_utf8_lossy(&bytes)) {
            hash.update(name.as_bytes());
            match vars.get(&name) {
                Some(value) => hash.update(value.as_bytes()),
                None => hash.update(b"\0unset"),
            }
        }
        Some(hash.finalize().into())
    }
}

/// The names in every `${NAME}` of `text`.
fn referenced_vars(text: &str) -> BTreeSet<String> {
    let mut names = BTreeSet::new();
    let mut rest = text;
    while let Some(start) = rest.find("${") {
        let after = &rest[start + 2..];
        let Some(end) = after.find('}') else {
            break;
        };
        names.insert(after[..end].to_owned());
        rest = &after[end + 1..];
    }
    names
}

/// Polls the rules file and the variables it names. A valid change replaces the rules. An
/// invalid one keeps the old rules and logs why.
pub(crate) fn spawn_rules_watcher(
    state: AppState,
    sources: Arc<ExtensionSources>,
    load_vars: fn() -> HashMap<String, String>,
) {
    tokio::spawn(async move {
        let mut last = probe(&sources, load_vars).await.1;
        let mut interval = tokio::time::interval(ENV_WATCH_INTERVAL);
        interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        loop {
            interval.tick().await;
            let (_, current) = probe(&sources, load_vars).await;
            if current == last {
                continue;
            }
            tokio::time::sleep(RULES_SETTLE).await;
            let (vars, settled) = probe(&sources, load_vars).await;
            if settled != current {
                continue;
            }
            last = settled;
            let (state, sources) = (state.clone(), Arc::clone(&sources));
            let message =
                tokio::task::spawn_blocking(move || reload(&state, &sources, &vars)).await;
            if let Ok(Some(message)) = message {
                tracing::info!("{message}");
            }
        }
    });
}

/// The current variables and the rules fingerprint, read off the async runtime.
async fn probe(
    sources: &Arc<ExtensionSources>,
    load_vars: fn() -> HashMap<String, String>,
) -> (HashMap<String, String>, Option<Fingerprint>) {
    let sources = Arc::clone(sources);
    tokio::task::spawn_blocking(move || {
        let vars = load_vars();
        let fingerprint = sources.fingerprint(&vars);
        (vars, fingerprint)
    })
    .await
    .unwrap_or_default()
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
            Some("The hub found no extension rules file and cleared the rules.".to_owned())
        }
        Err(e) => {
            tracing::error!(
                "The hub kept the previous extension rules because the new file is invalid: {e}"
            );
            None
        }
    }
}

#[cfg(test)]
mod tests {
    use std::path::Path;
    use std::sync::Mutex;
    use std::time::Duration;

    use axum::http::{Extensions, HeaderMap};
    use bytes::Bytes;

    use super::*;
    use crate::configs::HubConfig;
    use crate::ext::{Api, AttemptCtx, AttemptTarget};
    use crate::services::fake_gh_cli::FakeGhCli;
    use crate::services::gh_cli::GhOutcome;
    use crate::services::memory_credential_store::MemoryCredentialStore;

    struct Library;

    impl Extension for Library {
        fn name(&self) -> &'static str {
            "library"
        }
    }

    fn sources(dir: &Path) -> ExtensionSources {
        ExtensionSources::new(vec![Arc::new(Library)], dir.to_path_buf())
    }

    fn vars_with(pairs: &[(&str, &str)]) -> HashMap<String, String> {
        pairs
            .iter()
            .map(|(key, value)| ((*key).to_owned(), (*value).to_owned()))
            .collect()
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

    /// The `x-team` header the running rules set on an attempt, if any.
    fn attempt_team(state: &AppState) -> Option<String> {
        let headers = HeaderMap::new();
        let mut ext_state = Extensions::new();
        let target = AttemptTarget {
            profile: "p",
            model: "p/m",
            api: Api::Chat,
            client_path: "/v1/chat/completions",
            upstream_path: "/chat/completions",
        };
        let mut ctx = AttemptCtx::new(target, &headers, Bytes::new(), &mut ext_state);
        state.extensions().attempt(&mut ctx);
        let (changed, _) = ctx.into_parts();
        changed?
            .get("x-team")
            .map(|value| value.to_str().unwrap().to_owned())
    }

    async fn wait_for_team(state: &AppState, expected: &str) -> Option<String> {
        let mut seen = attempt_team(state);
        for _ in 0..80 {
            if seen.as_deref() == Some(expected) {
                break;
            }
            tokio::time::sleep(Duration::from_millis(100)).await;
            seen = attempt_team(state);
        }
        seen
    }

    const RULE: &str = "rules:\n  - name: team\n    phase: attempt\n    actions:\n      - set_header: {x-team: '${TEAM}'}\n";

    #[test]
    fn without_a_file_the_set_is_builtins_then_library() {
        let dir = tempfile::tempdir().unwrap();
        let sources = sources(dir.path());

        assert!(sources.rules_path(&HashMap::new()).is_none());
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
        let sources = sources(dir.path());

        let (path, rules) = sources
            .load_rules(&vars_with(&[("TEAM", "blue")]))
            .unwrap()
            .unwrap();

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
        let sources = sources(dir.path());
        let vars = vars_with(&[(EXTENSIONS_FILE_VAR, json.to_str().unwrap())]);

        let error = sources.load_rules(&vars).unwrap_err();

        assert!(error.starts_with(&json.display().to_string()), "{error}");
    }

    #[tokio::test]
    async fn reload_swaps_valid_rules_keeps_old_ones_on_error_and_clears_on_delete() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("extensions.yaml");
        let sources = sources(dir.path());
        let state = state();
        let vars = vars_with(&[("TEAM", "blue")]);

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

    #[test]
    fn the_fingerprint_follows_the_file_the_path_and_the_variables_it_names() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("extensions.yaml");
        let sources = sources(dir.path());
        assert_eq!(sources.fingerprint(&HashMap::new()), None);

        std::fs::write(&file, RULE).unwrap();
        let blue = sources.fingerprint(&vars_with(&[("TEAM", "blue")]));
        assert!(blue.is_some());
        assert_ne!(blue, sources.fingerprint(&vars_with(&[("TEAM", "red")])));
        assert_ne!(blue, sources.fingerprint(&HashMap::new()));
        assert_eq!(
            blue,
            sources.fingerprint(&vars_with(&[("TEAM", "blue"), ("OTHER", "x")]))
        );

        let elsewhere = dir.path().join("other.yaml");
        std::fs::write(&elsewhere, RULE).unwrap();
        let moved = vars_with(&[
            ("TEAM", "blue"),
            (EXTENSIONS_FILE_VAR, elsewhere.to_str().unwrap()),
        ]);
        assert_ne!(blue, sources.fingerprint(&moved));

        std::fs::write(&file, format!("{RULE}\n")).unwrap();
        assert_ne!(blue, sources.fingerprint(&vars_with(&[("TEAM", "blue")])));
    }

    #[test]
    fn referenced_variables_are_found_once_and_an_open_brace_is_ignored() {
        let names = referenced_vars("a: ${ONE}\nb: ${TWO} ${ONE}\nc: ${OPEN");

        assert_eq!(names.into_iter().collect::<Vec<_>>(), ["ONE", "TWO"]);
        assert!(referenced_vars("plain").is_empty());
    }

    #[tokio::test]
    async fn the_watcher_loads_a_file_created_after_start() {
        fn vars() -> HashMap<String, String> {
            vars_with(&[("TEAM", "blue")])
        }
        let dir = tempfile::tempdir().unwrap();
        let sources = Arc::new(sources(dir.path()));
        let state = state();

        spawn_rules_watcher(state.clone(), Arc::clone(&sources), vars);
        tokio::time::sleep(Duration::from_millis(100)).await;
        std::fs::write(dir.path().join("extensions.yaml"), RULE).unwrap();

        assert_eq!(wait_for_team(&state, "blue").await.as_deref(), Some("blue"));
    }

    #[tokio::test]
    async fn the_watcher_reloads_when_a_variable_the_rules_name_changes() {
        static TEAM: Mutex<String> = Mutex::new(String::new());
        fn vars() -> HashMap<String, String> {
            vars_with(&[("TEAM", TEAM.lock().unwrap().as_str())])
        }
        *TEAM.lock().unwrap() = "blue".to_owned();
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("extensions.yaml"), RULE).unwrap();
        let sources = Arc::new(sources(dir.path()));
        let state = state();
        reload(&state, &sources, &vars()).unwrap();
        assert_eq!(attempt_team(&state).as_deref(), Some("blue"));

        spawn_rules_watcher(state.clone(), Arc::clone(&sources), vars);
        tokio::time::sleep(Duration::from_millis(100)).await;
        *TEAM.lock().unwrap() = "red".to_owned();

        assert_eq!(wait_for_team(&state, "red").await.as_deref(), Some("red"));
    }
}
