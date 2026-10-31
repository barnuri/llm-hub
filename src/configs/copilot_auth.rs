use crate::configs::{CopilotFlow, GithubTokenSource};
use crate::consts::{DEFAULT_GH_PATH, DEFAULT_GITHUB_API_URL, DEFAULT_GITHUB_DOMAIN};

/// Native GitHub Copilot auth for one profile. Env-only, and none of it is a secret.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CopilotAuth {
    pub flow: CopilotFlow,
    pub token_source: GithubTokenSource,
    /// Host only, without scheme or trailing `/` (`github.com`, `acme.ghe.com`).
    pub github_domain: String,
    /// GitHub REST base for `copilot_internal/*`, without a trailing `/`.
    pub github_api_url: String,
    pub gh_path: String,
    pub gh_user: Option<String>,
}

impl CopilotAuth {
    /// Reads `LLM_HUB_<upper>_AUTH` and its companion keys through `get`.
    /// Returns `None` when `AUTH` is unset or empty.
    ///
    /// # Errors
    /// A message naming the offending variable when a value is unknown or
    /// malformed, or when the token source cannot work with the flow.
    pub fn from_env(
        upper: &str,
        get: &dyn Fn(&str) -> Option<String>,
    ) -> Result<Option<CopilotAuth>, String> {
        let Some(auth) = get("AUTH").filter(|v| !v.is_empty()) else {
            return Ok(None);
        };
        let flow = CopilotFlow::from_auth_value(&auth).ok_or_else(|| {
            format!(
                "LLM_HUB_{upper}_AUTH must be {} or {}, not {auth:?}",
                CopilotFlow::Opencode.auth_value(),
                CopilotFlow::Vscode.auth_value()
            )
        })?;
        let token_source = match get("GITHUB_TOKEN_SOURCE").filter(|v| !v.is_empty()) {
            Some(raw) => raw
                .parse::<GithubTokenSource>()
                .map_err(|e| format!("LLM_HUB_{upper}_GITHUB_TOKEN_SOURCE {e}"))?,
            None => flow.default_token_source(),
        };
        if flow == CopilotFlow::Vscode && token_source != GithubTokenSource::Login {
            return Err(format!(
                "LLM_HUB_{upper}_GITHUB_TOKEN_SOURCE={} does not work with LLM_HUB_{upper}_AUTH={}: \
                 GitHub returns 403 when a gh token is exchanged for a Copilot token. \
                 Use login and run `llm-hub copilot login`, or use AUTH={}",
                token_source.as_str(),
                flow.auth_value(),
                CopilotFlow::Opencode.auth_value()
            ));
        }

        let github_domain = match get("GITHUB_DOMAIN").filter(|v| !v.is_empty()) {
            Some(raw) => normalize_domain(&raw).ok_or_else(|| {
                format!("LLM_HUB_{upper}_GITHUB_DOMAIN is not a host name: {raw}")
            })?,
            None => DEFAULT_GITHUB_DOMAIN.to_string(),
        };
        let github_api_url = match get("GITHUB_API_URL").filter(|v| !v.is_empty()) {
            Some(url) if url.starts_with("http://") || url.starts_with("https://") => {
                url.trim_end_matches('/').to_string()
            }
            Some(_) => {
                return Err(format!(
                    "LLM_HUB_{upper}_GITHUB_API_URL must start with http:// or https://"
                ));
            }
            None if github_domain == DEFAULT_GITHUB_DOMAIN => DEFAULT_GITHUB_API_URL.to_string(),
            None => format!("https://api.{github_domain}"),
        };

        let gh_user = get("GH_USER").filter(|v| !v.is_empty());
        if let Some(user) = &gh_user
            && (user.starts_with('-')
                || !user.chars().all(|c| c.is_ascii_alphanumeric() || c == '-'))
        {
            return Err(format!(
                "LLM_HUB_{upper}_GH_USER may contain only letters, digits and '-', and may not start with '-': {user}"
            ));
        }

        Ok(Some(CopilotAuth {
            flow,
            token_source,
            github_domain,
            github_api_url,
            gh_path: get("GH_PATH")
                .filter(|v| !v.is_empty())
                .unwrap_or_else(|| DEFAULT_GH_PATH.to_string()),
            gh_user,
        }))
    }
}

