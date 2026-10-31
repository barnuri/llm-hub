use futures_util::future::BoxFuture;

/// The result of asking the GitHub CLI for a token.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum GhOutcome {
    Token(String),
    NotInstalled,
    /// gh ran but gave no token. Holds a reason that never contains stdout.
    Failed(String),
}

/// Reads a GitHub token the way `gh auth token` does.
pub trait GhCli: Send + Sync {
    /// Returns the token for `host`, using the account `user` when set.
    fn auth_token<'a>(
        &'a self,
        gh_path: &'a str,
        host: &'a str,
        user: Option<&'a str>,
    ) -> BoxFuture<'a, GhOutcome>;
}
