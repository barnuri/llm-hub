use std::sync::Arc;
use std::time::Duration;

use reqwest::header::{ACCEPT, USER_AGENT};
use serde::Deserialize;
use serde_json::json;
use tokio::time::Instant;

use crate::configs::{CopilotAuth, GithubTokenSource, HubConfig, env_name};
use crate::consts::{COPILOT_DEVICE_POLL_MARGIN, GITHUB_OAUTH_SCOPE, VERSION};
use crate::services::copilot_headers::ACCEPT_JSON;
use crate::services::copilot_sessions::CopilotSessionCache;
use crate::services::credential_store::CredentialStore;
use crate::services::gh_cli::GhCli;

const DEVICE_CODE_PATH: &str = "login/device/code";
const ACCESS_TOKEN_PATH: &str = "login/oauth/access_token";
const DEVICE_GRANT_TYPE: &str = "urn:ietf:params:oauth:grant-type:device_code";
const ERROR_AUTHORIZATION_PENDING: &str = "authorization_pending";
const ERROR_SLOW_DOWN: &str = "slow_down";
const SLOW_DOWN_STEP: Duration = Duration::from_secs(5);

#[derive(Deserialize)]
struct DeviceCodeResponse {
    device_code: String,
    user_code: String,
    verification_uri: String,
    expires_in: u64,
    interval: u64,
}

/// GitHub answers every poll with HTTP 200: either `access_token` or `error` is set.
#[derive(Deserialize)]
struct AccessTokenResponse {
    access_token: Option<String>,
    error: Option<String>,
    error_description: Option<String>,
    interval: Option<u64>,
}

/// Hands the candidate login token to the session cache, so the check before
/// saving runs the same Copilot handshake as a real request.
struct CandidateToken(String);

impl CredentialStore for CandidateToken {
    fn get(&self, _account: &str) -> Result<Option<String>, String> {
        Ok(Some(self.0.clone()))
    }

    fn set(&self, _account: &str, _secret: &str) -> Result<(), String> {
        Err("the candidate login token cannot be overwritten".to_string())
    }

    fn delete(&self, _account: &str) -> Result<bool, String> {
        Err("the candidate login token cannot be deleted".to_string())
    }
}

/// Returns the profile named `requested`, or the only Copilot profile when
/// `requested` is `None`, together with its Copilot auth.
///
/// # Errors
/// When the named profile does not exist or does not use Copilot, or when no
/// name is given and the config has zero or several Copilot profiles.
pub fn select_profile<'a>(
    config: &'a HubConfig,
    requested: Option<&str>,
) -> Result<(&'a str, &'a CopilotAuth), String> {
    let candidates: Vec<(&str, &CopilotAuth)> = config
        .profiles
        .iter()
        .filter_map(|p| p.copilot.as_ref().map(|auth| (p.name.as_str(), auth)))
        .collect();
    let names = candidates
        .iter()
        .map(|(name, _)| *name)
        .collect::<Vec<_>>()
        .join(", ");
    if let Some(name) = requested {
        let Some(profile) = config.profile(name) else {
            return Err(format!(
                "no profile is named {name}. Copilot profiles: {names}"
            ));
        };
        return profile
            .copilot
            .as_ref()
            .map(|auth| (profile.name.as_str(), auth))
            .ok_or_else(|| {
                format!(
                    "profile {name} does not set LLM_HUB_{}_AUTH, so it does not use Copilot. Copilot profiles: {names}",
                    env_name(name)
                )
            });
    }
    match candidates.as_slice() {
        [only] => Ok(*only),
        [] => Err(
            "no profile sets LLM_HUB_<NAME>_AUTH to github-copilot or github-copilot-vscode"
                .to_string(),
        ),
        _ => Err(format!(
            "several profiles use Copilot: {names}. Pick one with --profile NAME"
        )),
    }
}

