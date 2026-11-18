//! Rules files: YAML or JSON rules compiled into one extension. See `docs/extensions.md`.

mod action;
mod matcher;
mod rule_spec;
mod rules_extension;

use std::path::{Path, PathBuf};

pub(crate) use action::{Action, ActionTarget, expand};
pub(crate) use matcher::{Facts, Matcher, header_name, header_value};
use rule_spec::RuleFile;
pub(crate) use rule_spec::{MatchSpec, PhaseName, RuleMode};
pub(crate) use rules_extension::RulesExtension;

/// File names looked up in the working directory when `LLM_HUB_EXTENSIONS_FILE` is not set.
const DEFAULT_FILES: [&str; 3] = ["extensions.yaml", "extensions.yml", "extensions.json"];

/// The rules file to load: `configured` when set, else the first default name found in `dir`.
pub(crate) fn locate(configured: Option<&str>, dir: &Path) -> Option<PathBuf> {
    if let Some(path) = configured.filter(|path| !path.trim().is_empty()) {
        return Some(PathBuf::from(path));
    }
    DEFAULT_FILES
        .iter()
        .map(|name| dir.join(name))
        .find(|path| path.is_file())
}

/// Reads, parses and compiles `path`. The error starts with the path.
pub(crate) fn load(
    path: &Path,
    env: &dyn Fn(&str) -> Option<String>,
) -> Result<RulesExtension, String> {
    let shown = path.display();
    let text = std::fs::read_to_string(path).map_err(|e| format!("{shown}: {e}"))?;
    parse(&text, is_json(path), env).map_err(|e| format!("{shown}: {e}"))
}

/// Parses and compiles rules text.
pub(crate) fn parse(
    text: &str,
    json: bool,
    env: &dyn Fn(&str) -> Option<String>,
) -> Result<RulesExtension, String> {
    let file: RuleFile = if json {
        serde_json::from_str(text).map_err(|e| e.to_string())?
    } else {
        serde_saphyr::from_str(text).map_err(|e| e.to_string())?
    };
    RulesExtension::compile(file.rules, file.hooks, env)
}

fn is_json(path: &Path) -> bool {
    path.extension()
        .is_some_and(|ext| ext.eq_ignore_ascii_case("json"))
}
