use std::path::PathBuf;

/// Where a harness keeps its user config.
#[derive(Debug)]
pub struct ConfigPaths;

impl ConfigPaths {
    /// `override_var`'s value when set, else `<home>/<default_under_home>`.
    ///
    /// # Errors
    /// Returns a message when neither the variable nor `HOME` is set.
    pub fn dir(
        lookup: impl Fn(&str) -> Option<String>,
        override_var: &str,
        default_under_home: &str,
    ) -> Result<PathBuf, String> {
        let read = |name: &str| lookup(name).filter(|value| !value.is_empty());
        if let Some(dir) = read(override_var) {
            return Ok(PathBuf::from(dir));
        }
        read("HOME")
            .map(|home| PathBuf::from(home).join(default_under_home))
            .ok_or_else(|| format!("cannot find the config dir: set HOME or {override_var}"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_override_wins_over_home() {
        let dir = ConfigPaths::dir(|name| Some(format!("/{name}")), "CODEX_HOME", ".codex");

        assert_eq!(dir, Ok(PathBuf::from("/CODEX_HOME")));
    }

    #[test]
    fn home_is_the_fallback() {
        let dir = ConfigPaths::dir(
            |name| (name == "HOME").then(|| "/h".to_string()),
            "CODEX_HOME",
            ".codex",
        );

        assert_eq!(dir, Ok(PathBuf::from("/h/.codex")));
    }

    #[test]
    fn no_home_is_an_error() {
        let error = ConfigPaths::dir(|_| Some(String::new()), "CODEX_HOME", ".codex").unwrap_err();

        assert!(error.contains("CODEX_HOME"));
    }
}