/// Runs the GitHub device login for `auth`'s flow, checks that the new token
/// has Copilot access, and stores it in `credentials` under the flow's account.
///
/// # Errors
/// When the device login fails or expires, when GitHub denies Copilot access
/// to the token, or when the credential store cannot be written. Nothing is
/// stored on error.
pub async fn login(
    client: &reqwest::Client,
    profile: &str,
    auth: &CopilotAuth,
    credentials: Arc<dyn CredentialStore>,
    gh: Arc<dyn GhCli>,
) -> Result<(), String> {
    let oauth_base = format!("https://{}", auth.github_domain);
    login_at(
        client,
        profile,
        auth,
        &oauth_base,
        COPILOT_DEVICE_POLL_MARGIN,
        credentials,
        gh,
    )
    .await
}

/// [`login`] with the device login base URL and the polling margin passed in.
pub(crate) async fn login_at(
    client: &reqwest::Client,
    profile: &str,
    auth: &CopilotAuth,
    oauth_base: &str,
    poll_margin: Duration,
    credentials: Arc<dyn CredentialStore>,
    gh: Arc<dyn GhCli>,
) -> Result<(), String> {
    let token = run_device_flow(client, oauth_base, auth.flow.client_id(), poll_margin).await?;
    verify_copilot_access(client, profile, auth, &token, gh)
        .await
        .map_err(|e| {
            format!(
                "GitHub issued a token, but the Copilot check failed, so nothing was stored: {e}"
            )
        })?;
    let account = auth.flow.credential_account(&auth.github_domain);
    tokio::task::spawn_blocking(move || credentials.set(&account, &token))
        .await
        .map_err(|e| format!("the credential store write did not finish: {e}"))?
}

/// Removes the stored login token for `auth`'s flow. Returns whether one existed.
///
/// # Errors
/// When the credential store cannot be written.
pub async fn logout(
    auth: &CopilotAuth,
    credentials: Arc<dyn CredentialStore>,
) -> Result<bool, String> {
    let account = auth.flow.credential_account(&auth.github_domain);
    tokio::task::spawn_blocking(move || credentials.delete(&account))
        .await
        .map_err(|e| format!("the credential store delete did not finish: {e}"))?
}

/// Runs the GitHub OAuth device flow against `oauth_base` and returns the
/// access token. Prints the URL and code the user must enter.
///
/// # Errors
/// When GitHub rejects a request, the user denies access, or the device code
/// expires before the user approves it.
pub(crate) async fn run_device_flow(
    client: &reqwest::Client,
    oauth_base: &str,
    client_id: &str,
    poll_margin: Duration,
) -> Result<String, String> {
    let device = request_device_code(client, oauth_base, client_id).await?;
    println!(
        "Open {} and enter the code {}",
        device.verification_uri, device.user_code
    );
    let deadline = Instant::now() + Duration::from_secs(device.expires_in);
    let mut interval = Duration::from_secs(device.interval);
    loop {
        tokio::time::sleep(interval + poll_margin).await;
        if Instant::now() >= deadline {
            return Err(
                "the device code expired before the login was approved. Run the login again"
                    .to_string(),
            );
        }
        let poll = poll_access_token(client, oauth_base, client_id, &device.device_code).await?;
        if let Some(token) = poll.access_token.filter(|t| !t.is_empty()) {
            return Ok(token);
        }
        match poll.error.as_deref() {
            Some(ERROR_AUTHORIZATION_PENDING) => {}
            Some(ERROR_SLOW_DOWN) => interval = slowed_interval(interval, poll.interval),
            Some(error) => {
                let detail = poll
                    .error_description
                    .map(|d| format!(": {d}"))
                    .unwrap_or_default();
                return Err(format!(
                    "GitHub ended the device login with {error}{detail}"
                ));
            }
            None => {
                return Err(
                    "GitHub answered the device login poll with neither a token nor an error"
                        .to_string(),
                );
            }
        }
    }
}

/// The interval GitHub returned with `slow_down`, else the current one plus 5 s.
fn slowed_interval(current: Duration, returned_secs: Option<u64>) -> Duration {
    returned_secs.map_or(current + SLOW_DOWN_STEP, Duration::from_secs)
}

