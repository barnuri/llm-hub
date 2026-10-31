use std::str::FromStr;

/// Where a Copilot profile gets its GitHub OAuth token.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GithubTokenSource {
    /// `gh auth token` when gh is installed and logged in, else the `copilot login` token.
    Auto,
    Gh,
    /// The token `llm-hub copilot login` stored in the OS credential store.
    Login,
}

impl GithubTokenSource {
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            GithubTokenSource::Auto => "auto",
            GithubTokenSource::Gh => "gh",
            GithubTokenSource::Login => "login",
        }
    }
}

impl FromStr for GithubTokenSource {
    type Err = String;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        match value {
            "auto" => Ok(GithubTokenSource::Auto),
            "gh" => Ok(GithubTokenSource::Gh),
            "login" => Ok(GithubTokenSource::Login),
            other => Err(format!("must be auto, gh or login, not {other:?}")),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_source_round_trips_through_its_string() {
        for source in [
            GithubTokenSource::Auto,
            GithubTokenSource::Gh,
            GithubTokenSource::Login,
        ] {
            assert_eq!(source.as_str().parse::<GithubTokenSource>(), Ok(source));
        }
    }

    #[test]
    fn unknown_source_is_rejected_with_the_valid_values() {
        let err = "keychain".parse::<GithubTokenSource>().unwrap_err();
        assert!(err.contains("auto, gh or login"), "{err}");
        assert!(err.contains("keychain"), "{err}");
    }
}
