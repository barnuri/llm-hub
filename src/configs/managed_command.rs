use std::time::Duration;

use crate::consts::{DEFAULT_MANAGED_IDLE_SECS, DEFAULT_MANAGED_START_TIMEOUT_SECS};

/// A local server llm-hub starts on demand for one profile. It only ever comes
/// from the operator's `.env`: the admin API can neither set nor write it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ManagedCommand {
    /// Program followed by its arguments, executed directly (never via a shell).
    pub argv: Vec<String>,
    /// `host:port` from the profile's `BASE_URL`; the process is ready once it accepts TCP here.
    pub address: String,
    /// `None` when `IDLE_SECS=0`: the process is never stopped for being idle.
    pub idle_timeout: Option<Duration>,
    /// How long to wait for `address` to accept TCP before the attempt fails.
    pub start_timeout: Duration,
}

impl ManagedCommand {
    /// Reads `LLM_HUB_<upper>_COMMAND`, `_IDLE_SECS` and `_START_TIMEOUT_SECS`
    /// through `get`. Returns `None` when the profile has no command.
    ///
    /// # Errors
    /// A message naming the offending variable when the command cannot be
    /// split, the base URL has no host, or a seconds value is not a number.
    pub fn from_env(
        upper: &str,
        base_url: &str,
        home: Option<&str>,
        get: &dyn Fn(&str) -> Option<String>,
    ) -> Result<Option<ManagedCommand>, String> {
        let Some(raw) = get("COMMAND").filter(|v| !v.is_empty()) else {
            return Ok(None);
        };
        let argv =
            Self::parse_argv(&raw, home).map_err(|e| format!("LLM_HUB_{upper}_COMMAND {e}"))?;
        let address = Self::address_from_base_url(base_url)
            .map_err(|e| format!("LLM_HUB_{upper}_BASE_URL {e}"))?;
        let idle_secs = Self::parse_secs(upper, "IDLE_SECS", get, DEFAULT_MANAGED_IDLE_SECS)?;
        let start_secs = Self::parse_secs(
            upper,
            "START_TIMEOUT_SECS",
            get,
            DEFAULT_MANAGED_START_TIMEOUT_SECS,
        )?;
        Ok(Some(ManagedCommand {
            argv,
            address,
            idle_timeout: (idle_secs > 0).then(|| Duration::from_secs(idle_secs)),
            start_timeout: Duration::from_secs(start_secs),
        }))
    }

    /// Splits with POSIX shell quoting rules (no expansion, no operators) and
    /// expands a leading `~/` in each word to `home`.
    ///
    /// # Errors
    /// When the quoting is unbalanced or the command is empty.
    pub fn parse_argv(raw: &str, home: Option<&str>) -> Result<Vec<String>, String> {
        let words = shlex::split(raw).ok_or("has unbalanced quotes")?;
        if words.is_empty() {
            return Err("is empty".to_string());
        }
        Ok(words
            .into_iter()
            .map(|word| Self::expand_home(word, home))
            .collect())
    }

    fn expand_home(word: String, home: Option<&str>) -> String {
        match (word.strip_prefix("~/"), home) {
            (Some(rest), Some(home)) => format!("{}/{rest}", home.trim_end_matches('/')),
            _ => word,
        }
    }

    fn address_from_base_url(base_url: &str) -> Result<String, String> {
        let url = reqwest::Url::parse(base_url).map_err(|e| format!("is not a valid URL: {e}"))?;
        let host = url.host_str().ok_or("has no host")?;
        let port = url.port_or_known_default().ok_or("has no port")?;
        Ok(format!("{host}:{port}"))
    }

    fn parse_secs(
        upper: &str,
        key: &str,
        get: &dyn Fn(&str) -> Option<String>,
        default: u64,
    ) -> Result<u64, String> {
        match get(key).filter(|v| !v.is_empty()) {
            None => Ok(default),
            Some(v) => v
                .parse()
                .map_err(|_| format!("LLM_HUB_{upper}_{key} is not a number of seconds: {v}")),
        }
    }
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;