async fn request_device_code(
    client: &reqwest::Client,
    oauth_base: &str,
    client_id: &str,
) -> Result<DeviceCodeResponse, String> {
    let response = client
        .post(format!("{oauth_base}/{DEVICE_CODE_PATH}"))
        .header(ACCEPT, ACCEPT_JSON)
        .header(USER_AGENT, format!("llm-hub/{VERSION}"))
        .json(&json!({ "client_id": client_id, "scope": GITHUB_OAUTH_SCOPE }))
        .send()
        .await
        .map_err(|e| format!("the device code request failed: {e}"))?;
    let status = response.status();
    if !status.is_success() {
        return Err(format!("the device code request returned status {status}"));
    }
    response
        .json()
        .await
        .map_err(|e| format!("the device code response is not readable: {e}"))
}

async fn poll_access_token(
    client: &reqwest::Client,
    oauth_base: &str,
    client_id: &str,
    device_code: &str,
) -> Result<AccessTokenResponse, String> {
    let response = client
        .post(format!("{oauth_base}/{ACCESS_TOKEN_PATH}"))
        .header(ACCEPT, ACCEPT_JSON)
        .header(USER_AGENT, format!("llm-hub/{VERSION}"))
        .json(&json!({
            "client_id": client_id,
            "device_code": device_code,
            "grant_type": DEVICE_GRANT_TYPE,
        }))
        .send()
        .await
        .map_err(|e| format!("the device login poll failed: {e}"))?;
    let status = response.status();
    if !status.is_success() {
        return Err(format!("the device login poll returned status {status}"));
    }
    // The decode error is dropped on purpose: it can quote the response body, which holds the token.
    response.json().await.map_err(|_| {
        "the device login poll returned a body that is not a token response".to_string()
    })
}

