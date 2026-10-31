use std::sync::Arc;

use crate::configs::{CopilotAuth, GithubTokenSource, env_name};
use crate::services::credential_store::CredentialStore;
use crate::services::gh_cli::{GhCli, GhOutcome};

/// Returns the GitHub OAuth token for `profile` from the source `auth` selects.
/// `Auto` asks gh first and reads the `copilot login` token when gh gives none.
///
/// # Errors
/// A message that names the next step for the user when no source has a token,
/// or the credential store error when the store cannot be read.
pub async fn resolve(
    profile: &str,
    auth: &CopilotAuth,
    gh: &dyn GhCli,
    credentials: Arc<dyn CredentialStore>,
) -> Result<String, String> {
    match auth.token_source {
        GithubTokenSource::Gh => gh_token(profile, auth, gh).await,
        GithubTokenSource::Login => login_token(auth, credentials)
            .await?
            .ok_or_else(|| format!("run: llm-hub copilot login --profile {profile}")),
        GithubTokenSource::Auto => {
            if let Some(token) = optional_gh_token(profile, auth, gh).await {
                return Ok(token);
            }
            login_token(auth, credentials).await?.ok_or_else(|| {
                format!(
                    "run: llm-hub copilot login --profile {profile} (or install gh and run gh auth login)"
                )
            })
        }
    }
}

async fn gh_token(profile: &str, auth: &CopilotAuth, gh: &dyn GhCli) -> Result<String, String> {
    let reason = match ask_gh(auth, gh).await {
        GhOutcome::Token(token) => return Ok(token),
        GhOutcome::NotInstalled => format!("gh was not found at {:?}", auth.gh_path),
        GhOutcome::Failed(reason) => reason,
    };
    Err(format!(
        "profile {profile} reads its GitHub token from gh, and gh gave no token: {reason}. \
         Set LLM_HUB_{}_GH_PATH to the gh binary, and run gh auth login",
        env_name(profile)
    ))
}

async fn optional_gh_token(profile: &str, auth: &CopilotAuth, gh: &dyn GhCli) -> Option<String> {
    match ask_gh(auth, gh).await {
        GhOutcome::Token(token) => Some(token),
        GhOutcome::NotInstalled => {
            tracing::debug!(
                "profile {profile}: gh was not found at {:?}, so the hub reads the copilot login token",
                auth.gh_path
            );
            None
        }
        GhOutcome::Failed(reason) => {
            tracing::debug!(
                "profile {profile}: gh gave no token ({reason}), so the hub reads the copilot login token"
            );
            None
        }
    }
}

async fn ask_gh(auth: &CopilotAuth, gh: &dyn GhCli) -> GhOutcome {
    gh.auth_token(&auth.gh_path, &auth.github_domain, auth.gh_user.as_deref())
        .await
}