    use super::*;

    fn from_vars(vars: &[(&str, &str)]) -> Result<Option<ManagedCommand>, String> {
        let map: HashMap<String, String> = vars
            .iter()
            .map(|(k, v)| ((*k).to_string(), (*v).to_string()))
            .collect();
        ManagedCommand::from_env(
            "WHISPER",
            "http://127.0.0.1:11440/v1",
            Some("/Users/me"),
            &|key| map.get(key).cloned(),
        )
    }

    #[test]
    fn parse_argv_quoted_args_kept_as_single_words() {
        let argv =
            ManagedCommand::parse_argv(r#"server --name "two words" 'x y' plain"#, None).unwrap();
        assert_eq!(argv, vec!["server", "--name", "two words", "x y", "plain"]);
    }

    #[test]
    fn parse_argv_leading_tilde_expanded_to_home() {
        let argv = ManagedCommand::parse_argv("~/bin/srv --model ~/m.bin a~/b", Some("/Users/me/"))
            .unwrap();
        assert_eq!(
            argv,
            vec!["/Users/me/bin/srv", "--model", "/Users/me/m.bin", "a~/b"]
        );
    }

    #[test]
    fn parse_argv_without_home_leaves_tilde() {
        let argv = ManagedCommand::parse_argv("srv ~/m.bin", None).unwrap();
        assert_eq!(argv, vec!["srv", "~/m.bin"]);
    }

    #[test]
    fn parse_argv_shell_operators_are_plain_words() {
        let argv = ManagedCommand::parse_argv("srv ; echo pwned && cat $HOME", None).unwrap();
        assert_eq!(
            argv,
            vec!["srv", ";", "echo", "pwned", "&&", "cat", "$HOME"]
        );
    }

    #[test]
    fn parse_argv_unbalanced_or_empty_rejected() {
        assert!(ManagedCommand::parse_argv("srv 'open", None).is_err());
        assert!(ManagedCommand::parse_argv("   ", None).is_err());
    }

    #[test]
    fn from_env_without_command_none() {
        assert_eq!(from_vars(&[("IDLE_SECS", "5")]).unwrap(), None);
        assert_eq!(from_vars(&[("COMMAND", "")]).unwrap(), None);
    }

    #[test]
    fn from_env_defaults_idle_and_start_timeout() {
        let command = from_vars(&[("COMMAND", "srv --port 11440")])
            .unwrap()
            .unwrap();
        assert_eq!(command.argv, vec!["srv", "--port", "11440"]);
        assert_eq!(command.address, "127.0.0.1:11440");
        assert_eq!(
            command.idle_timeout,
            Some(Duration::from_secs(DEFAULT_MANAGED_IDLE_SECS))
        );
        assert_eq!(
            command.start_timeout,
            Duration::from_secs(DEFAULT_MANAGED_START_TIMEOUT_SECS)
        );
    }

    #[test]
    fn from_env_idle_zero_never_stops() {
        let command = from_vars(&[
            ("COMMAND", "srv"),
            ("IDLE_SECS", "0"),
            ("START_TIMEOUT_SECS", "7"),
        ])
        .unwrap()
        .unwrap();
        assert_eq!(command.idle_timeout, None);
        assert_eq!(command.start_timeout, Duration::from_secs(7));
    }

    #[test]
    fn from_env_bad_seconds_error_names_variable() {
        let err = from_vars(&[("COMMAND", "srv"), ("IDLE_SECS", "soon")]).unwrap_err();
        assert!(err.contains("LLM_HUB_WHISPER_IDLE_SECS"), "{err}");
    }

    #[test]
    fn address_from_base_url_without_port_uses_scheme_default() {
        assert_eq!(
            ManagedCommand::address_from_base_url("http://localhost/v1").unwrap(),
            "localhost:80"
        );
    }
}