/// Runs one Copilot handshake with `token` as the login token.
async fn verify_copilot_access(
    client: &reqwest::Client,
    profile: &str,
    auth: &CopilotAuth,
    token: &str,
    gh: Arc<dyn GhCli>,
) -> Result<(), String> {
    let candidate = CopilotAuth {
        token_source: GithubTokenSource::Login,
        ..auth.clone()
    };
    let cache = CopilotSessionCache::new(Arc::new(CandidateToken(token.to_string())), gh);
    cache.get(client, profile, &candidate).await.map(|_| ())
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;

    use serde_json::Value;
    use wiremock::matchers::{body_partial_json, method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    use super::*;
    use crate::configs::CopilotFlow;
    use crate::services::fake_gh_cli::FakeGhCli;
    use crate::services::gh_cli::GhOutcome;
    use crate::services::memory_credential_store::MemoryCredentialStore;

    const PROFILE: &str = "copilot";
    const LOGIN_TOKEN: &str = "gho_from_device_login";
    const DEVICE_CODE: &str = "dev-123";
    const DEVICE_CODE_URL_PATH: &str = "/login/device/code";
    const ACCESS_TOKEN_URL_PATH: &str = "/login/oauth/access_token";
    const USER_URL_PATH: &str = "/copilot_internal/user";
    const EXCHANGE_URL_PATH: &str = "/copilot_internal/v2/token";

    fn auth(flow: CopilotFlow, api_url: &str) -> CopilotAuth {
        CopilotAuth {
            flow,
            token_source: flow.default_token_source(),
            github_domain: "github.com".to_string(),
            github_api_url: api_url.to_string(),
            gh_path: "gh".to_string(),
            gh_user: None,
        }
    }

    fn unused_gh() -> Arc<FakeGhCli> {
        Arc::new(FakeGhCli::new(GhOutcome::NotInstalled))
    }

    async fn mount_device_code(server: &MockServer, client_id: &str, expires_in: u64) {
        Mock::given(method("POST"))
            .and(path(DEVICE_CODE_URL_PATH))
            .and(body_partial_json(
                json!({ "client_id": client_id, "scope": "read:user" }),
            ))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "device_code": DEVICE_CODE,
                "user_code": "ABCD-1234",
                "verification_uri": "https://github.com/login/device",
                "expires_in": expires_in,
                "interval": 0,
            })))
            .expect(1)
            .mount(server)
            .await;
    }

    /// Answers the polls with `answers` in order, the last one repeating.
    async fn mount_polls(server: &MockServer, client_id: &str, answers: &[Value]) {
        let (last, earlier) = answers.split_last().unwrap();
        let poll = || {
            Mock::given(method("POST"))
                .and(path(ACCESS_TOKEN_URL_PATH))
                .and(body_partial_json(json!({
                    "client_id": client_id,
                    "device_code": DEVICE_CODE,
                    "grant_type": DEVICE_GRANT_TYPE,
                })))
        };
        for answer in earlier {
            poll()
                .respond_with(ResponseTemplate::new(200).set_body_json(answer))
                .up_to_n_times(1)
                .expect(1)
                .mount(server)
                .await;
        }
        poll()
            .respond_with(ResponseTemplate::new(200).set_body_json(last))
            .mount(server)
            .await;
    }

    async fn mount_user(server: &MockServer, chat_enabled: bool) {
        Mock::given(method("GET"))
            .and(path(USER_URL_PATH))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "chat_enabled": chat_enabled,
                "can_signup_for_limited": false,
                "endpoints": { "api": "https://api.business.githubcopilot.com" },
            })))
            .mount(server)
            .await;
    }

    fn pending() -> Value {
        json!({ "error": "authorization_pending" })
    }

    fn granted() -> Value {
        json!({ "access_token": LOGIN_TOKEN, "token_type": "bearer", "scope": "read:user" })
    }

    async fn device_flow(server: &MockServer, client_id: &str) -> Result<String, String> {
        run_device_flow(
            &reqwest::Client::new(),
            &server.uri(),
            client_id,
            Duration::ZERO,
        )
        .await
    }

    async fn login_with(
        server: &MockServer,
        auth: &CopilotAuth,
        store: &Arc<MemoryCredentialStore>,
    ) -> Result<(), String> {
        login_at(
            &reqwest::Client::new(),
            PROFILE,
            auth,
            &server.uri(),
            Duration::ZERO,
            Arc::clone(store) as Arc<dyn CredentialStore>,
            unused_gh(),
        )
        .await
    }

    async fn poll_count(server: &MockServer) -> usize {
        server
            .received_requests()
            .await
            .unwrap()
            .iter()
            .filter(|r| r.url.path() == ACCESS_TOKEN_URL_PATH)
            .count()
    }

    #[tokio::test]
    async fn pending_twice_then_token_returns_token() {
        let server = MockServer::start().await;
        let client_id = CopilotFlow::Opencode.client_id();
        mount_device_code(&server, client_id, 900).await;
        mount_polls(&server, client_id, &[pending(), pending(), granted()]).await;

        let token = device_flow(&server, client_id).await;

        assert_eq!(token.as_deref(), Ok(LOGIN_TOKEN));
        assert_eq!(poll_count(&server).await, 3);
    }

    #[tokio::test]
    async fn each_flow_sends_its_own_client_id() {
        for flow in [CopilotFlow::Opencode, CopilotFlow::Vscode] {
            let server = MockServer::start().await;
            mount_device_code(&server, flow.client_id(), 900).await;
            mount_polls(&server, flow.client_id(), &[granted()]).await;

            let token = device_flow(&server, flow.client_id()).await;

            assert_eq!(token.as_deref(), Ok(LOGIN_TOKEN), "{flow:?}");
        }
    }

    #[tokio::test]
    async fn slow_down_causes_one_more_poll_before_token() {
        let server = MockServer::start().await;
        let client_id = CopilotFlow::Opencode.client_id();
        mount_device_code(&server, client_id, 900).await;
        let slow_down = json!({ "error": "slow_down", "interval": 0 });
        mount_polls(&server, client_id, &[slow_down, granted()]).await;

        let token = device_flow(&server, client_id).await;

        assert_eq!(token.as_deref(), Ok(LOGIN_TOKEN));
        assert_eq!(poll_count(&server).await, 2);
    }

    #[test]
    fn slow_down_without_interval_adds_five_seconds() {
        assert_eq!(
            slowed_interval(Duration::from_secs(5), None),
            Duration::from_secs(10)
        );
        assert_eq!(
            slowed_interval(Duration::from_secs(5), Some(7)),
            Duration::from_secs(7)
        );
    }

    #[tokio::test]
    async fn access_denied_and_expired_token_fail() {
        for error in ["access_denied", "expired_token"] {
            let server = MockServer::start().await;
            let client_id = CopilotFlow::Opencode.client_id();
            mount_device_code(&server, client_id, 900).await;
            let answer = json!({ "error": error, "error_description": "nope" });
            mount_polls(&server, client_id, &[pending(), answer]).await;

            let err = device_flow(&server, client_id).await.unwrap_err();

            assert!(err.contains(error), "{err}");
            assert!(err.contains("nope"), "{err}");
        }
    }

    #[tokio::test]
    async fn past_deadline_fails_without_polling() {
        let server = MockServer::start().await;
        let client_id = CopilotFlow::Opencode.client_id();
        mount_device_code(&server, client_id, 0).await;
        mount_polls(&server, client_id, &[granted()]).await;

        let err = device_flow(&server, client_id).await.unwrap_err();

        assert!(err.contains("expired"), "{err}");
        assert_eq!(poll_count(&server).await, 0);
    }

    #[tokio::test]
    async fn device_code_error_status_fails() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path(DEVICE_CODE_URL_PATH))
            .respond_with(ResponseTemplate::new(404))
            .mount(&server)
            .await;

        let err = device_flow(&server, CopilotFlow::Opencode.client_id())
            .await
            .unwrap_err();

        assert!(err.contains("404"), "{err}");
    }

    #[tokio::test]
    async fn opencode_login_with_copilot_access_stores_token() {
        let server = MockServer::start().await;
        let auth = auth(CopilotFlow::Opencode, &server.uri());
        mount_device_code(&server, auth.flow.client_id(), 900).await;
        mount_polls(&server, auth.flow.client_id(), &[granted()]).await;
        mount_user(&server, true).await;
        let store = Arc::new(MemoryCredentialStore::default());

        login_with(&server, &auth, &store).await.unwrap();

        assert_eq!(
            store.get("github-copilot:github.com").unwrap().as_deref(),
            Some(LOGIN_TOKEN)
        );
    }

    #[tokio::test]
    async fn opencode_login_without_chat_stores_nothing() {
        let server = MockServer::start().await;
        let auth = auth(CopilotFlow::Opencode, &server.uri());
        mount_device_code(&server, auth.flow.client_id(), 900).await;
        mount_polls(&server, auth.flow.client_id(), &[granted()]).await;
        mount_user(&server, false).await;
        let store = Arc::new(MemoryCredentialStore::default());

        let err = login_with(&server, &auth, &store).await.unwrap_err();

        assert!(err.contains("nothing was stored"), "{err}");
        assert_eq!(store.get("github-copilot:github.com").unwrap(), None);
    }

    #[tokio::test]
    async fn vscode_login_stores_token_after_exchange() {
        let server = MockServer::start().await;
        let auth = auth(CopilotFlow::Vscode, &server.uri());
        mount_device_code(&server, auth.flow.client_id(), 900).await;
        mount_polls(&server, auth.flow.client_id(), &[granted()]).await;
        Mock::given(method("GET"))
            .and(path(EXCHANGE_URL_PATH))
            .and(wiremock::matchers::header(
                "authorization",
                format!("token {LOGIN_TOKEN}").as_str(),
            ))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "token": "tid=1;exp=2",
                "expires_at": 4_000_000_000_u64,
                "refresh_in": 1500,
            })))
            .expect(1)
            .mount(&server)
            .await;
        let store = Arc::new(MemoryCredentialStore::default());

        login_with(&server, &auth, &store).await.unwrap();

        assert_eq!(
            store
                .get("github-copilot-vscode:github.com")
                .unwrap()
                .as_deref(),
            Some(LOGIN_TOKEN)
        );
    }

    #[tokio::test]
    async fn vscode_login_with_denied_exchange_stores_nothing() {
        let server = MockServer::start().await;
        let auth = auth(CopilotFlow::Vscode, &server.uri());
        mount_device_code(&server, auth.flow.client_id(), 900).await;
        mount_polls(&server, auth.flow.client_id(), &[granted()]).await;
        Mock::given(method("GET"))
            .and(path(EXCHANGE_URL_PATH))
            .respond_with(ResponseTemplate::new(403))
            .mount(&server)
            .await;
        let store = Arc::new(MemoryCredentialStore::default());

        let err = login_with(&server, &auth, &store).await.unwrap_err();

        assert!(err.contains("403"), "{err}");
        assert_eq!(store.get("github-copilot-vscode:github.com").unwrap(), None);
    }

    #[tokio::test]
    async fn logout_removes_entry() {
        let auth = auth(CopilotFlow::Opencode, "https://api.github.com");
        let store = Arc::new(MemoryCredentialStore::default());
        store.set("github-copilot:github.com", LOGIN_TOKEN).unwrap();
        let credentials = Arc::clone(&store) as Arc<dyn CredentialStore>;

        assert!(logout(&auth, Arc::clone(&credentials)).await.unwrap());
        assert!(!logout(&auth, credentials).await.unwrap());
        assert_eq!(store.get("github-copilot:github.com").unwrap(), None);
    }

    fn config(profiles: &[(&str, Option<&str>)]) -> HubConfig {
        let mut vars: HashMap<String, String> = HashMap::new();
        let names: Vec<&str> = profiles.iter().map(|(name, _)| *name).collect();
        vars.insert("LLM_HUB_PROFILES".into(), names.join(","));
        for (name, auth_value) in profiles {
            let upper = env_name(name);
            vars.insert(
                format!("LLM_HUB_{upper}_BASE_URL"),
                "https://api.githubcopilot.com".into(),
            );
            match auth_value {
                Some(value) => {
                    vars.insert(format!("LLM_HUB_{upper}_AUTH"), (*value).into());
                }
                None => {
                    vars.insert(format!("LLM_HUB_{upper}_API_KEY"), "sk-test".into());
                }
            }
        }
        HubConfig::from_map(&vars).unwrap()
    }

    #[test]
    fn select_without_name_picks_only_copilot_profile() {
        let config = config(&[("openai", None), ("copilot", Some("github-copilot"))]);

        let (name, auth) = select_profile(&config, None).unwrap();

        assert_eq!(name, "copilot");
        assert_eq!(auth.flow, CopilotFlow::Opencode);
    }

    #[test]
    fn select_without_name_and_several_copilot_profiles_lists_them() {
        let config = config(&[
            ("copilot", Some("github-copilot")),
            ("vscode", Some("github-copilot-vscode")),
        ]);

        let err = select_profile(&config, None).unwrap_err();

        assert!(err.contains("copilot, vscode"), "{err}");
        assert!(err.contains("--profile"), "{err}");
    }

    #[test]
    fn select_without_name_and_no_copilot_profile_fails() {
        let config = config(&[("openai", None)]);

        let err = select_profile(&config, None).unwrap_err();

        assert!(err.contains("AUTH"), "{err}");
    }

    #[test]
    fn select_by_name_rejects_unknown_and_non_copilot_profiles() {
        let config = config(&[("openai", None), ("copilot", Some("github-copilot"))]);

        let (name, _) = select_profile(&config, Some("copilot")).unwrap();
        let unknown = select_profile(&config, Some("missing")).unwrap_err();
        let plain = select_profile(&config, Some("openai")).unwrap_err();

        assert_eq!(name, "copilot");
        assert!(
            unknown.contains("missing") && unknown.contains("copilot"),
            "{unknown}"
        );
        assert!(plain.contains("LLM_HUB_OPENAI_AUTH"), "{plain}");
    }
}