/// Strips the scheme and trailing `/` from a GitHub host. `None` when what
/// remains is not a plain host name.
fn normalize_domain(raw: &str) -> Option<String> {
    let host = raw
        .strip_prefix("https://")
        .or_else(|| raw.strip_prefix("http://"))
        .unwrap_or(raw)
        .trim_end_matches('/');
    let is_host = !host.is_empty()
        && host
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '.');
    is_host.then(|| host.to_ascii_lowercase())
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;

    use super::*;

    fn parse(pairs: &[(&str, &str)]) -> Result<Option<CopilotAuth>, String> {
        let vars: HashMap<String, String> = pairs
            .iter()
            .map(|(k, v)| ((*k).to_string(), (*v).to_string()))
            .collect();
        CopilotAuth::from_env("COP", &|key| vars.get(key).cloned())
    }

    #[test]
    fn unset_or_empty_auth_is_none() {
        assert_eq!(parse(&[]), Ok(None));
        assert_eq!(parse(&[("AUTH", ""), ("GH_USER", "bad user")]), Ok(None));
    }

    #[test]
    fn opencode_flow_defaults() {
        let auth = parse(&[("AUTH", "github-copilot")]).unwrap().unwrap();
        assert_eq!(
            auth,
            CopilotAuth {
                flow: CopilotFlow::Opencode,
                token_source: GithubTokenSource::Auto,
                github_domain: "github.com".into(),
                github_api_url: "https://api.github.com".into(),
                gh_path: "gh".into(),
                gh_user: None,
            }
        );
    }

    #[test]
    fn vscode_flow_defaults_to_login() {
        let auth = parse(&[("AUTH", "github-copilot-vscode")])
            .unwrap()
            .unwrap();
        assert_eq!(auth.flow, CopilotFlow::Vscode);
        assert_eq!(auth.token_source, GithubTokenSource::Login);
    }

    #[test]
    fn explicit_values_are_kept() {
        let auth = parse(&[
            ("AUTH", "github-copilot"),
            ("GITHUB_TOKEN_SOURCE", "gh"),
            ("GH_PATH", "/Users/me/.local/bin/ghx"),
            ("GH_USER", "my-user-1"),
        ])
        .unwrap()
        .unwrap();
        assert_eq!(auth.token_source, GithubTokenSource::Gh);
        assert_eq!(auth.gh_path, "/Users/me/.local/bin/ghx");
        assert_eq!(auth.gh_user.as_deref(), Some("my-user-1"));
    }

    #[test]
    fn unknown_auth_names_the_variable_and_both_values() {
        let err = parse(&[("AUTH", "github")]).unwrap_err();
        assert!(err.contains("LLM_HUB_COP_AUTH"), "{err}");
        assert!(
            err.contains("github-copilot or github-copilot-vscode"),
            "{err}"
        );
    }

    #[test]
    fn unknown_token_source_names_the_variable() {
        let err =
            parse(&[("AUTH", "github-copilot"), ("GITHUB_TOKEN_SOURCE", "file")]).unwrap_err();
        assert!(err.contains("LLM_HUB_COP_GITHUB_TOKEN_SOURCE"), "{err}");
    }

    #[test]
    fn vscode_flow_rejects_gh_and_auto_sources() {
        for source in ["gh", "auto"] {
            let err = parse(&[
                ("AUTH", "github-copilot-vscode"),
                ("GITHUB_TOKEN_SOURCE", source),
            ])
            .unwrap_err();
            assert!(err.contains("LLM_HUB_COP_GITHUB_TOKEN_SOURCE"), "{err}");
            assert!(err.contains("403"), "{err}");
        }
        assert!(
            parse(&[
                ("AUTH", "github-copilot-vscode"),
                ("GITHUB_TOKEN_SOURCE", "login"),
            ])
            .is_ok()
        );
    }

    #[test]
    fn bad_gh_user_is_rejected() {
        for user in ["me; rm", "a b", "--hostname", "u_ser"] {
            let err = parse(&[("AUTH", "github-copilot"), ("GH_USER", user)]).unwrap_err();
            assert!(err.contains("LLM_HUB_COP_GH_USER"), "{user}: {err}");
        }
    }

    #[test]
    fn enterprise_domain_is_normalized_and_derives_the_api_url() {
        let auth = parse(&[
            ("AUTH", "github-copilot"),
            ("GITHUB_DOMAIN", "https://Acme.GHE.com/"),
        ])
        .unwrap()
        .unwrap();
        assert_eq!(auth.github_domain, "acme.ghe.com");
        assert_eq!(auth.github_api_url, "https://api.acme.ghe.com");
    }

    #[test]
    fn bad_domain_is_rejected() {
        let err = parse(&[
            ("AUTH", "github-copilot"),
            ("GITHUB_DOMAIN", "acme.com/path"),
        ])
        .unwrap_err();
        assert!(err.contains("LLM_HUB_COP_GITHUB_DOMAIN"), "{err}");
    }

    #[test]
    fn api_url_override_must_be_http() {
        let auth = parse(&[
            ("AUTH", "github-copilot"),
            ("GITHUB_API_URL", "http://127.0.0.1:9000/"),
        ])
        .unwrap()
        .unwrap();
        assert_eq!(auth.github_api_url, "http://127.0.0.1:9000");

        let err = parse(&[
            ("AUTH", "github-copilot"),
            ("GITHUB_API_URL", "api.github.com"),
        ])
        .unwrap_err();
        assert!(err.contains("LLM_HUB_COP_GITHUB_API_URL"), "{err}");
    }
}
