use std::collections::HashMap;
use std::path::{Path, PathBuf};

use crate::consts::{ENV_FILE, LOGS_DIR};

/// Where a hub reads its settings and writes its files. The stock binary uses the working
/// directory and the process environment ([`HubRuntime::process`]). An embedded hub, such as a
/// test, gets its own folder and variables from [`HubRuntime::in_dir`], so it never reads or
/// changes process-wide state.
#[derive(Debug, Clone)]
pub struct HubRuntime {
    /// The only variables the hub sees. `None` merges the process environment instead.
    vars: Option<HashMap<String, String>>,
    env_file: PathBuf,
    logs_dir: PathBuf,
    rules_dir: PathBuf,
}

impl HubRuntime {
    /// The stock binary's runtime: `./.env`, `./logs`, `./` for rules, and the process environment.
    #[must_use]
    pub fn process() -> HubRuntime {
        HubRuntime {
            vars: None,
            env_file: PathBuf::from(ENV_FILE),
            logs_dir: PathBuf::from(LOGS_DIR),
            rules_dir: PathBuf::from("."),
        }
    }

    /// A runtime rooted at `dir`: `<dir>/.env`, `<dir>/logs`, rules in `dir`. The hub sees `vars`
    /// and the `.env` file in `dir`, and no process environment. A relative store path in `vars`
    /// still resolves against the working directory, so give an absolute one.
    #[must_use]
    pub fn in_dir(dir: &Path, vars: HashMap<String, String>) -> HubRuntime {
        HubRuntime {
            vars: Some(vars),
            env_file: dir.join(ENV_FILE),
            logs_dir: dir.join(LOGS_DIR),
            rules_dir: dir.to_path_buf(),
        }
    }

    /// The `.env` file the admin API writes and the watcher reads.
    #[must_use]
    pub fn env_file(&self) -> &Path {
        &self.env_file
    }

    /// The folder of managed process logs.
    #[must_use]
    pub fn logs_dir(&self) -> &Path {
        &self.logs_dir
    }

    /// The folder searched for the default rules file.
    #[must_use]
    pub fn rules_dir(&self) -> &Path {
        &self.rules_dir
    }

    /// The variables now: the `.env` file, then the explicit variables or the process
    /// environment on top. It reads through `from_path_iter`, so it never changes global state.
    #[must_use]
    pub fn load_vars(&self) -> HashMap<String, String> {
        let mut vars: HashMap<String, String> = HashMap::new();
        if let Ok(iter) = dotenvy::from_path_iter(&self.env_file) {
            vars.extend(iter.flatten());
        }
        match &self.vars {
            Some(explicit) => vars.extend(explicit.clone()),
            None => vars.extend(std::env::vars()),
        }
        vars
    }
}

impl Default for HubRuntime {
    fn default() -> Self {
        HubRuntime::process()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn vars(pairs: &[(&str, &str)]) -> HashMap<String, String> {
        pairs
            .iter()
            .map(|(k, v)| ((*k).to_string(), (*v).to_string()))
            .collect()
    }

    #[test]
    fn the_process_runtime_uses_the_working_directory_names() {
        let runtime = HubRuntime::default();
        assert_eq!(runtime.env_file(), Path::new(".env"));
        assert_eq!(runtime.logs_dir(), Path::new("logs"));
        assert_eq!(runtime.rules_dir(), Path::new("."));
    }

    #[test]
    fn the_process_runtime_merges_the_environment_over_the_file() {
        let loaded = HubRuntime::process().load_vars();
        assert_eq!(loaded.get("PATH"), std::env::var("PATH").ok().as_ref());
    }

    #[test]
    fn a_dir_runtime_roots_every_path_in_the_dir() {
        let dir = tempfile::tempdir().unwrap();
        let runtime = HubRuntime::in_dir(dir.path(), HashMap::new());
        assert_eq!(runtime.env_file(), dir.path().join(".env"));
        assert_eq!(runtime.logs_dir(), dir.path().join("logs"));
        assert_eq!(runtime.rules_dir(), dir.path());
    }

    #[test]
    fn a_dir_runtime_sees_its_vars_and_file_but_not_the_process_environment() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join(".env"), "FROM_FILE=a\nBOTH=file\n").unwrap();
        let runtime = HubRuntime::in_dir(dir.path(), vars(&[("BOTH", "explicit"), ("X", "1")]));
        let loaded = runtime.load_vars();
        assert_eq!(
            loaded,
            vars(&[("FROM_FILE", "a"), ("BOTH", "explicit"), ("X", "1")])
        );
    }

    #[test]
    fn a_missing_env_file_loads_only_the_vars() {
        let dir = tempfile::tempdir().unwrap();
        let runtime = HubRuntime::in_dir(dir.path(), vars(&[("X", "1")]));
        assert_eq!(runtime.load_vars(), vars(&[("X", "1")]));
    }
}
