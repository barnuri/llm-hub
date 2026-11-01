use crate::configs::GithubTokenSource;
use crate::consts::{
    COPILOT_AUTH_OPENCODE, COPILOT_AUTH_VSCODE, COPILOT_OPENCODE_CLIENT_ID,
    COPILOT_VSCODE_CLIENT_ID,
};

/// How a profile authenticates to GitHub Copilot, picked by `LLM_HUB_<NAME>_AUTH`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CopilotFlow {
    /// `github-copilot`: the GitHub OAuth token is sent directly as the bearer.
    Opencode,
    /// `github-copilot-vscode`: the GitHub token is exchanged for a short-lived Copilot token.
    Vscode,
}

impl CopilotFlow {
    /// Parses an `LLM_HUB_<NAME>_AUTH` value. `None` when it is unknown.
    #[must_use]
    pub fn from_auth_value(value: &str) -> Option<CopilotFlow> {
        match value {
            COPILOT_AUTH_OPENCODE => Some(CopilotFlow::Opencode),
            COPILOT_AUTH_VSCODE => Some(CopilotFlow::Vscode),
            _ => None,
        }
    }

    /// The `LLM_HUB_<NAME>_AUTH` value that selects this flow.
    #[must_use]
    pub fn auth_value(self) -> &'static str {
        match self {
            CopilotFlow::Opencode => COPILOT_AUTH_OPENCODE,
            CopilotFlow::Vscode => COPILOT_AUTH_VSCODE,
        }
    }

    /// OAuth client id for this flow's GitHub device login.
    #[must_use]
    pub fn client_id(self) -> &'static str {
        match self {
            CopilotFlow::Opencode => COPILOT_OPENCODE_CLIENT_ID,
            CopilotFlow::Vscode => COPILOT_VSCODE_CLIENT_ID,
        }
    }

    /// Credential store account holding this flow's login token for `domain`.
    #[must_use]
    pub fn credential_account(self, domain: &str) -> String {
        format!("{}:{domain}", self.auth_value())
    }

    /// Where this flow reads its GitHub token when the profile sets no source.
    #[must_use]
    pub fn default_token_source(self) -> GithubTokenSource {
        match self {
            CopilotFlow::Opencode => GithubTokenSource::Auto,
            CopilotFlow::Vscode => GithubTokenSource::Login,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_flow_round_trips_through_its_auth_value() {
        for flow in [CopilotFlow::Opencode, CopilotFlow::Vscode] {
            assert_eq!(CopilotFlow::from_auth_value(flow.auth_value()), Some(flow));
        }
        assert_eq!(CopilotFlow::Opencode.auth_value(), "github-copilot");
        assert_eq!(CopilotFlow::Vscode.auth_value(), "github-copilot-vscode");
    }

    #[test]
    fn unknown_auth_value_is_none() {
        assert_eq!(CopilotFlow::from_auth_value("github"), None);
        assert_eq!(CopilotFlow::from_auth_value(""), None);
    }

    #[test]
    fn each_flow_has_its_own_client_id() {
        assert_eq!(CopilotFlow::Opencode.client_id(), "Ov23li8tweQw6odWQebz");
        assert_eq!(CopilotFlow::Vscode.client_id(), "Iv1.b507a08c87ecfe98");
    }

    #[test]
    fn credential_account_joins_auth_value_and_domain() {
        assert_eq!(
            CopilotFlow::Vscode.credential_account("github.com"),
            "github-copilot-vscode:github.com"
        );
        assert_eq!(
            CopilotFlow::Opencode.credential_account("acme.ghe.com"),
            "github-copilot:acme.ghe.com"
        );
    }

    #[test]
    fn default_token_source_is_auto_for_opencode_and_login_for_vscode() {
        assert_eq!(
            CopilotFlow::Opencode.default_token_source(),
            GithubTokenSource::Auto
        );
        assert_eq!(
            CopilotFlow::Vscode.default_token_source(),
            GithubTokenSource::Login
        );
    }
}
