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

    /// Splits with POSIX shell quoting rules, or with Windows command-line rules
    /// on Windows (no expansion, no operators either way), and expands a
    /// leading `~/` in each word to `home`.
    ///
    /// # Errors
    /// When the quoting is unbalanced or the command is empty.
    pub fn parse_argv(raw: &str, home: Option<&str>) -> Result<Vec<String>, String> {
        let words = if cfg!(windows) {
            Self::split_windows(raw)
        } else {
            shlex::split(raw)
        }
        .ok_or("has unbalanced quotes")?;
        if words.is_empty() {
            return Err("is empty".to_string());
        }
        Ok(words
            .into_iter()
            .map(|word| Self::expand_home(word, home))
            .collect())
    }

    /// Splits like `CommandLineToArgvW`: a backslash is literal unless it
    /// precedes a `"`, so `C:\tools\server.exe` needs no quoting. `None` when a
    /// quote is left open.
    fn split_windows(raw: &str) -> Option<Vec<String>> {
        let mut words = Vec::new();
        let mut word = String::new();
        let mut in_word = false;
        let mut in_quotes = false;
        let mut backslashes = 0;
        let mut chars = raw.chars().peekable();
        while let Some(c) = chars.next() {
            if c == '\\' {
                backslashes += 1;
                in_word = true;
                continue;
            }
            if c == '"' {
                word.extend(std::iter::repeat_n('\\', backslashes / 2));
                let escaped = backslashes % 2 == 1;
                backslashes = 0;
                in_word = true;
                if escaped {
                    word.push('"');
                } else if in_quotes && chars.peek() == Some(&'"') {
                    chars.next();
                    word.push('"');
                } else {
                    in_quotes = !in_quotes;
                }
                continue;
            }
            word.extend(std::iter::repeat_n('\\', backslashes));
            backslashes = 0;
            if !in_quotes && matches!(c, ' ' | '\t') {
                if in_word {
                    words.push(std::mem::take(&mut word));
                    in_word = false;
                }
                continue;
            }
            word.push(c);
            in_word = true;
        }
        if in_quotes {
            return None;
        }
        word.extend(std::iter::repeat_n('\\', backslashes));
        if in_word {
            words.push(word);
        }
        Some(words)
    }

    fn expand_home(word: String, home: Option<&str>) -> String {
        match (word.strip_prefix("~/"), home) {
            (Some(rest), Some(home)) => {
                format!("{}/{rest}", home.trim_end_matches(['/', '\\']))
            }
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
        assert!(ManagedCommand::parse_argv(r#"srv "open"#, None).is_err());
        assert!(ManagedCommand::parse_argv("   ", None).is_err());
    }

    #[test]
    fn parse_argv_home_with_trailing_backslash_joined_once() {
        let argv = ManagedCommand::parse_argv("srv ~/m.bin", Some(r"C:\Users\me\")).unwrap();
        assert_eq!(argv, vec!["srv", r"C:\Users\me/m.bin"]);
    }

    #[cfg(windows)]
    #[test]
    fn parse_argv_on_windows_keeps_backslash_paths() {
        let argv =
            ManagedCommand::parse_argv(r"C:\tools\whisper-server.exe -m C:\m.bin", None).unwrap();
        assert_eq!(
            argv,
            vec![r"C:\tools\whisper-server.exe", "-m", r"C:\m.bin"]
        );
    }

    #[test]
    fn split_windows_unquoted_backslash_paths_kept_literal() {
        let argv = ManagedCommand::split_windows(
            r"C:\tools\whisper-server.exe --model C:\models\ggml.bin --port 11440",
        )
        .unwrap();
        assert_eq!(
            argv,
            vec![
                r"C:\tools\whisper-server.exe",
                "--model",
                r"C:\models\ggml.bin",
                "--port",
                "11440"
            ]
        );
    }

    #[test]
    fn split_windows_tabs_separate_words() {
        let argv = ManagedCommand::split_windows("srv\t-t\t4").unwrap();
        assert_eq!(argv, vec!["srv", "-t", "4"]);
    }

    #[test]
    fn split_windows_double_quoted_path_with_spaces_single_word() {
        let argv = ManagedCommand::split_windows(
            r#"  "C:\Program Files\whisper\whisper-server.exe"   --model "C:\my models\x.bin" -t 4 "#,
        )
        .unwrap();
        assert_eq!(
            argv,
            vec![
                r"C:\Program Files\whisper\whisper-server.exe",
                "--model",
                r"C:\my models\x.bin",
                "-t",
                "4"
            ]
        );
    }

    #[test]
    fn split_windows_backslashes_before_quote_follow_crt_rules() {
        let argv = ManagedCommand::split_windows(r#"a\\\"b "c\\" d\\"#).unwrap();
        assert_eq!(argv, vec![r#"a\"b"#, r"c\", r"d\\"]);
    }

    #[test]
    fn split_windows_doubled_quote_inside_quotes_is_literal() {
        let argv = ManagedCommand::split_windows(r#""say ""hi""" "" x"#).unwrap();
        assert_eq!(argv, vec![r#"say "hi""#, "", "x"]);
    }

    #[test]
    fn split_windows_single_quotes_and_operators_are_plain_text() {
        let argv = ManagedCommand::split_windows("srv 'a b' & del %HOME%").unwrap();
        assert_eq!(argv, vec!["srv", "'a", "b'", "&", "del", "%HOME%"]);
    }

    #[test]
    fn split_windows_open_quote_rejected() {
        assert_eq!(ManagedCommand::split_windows(r#"srv "C:\open"#), None);
        assert_eq!(ManagedCommand::split_windows(r#"srv "C:\dir\" x"#), None);
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