async fn login_token(
    auth: &CopilotAuth,
    credentials: Arc<dyn CredentialStore>,
) -> Result<Option<String>, String> {
    let account = auth.flow.credential_account(&auth.github_domain);
    tokio::task::spawn_blocking(move || credentials.get(&account))
        .await
        .map_err(|e| format!("the credential store read did not finish: {e}"))?
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::configs::CopilotFlow;
    use crate::services::fake_gh_cli::FakeGhCli;
    use crate::services::memory_credential_store::MemoryCredentialStore;

    const PROFILE: &str = "copilot";
    const GH_TOKEN: &str = "gho_from_gh";
    const LOGIN_TOKEN: &str = "gho_from_login";

    /// Fails every read, so a test sees an error whenever the resolver consults it.
    #[derive(Debug)]
    struct UnreadableCredentialStore;

    impl CredentialStore for UnreadableCredentialStore {
        fn get(&self, _account: &str) -> Result<Option<String>, String> {
            Err("the credential store was consulted".to_string())
        }

        fn set(&self, _account: &str, _secret: &str) -> Result<(), String> {
            Err("the credential store was consulted".to_string())
        }

        fn delete(&self, _account: &str) -> Result<bool, String> {
            Err("the credential store was consulted".to_string())
        }
    }

    fn auth(token_source: GithubTokenSource) -> CopilotAuth {
        CopilotAuth {
            flow: CopilotFlow::Opencode,
            token_source,
            github_domain: "github.com".to_string(),
            github_api_url: "https://api.github.com".to_string(),
            gh_path: "gh".to_string(),
            gh_user: None,
        }
    }

    fn store_with_login_token(auth: &CopilotAuth) -> Arc<dyn CredentialStore> {
        let store = MemoryCredentialStore::default();
        store
            .set(
                &auth.flow.credential_account(&auth.github_domain),
                LOGIN_TOKEN,
            )
            .unwrap();
        Arc::new(store)
    }

    #[tokio::test]
    async fn auto_with_gh_token_uses_gh_without_reading_the_store() {
        let auth = auth(GithubTokenSource::Auto);
        let gh = FakeGhCli::new(GhOutcome::Token(GH_TOKEN.to_string()));

        let token = resolve(PROFILE, &auth, &gh, Arc::new(UnreadableCredentialStore)).await;

        assert_eq!(token.as_deref(), Ok(GH_TOKEN));
        assert_eq!(gh.calls(), 1);
    }

    #[tokio::test]
    async fn auto_without_gh_reads_the_login_token() {
        let auth = auth(GithubTokenSource::Auto);
        let gh = FakeGhCli::new(GhOutcome::NotInstalled);

        let token = resolve(PROFILE, &auth, &gh, store_with_login_token(&auth)).await;

        assert_eq!(token.as_deref(), Ok(LOGIN_TOKEN));
    }

    #[tokio::test]
    async fn auto_with_gh_failure_reads_the_login_token() {
        let auth = auth(GithubTokenSource::Auto);
        let gh = FakeGhCli::new(GhOutcome::Failed("not logged in".to_string()));

        let token = resolve(PROFILE, &auth, &gh, store_with_login_token(&auth)).await;

        assert_eq!(token.as_deref(), Ok(LOGIN_TOKEN));
    }

    #[tokio::test]
    async fn auto_with_no_token_anywhere_names_both_ways_to_log_in() {
        let auth = auth(GithubTokenSource::Auto);
        let gh = FakeGhCli::new(GhOutcome::NotInstalled);

        let err = resolve(
            PROFILE,
            &auth,
            &gh,
            Arc::new(MemoryCredentialStore::default()),
        )
        .await
        .unwrap_err();

        assert!(
            err.contains("llm-hub copilot login --profile copilot"),
            "{err}"
        );
        assert!(err.contains("gh auth login"), "{err}");
    }

    #[tokio::test]
    async fn gh_source_without_gh_fails_without_reading_the_store() {
        let auth = auth(GithubTokenSource::Gh);
        let gh = FakeGhCli::new(GhOutcome::NotInstalled);

        let err = resolve(PROFILE, &auth, &gh, Arc::new(UnreadableCredentialStore))
            .await
            .unwrap_err();

        assert!(err.contains("LLM_HUB_COPILOT_GH_PATH"), "{err}");
        assert!(err.contains("gh auth login"), "{err}");
        assert!(!err.contains("consulted"), "{err}");
    }

    #[tokio::test]
    async fn gh_source_failure_includes_the_gh_reason() {
        let auth = auth(GithubTokenSource::Gh);
        let gh = FakeGhCli::new(GhOutcome::Failed("gh exited with status 1".to_string()));

        let err = resolve(PROFILE, &auth, &gh, Arc::new(UnreadableCredentialStore))
            .await
            .unwrap_err();

        assert!(err.contains("gh exited with status 1"), "{err}");
    }

    #[tokio::test]
    async fn login_source_reads_the_store_and_never_calls_gh() {
        let auth = auth(GithubTokenSource::Login);
        let gh = FakeGhCli::new(GhOutcome::Token(GH_TOKEN.to_string()));

        let token = resolve(PROFILE, &auth, &gh, store_with_login_token(&auth)).await;

        assert_eq!(token.as_deref(), Ok(LOGIN_TOKEN));
        assert_eq!(gh.calls(), 0);
    }

    #[tokio::test]
    async fn login_source_without_a_stored_token_names_the_login_command() {
        let auth = auth(GithubTokenSource::Login);
        let gh = FakeGhCli::new(GhOutcome::Token(GH_TOKEN.to_string()));

        let err = resolve(
            PROFILE,
            &auth,
            &gh,
            Arc::new(MemoryCredentialStore::default()),
        )
        .await
        .unwrap_err();

        assert_eq!(err, "run: llm-hub copilot login --profile copilot");
        assert_eq!(gh.calls(), 0);
    }
}
