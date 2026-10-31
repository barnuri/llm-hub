use std::net::IpAddr;
use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};

use dashmap::DashMap;
use reqwest::header::{ACCEPT, AUTHORIZATION, USER_AGENT};
use reqwest::{StatusCode, Url};

use crate::configs::{CopilotAuth, CopilotFlow, HubConfig};
use crate::consts::{
    COPILOT_EDITOR_PLUGIN_VERSION, COPILOT_EDITOR_VERSION, COPILOT_TOKEN_REFRESH_MARGIN,
    COPILOT_USER_RETRY, COPILOT_USER_TTL, COPILOT_VSCODE_API_VERSION, COPILOT_VSCODE_USER_AGENT,
    VERSION,
};
use crate::schemas::copilot_token_response::CopilotTokenResponse;
use crate::schemas::copilot_user_response::CopilotUserResponse;
use crate::services::credential_store::CredentialStore;
use crate::services::gh_cli::GhCli;
use crate::services::github_token_resolver;

const EXCHANGE_PATH: &str = "copilot_internal/v2/token";
const USER_PATH: &str = "copilot_internal/user";
const ACCEPT_JSON: &str = "application/json";
const HEADER_EDITOR_VERSION: &str = "editor-version";
const HEADER_EDITOR_PLUGIN_VERSION: &str = "editor-plugin-version";
const HEADER_API_VERSION: &str = "x-github-api-version";
const LOCALHOST: &str = "localhost";
const COPILOT_API_HOST: &str = "githubcopilot.com";
const GHE_DOMAIN_SUFFIX: &str = ".ghe.com";
const GHE_COPILOT_API_PREFIX: &str = "copilot-api.";

/// Each Copilot profile's bearer and API base, shared by every request so the
/// GitHub token lookup and the Copilot handshake run once per session.
pub struct CopilotSessionCache {
    credentials: Arc<dyn CredentialStore>,
    gh: Arc<dyn GhCli>,
    entries: DashMap<String, Arc<SessionEntry>>,
}

/// One profile's slot. The async mutex makes concurrent requests share one acquire.
struct SessionEntry {
    auth: CopilotAuth,
    session: tokio::sync::Mutex<Option<Session>>,
}

/// Never derive `Debug` here: `bearer` is a secret.
struct Session {
    bearer: String,
    api_base: Option<String>,
    refresh_at_secs: u64,
}

impl CopilotSessionCache {
    pub fn new(credentials: Arc<dyn CredentialStore>, gh: Arc<dyn GhCli>) -> CopilotSessionCache {
        CopilotSessionCache {
            credentials,
            gh,
            entries: DashMap::new(),
        }
    }

    /// Returns the bearer for `profile` and the Copilot API base it reported,
    /// or `None` for the base when Copilot reported none.
    ///
    /// # Errors
    /// A message for the user when no GitHub token is available, when GitHub
    /// denies Copilot access, or when Copilot reports an unsafe API endpoint.
    pub async fn get(
        &self,
        client: &reqwest::Client,
        profile: &str,
        auth: &CopilotAuth,
    ) -> Result<(String, Option<String>), String> {
        self.get_at(client, profile, auth, unix_now_secs()).await
    }

    /// [`Self::get`] with the clock passed in as unix seconds.
    pub(crate) async fn get_at(
        &self,
        client: &reqwest::Client,
        profile: &str,
        auth: &CopilotAuth,
        now_secs: u64,
    ) -> Result<(String, Option<String>), String> {
        let entry = self.entry_for(profile, auth);
        let mut slot = entry.session.lock().await;
        if let Some(session) = slot.as_ref().filter(|s| now_secs < s.refresh_at_secs) {
            return Ok((session.bearer.clone(), session.api_base.clone()));
        }
        let github_token = github_token_resolver::resolve(
            profile,
            auth,
            self.gh.as_ref(),
            Arc::clone(&self.credentials),
        )
        .await?;
        let session = match auth.flow {
            CopilotFlow::Vscode => exchange(client, profile, auth, &github_token, now_secs).await?,
            CopilotFlow::Opencode => {
                lookup_user(client, profile, auth, github_token, now_secs).await?
            }
        };
        let result = (session.bearer.clone(), session.api_base.clone());
        *slot = Some(session);
        Ok(result)
    }

    /// Drops the profile's session when it still holds `rejected_bearer`, so the
    /// next [`Self::get`] fetches a fresh GitHub token and re-acquires. A bearer
    /// another request has already replaced is left alone.
    pub async fn invalidate(&self, profile: &str, rejected_bearer: &str) {
        let Some(entry) = self.entries.get(profile).map(|e| Arc::clone(e.value())) else {
            return;
        };
        let mut slot = entry.session.lock().await;
        if slot.as_ref().is_some_and(|s| s.bearer == rejected_bearer) {
            *slot = None;
        }
    }

    /// Drops the sessions of profiles that `config` removed or whose Copilot auth changed.
    pub fn reconcile(&self, config: &HubConfig) {
        self.entries.retain(|profile, entry| {
            config.profile(profile).and_then(|p| p.copilot.as_ref()) == Some(&entry.auth)
        });
    }

    fn entry_for(&self, profile: &str, auth: &CopilotAuth) -> Arc<SessionEntry> {
        let mut slot = self
            .entries
            .entry(profile.to_string())
            .or_insert_with(|| Arc::new(SessionEntry::new(auth)));
        if slot.auth != *auth {
            *slot.value_mut() = Arc::new(SessionEntry::new(auth));
        }
        Arc::clone(slot.value())
    }
}

impl SessionEntry {
    fn new(auth: &CopilotAuth) -> SessionEntry {
        SessionEntry {
            auth: auth.clone(),
            session: tokio::sync::Mutex::new(None),
        }
    }
}

/// Exchanges the GitHub token for a short-lived Copilot token.
async fn exchange(
    client: &reqwest::Client,
    profile: &str,
    auth: &CopilotAuth,
    github_token: &str,
    now_secs: u64,
) -> Result<Session, String> {
    let response = client
        .get(format!("{}/{EXCHANGE_PATH}", auth.github_api_url))
        .header(AUTHORIZATION, format!("token {github_token}"))
        .header(ACCEPT, ACCEPT_JSON)
        .header(HEADER_EDITOR_VERSION, COPILOT_EDITOR_VERSION)
        .header(HEADER_EDITOR_PLUGIN_VERSION, COPILOT_EDITOR_PLUGIN_VERSION)
        .header(USER_AGENT, COPILOT_VSCODE_USER_AGENT)
        .header(HEADER_API_VERSION, COPILOT_VSCODE_API_VERSION)
        .send()
        .await
        .map_err(|e| {
            format!("profile {profile}: the Copilot token exchange request failed: {e}")
        })?;
    let status = response.status();
    if is_access_denied(status) {
        return Err(format!(
            "profile {profile}: GitHub refused the Copilot token exchange with status {status}. \
             The login token may be revoked, or the account may have no Copilot access. \
             Run: llm-hub copilot login --profile {profile}"
        ));
    }
    if !status.is_success() {
        return Err(format!(
            "profile {profile}: the Copilot token exchange returned status {status}"
        ));
    }
    // The decode error is dropped on purpose: it can quote the response body, which holds the token.
    let body: CopilotTokenResponse = response.json().await.map_err(|_| {
        format!("profile {profile}: the Copilot token exchange returned a body that is not a token response")
    })?;
    let refresh_at_secs = exchange_refresh_at(&body, now_secs);
    let api_base = reported_api_base(
        profile,
        &auth.github_domain,
        body.endpoints.and_then(|e| e.api),
    )?;
    Ok(Session {
        bearer: body.token,
        api_base,
        refresh_at_secs,
    })
}

/// The earlier of `expires_at` minus the margin and `now + refresh_in`.
fn exchange_refresh_at(body: &CopilotTokenResponse, now_secs: u64) -> u64 {
    let by_expiry = body
        .expires_at
        .saturating_sub(COPILOT_TOKEN_REFRESH_MARGIN.as_secs());
    body.refresh_in.map_or(by_expiry, |refresh_in| {
        by_expiry.min(now_secs.saturating_add(refresh_in))
    })
}

/// Uses the GitHub token as the bearer and reads the API base from
/// `copilot_internal/user`. A lookup that fails for any reason other than
/// denied access keeps the bearer, leaves the base unset and retries sooner.
async fn lookup_user(
    client: &reqwest::Client,
    profile: &str,
    auth: &CopilotAuth,
    github_token: String,
    now_secs: u64,
) -> Result<Session, String> {
    let (api_base, ttl) = match fetch_user(client, profile, auth, &github_token).await? {
        Some(user) => {
            if let Some(reason) = user.entitlement_error() {
                return Err(format!("profile {profile}: {reason}"));
            }
            let api_base = reported_api_base(
                profile,
                &auth.github_domain,
                user.endpoints.and_then(|e| e.api),
            )?;
            (api_base, COPILOT_USER_TTL)
        }
        None => (None, COPILOT_USER_RETRY),
    };
    Ok(Session {
        bearer: github_token,
        api_base,
        refresh_at_secs: now_secs.saturating_add(ttl.as_secs()),
    })
}

/// `Ok(None)` when the lookup failed in a way that must not deny access.
pub(crate) async fn fetch_user(
    client: &reqwest::Client,
    profile: &str,
    auth: &CopilotAuth,
    github_token: &str,
) -> Result<Option<CopilotUserResponse>, String> {
    // A gh token is rejected when X-GitHub-Api-Version is sent, so only these three go out.
    let sent = client
        .get(format!("{}/{USER_PATH}", auth.github_api_url))
        .header(ACCEPT, ACCEPT_JSON)
        .header(AUTHORIZATION, format!("Bearer {github_token}"))
        .header(USER_AGENT, format!("llm-hub/{VERSION}"))
        .send()
        .await;
    let response = match sent {
        Ok(response) => response,
        Err(e) => {
            tracing::warn!("profile {profile}: the Copilot user lookup failed: {e}");
            return Ok(None);
        }
    };
    let status = response.status();
    if is_access_denied(status) {
        return Err(format!(
            "profile {profile}: GitHub refused the Copilot user lookup with status {status}. \
             The GitHub token may be revoked, or the account may have no Copilot access"
        ));
    }
    if !status.is_success() {
        tracing::warn!("profile {profile}: the Copilot user lookup returned status {status}");
        return Ok(None);
    }
    let user = response.json::<CopilotUserResponse>().await.ok();
    if user.is_none() {
        tracing::warn!("profile {profile}: the Copilot user lookup returned an unreadable body");
    }
    Ok(user)
}

fn is_access_denied(status: StatusCode) -> bool {
    status == StatusCode::UNAUTHORIZED || status == StatusCode::FORBIDDEN
}

/// The reported `endpoints.api` without its trailing `/`.
///
/// # Errors
/// When the endpoint is not a URL, or is neither http(s) to a loopback address
/// nor https to a Copilot API host for `github_domain`.
fn reported_api_base(
    profile: &str,
    github_domain: &str,
    api: Option<String>,
) -> Result<Option<String>, String> {
    let Some(api) = api else {
        return Ok(None);
    };
    let trimmed = api.trim_end_matches('/');
    let url = Url::parse(trimmed).map_err(|_| {
        format!("profile {profile}: Copilot reported an API endpoint that is not a URL: {api}")
    })?;
    let host = url.host_str().unwrap_or_default().to_ascii_lowercase();
    if matches!(url.scheme(), "http" | "https") && is_loopback_host(&host) {
        return Ok(Some(trimmed.to_string()));
    }
    if url.scheme() != "https" {
        return Err(format!(
            "profile {profile}: Copilot reported the API endpoint {api}. \
             The hub sends a bearer only over https, or over http to a loopback address"
        ));
    }
    if !is_copilot_api_host(&host, github_domain) {
        return Err(format!(
            "profile {profile}: Copilot reported the API endpoint {api}. \
             The hub does not send a bearer to the host {host}. \
             Allowed hosts are {COPILOT_API_HOST} and its subdomains, \
             or {GHE_COPILOT_API_PREFIX}<tenant>{GHE_DOMAIN_SUFFIX} for a GHE.com domain"
        ));
    }
    Ok(Some(trimmed.to_string()))
}

/// `host` must be lowercase. `github_domain` is matched without case.
fn is_copilot_api_host(host: &str, github_domain: &str) -> bool {
    let is_github_copilot = host == COPILOT_API_HOST
        || host
            .strip_suffix(COPILOT_API_HOST)
            .is_some_and(|prefix| prefix.ends_with('.'));
    let github_domain = github_domain.to_ascii_lowercase();
    let is_ghe_tenant = github_domain.ends_with(GHE_DOMAIN_SUFFIX)
        && host.strip_prefix(GHE_COPILOT_API_PREFIX) == Some(github_domain.as_str());
    is_github_copilot || is_ghe_tenant
}

fn is_loopback_host(host: &str) -> bool {
    if host.eq_ignore_ascii_case(LOCALHOST) {
        return true;
    }
    host.trim_start_matches('[')
        .trim_end_matches(']')
        .parse::<IpAddr>()
        .is_ok_and(|ip| ip.is_loopback())
}

fn unix_now_secs() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |elapsed| elapsed.as_secs())
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;

    use serde_json::json;
    use wiremock::matchers::{header, method, path};
    use wiremock::{Mock, MockServer, Request, ResponseTemplate};

    use super::*;
    use crate::configs::GithubTokenSource;
    use crate::services::fake_gh_cli::FakeGhCli;
    use crate::services::gh_cli::GhOutcome;
    use crate::services::memory_credential_store::MemoryCredentialStore;

    const PROFILE: &str = "copilot";
    const GITHUB_DOMAIN: &str = "github.com";
    const GH_TOKEN: &str = "gho_from_gh";
    const LOGIN_TOKEN: &str = "gho_from_login";
    const COPILOT_TOKEN: &str = "tid=1;exp=2;sku=business";
    const BUSINESS_API: &str = "https://api.business.githubcopilot.com";
    const NOW: u64 = 1_761_930_000;
    const EXCHANGE_URL_PATH: &str = "/copilot_internal/v2/token";
    const USER_URL_PATH: &str = "/copilot_internal/user";

    fn auth(flow: CopilotFlow, token_source: GithubTokenSource, api_url: &str) -> CopilotAuth {
        CopilotAuth {
            flow,
            token_source,
            github_domain: "github.com".to_string(),
            github_api_url: api_url.to_string(),
            gh_path: "gh".to_string(),
            gh_user: None,
        }
    }

    fn vscode_auth(server: &MockServer) -> CopilotAuth {
        auth(CopilotFlow::Vscode, GithubTokenSource::Login, &server.uri())
    }

    fn opencode_auth(server: &MockServer) -> CopilotAuth {
        auth(
            CopilotFlow::Opencode,
            GithubTokenSource::Auto,
            &server.uri(),
        )
    }

    fn cache_with(auth: &CopilotAuth, gh: Arc<FakeGhCli>) -> CopilotSessionCache {
        let store = MemoryCredentialStore::default();
        store
            .set(
                &auth.flow.credential_account(&auth.github_domain),
                LOGIN_TOKEN,
            )
            .unwrap();
        CopilotSessionCache::new(Arc::new(store), gh)
    }

    fn vscode_cache(auth: &CopilotAuth) -> CopilotSessionCache {
        cache_with(auth, Arc::new(FakeGhCli::new(GhOutcome::NotInstalled)))
    }

    fn token_body(expires_at: u64, refresh_in: u64) -> serde_json::Value {
        json!({
            "token": COPILOT_TOKEN,
            "expires_at": expires_at,
            "refresh_in": refresh_in,
            "endpoints": {"api": format!("{BUSINESS_API}/")},
        })
    }

    async fn mount_exchange(server: &MockServer, body: serde_json::Value, calls: u64) {
        Mock::given(method("GET"))
            .and(path(EXCHANGE_URL_PATH))
            .respond_with(ResponseTemplate::new(200).set_body_json(body))
            .expect(calls)
            .mount(server)
            .await;
    }

    async fn mount_user(server: &MockServer, response: ResponseTemplate, calls: u64) {
        Mock::given(method("GET"))
            .and(path(USER_URL_PATH))
            .respond_with(response)
            .expect(calls)
            .mount(server)
            .await;
    }

    fn business_user() -> ResponseTemplate {
        ResponseTemplate::new(200).set_body_json(json!({
            "chat_enabled": true,
            "endpoints": {"api": BUSINESS_API},
        }))
    }

    #[tokio::test]
    async fn vscode_exchanges_once_for_two_calls() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path(EXCHANGE_URL_PATH))
            .and(header(
                "authorization",
                format!("token {LOGIN_TOKEN}").as_str(),
            ))
            .and(header("editor-version", COPILOT_EDITOR_VERSION))
            .and(header("x-github-api-version", COPILOT_VSCODE_API_VERSION))
            .respond_with(ResponseTemplate::new(200).set_body_json(token_body(NOW + 1800, 1500)))
            .expect(1)
            .mount(&server)
            .await;
        let auth = vscode_auth(&server);
        let cache = vscode_cache(&auth);
        let client = reqwest::Client::new();

        let first = cache.get_at(&client, PROFILE, &auth, NOW).await.unwrap();
        let second = cache
            .get_at(&client, PROFILE, &auth, NOW + 10)
            .await
            .unwrap();

        let expected = (COPILOT_TOKEN.to_string(), Some(BUSINESS_API.to_string()));
        assert_eq!(first, expected);
        assert_eq!(second, expected);
    }

    #[tokio::test]
    async fn concurrent_calls_share_one_exchange() {
        let server = MockServer::start().await;
        mount_exchange(&server, token_body(NOW + 1800, 1500), 1).await;
        let auth = vscode_auth(&server);
        let cache = vscode_cache(&auth);
        let client = reqwest::Client::new();

        let (first, second) = tokio::join!(
            cache.get_at(&client, PROFILE, &auth, NOW),
            cache.get_at(&client, PROFILE, &auth, NOW),
        );

        assert_eq!(first.unwrap(), second.unwrap());
    }

    #[tokio::test]
    async fn vscode_re_exchanges_at_refresh_in() {
        let server = MockServer::start().await;
        mount_exchange(&server, token_body(NOW + 3600, 1500), 2).await;
        let auth = vscode_auth(&server);
        let cache = vscode_cache(&auth);
        let client = reqwest::Client::new();

        for now in [NOW, NOW + 1499, NOW + 1500] {
            cache.get_at(&client, PROFILE, &auth, now).await.unwrap();
        }
    }

    #[test]
    fn refresh_at_is_the_earlier_of_margin_and_refresh_in() {
        let body = |expires_at: u64, refresh_in: Option<u64>| CopilotTokenResponse {
            token: COPILOT_TOKEN.to_string(),
            expires_at,
            refresh_in,
            endpoints: None,
        };
        let margin = COPILOT_TOKEN_REFRESH_MARGIN.as_secs();

        assert_eq!(
            exchange_refresh_at(&body(NOW + 3600, Some(1500)), NOW),
            NOW + 1500
        );
        assert_eq!(
            exchange_refresh_at(&body(NOW + 600, Some(1500)), NOW),
            NOW + 600 - margin
        );
        assert_eq!(
            exchange_refresh_at(&body(NOW + 3600, None), NOW),
            NOW + 3600 - margin
        );
    }

    #[tokio::test]
    async fn invalidate_with_current_bearer_re_exchanges() {
        let server = MockServer::start().await;
        mount_exchange(&server, token_body(NOW + 1800, 1500), 2).await;
        let auth = vscode_auth(&server);
        let cache = vscode_cache(&auth);
        let client = reqwest::Client::new();

        cache.get_at(&client, PROFILE, &auth, NOW).await.unwrap();
        cache.invalidate(PROFILE, COPILOT_TOKEN).await;
        cache.get_at(&client, PROFILE, &auth, NOW).await.unwrap();
    }

    #[tokio::test]
    async fn invalidate_with_stale_bearer_keeps_the_session() {
        let server = MockServer::start().await;
        mount_exchange(&server, token_body(NOW + 1800, 1500), 1).await;
        let auth = vscode_auth(&server);
        let cache = vscode_cache(&auth);
        let client = reqwest::Client::new();

        cache.get_at(&client, PROFILE, &auth, NOW).await.unwrap();
        cache.invalidate(PROFILE, "an_older_bearer").await;
        cache.get_at(&client, PROFILE, &auth, NOW).await.unwrap();
    }

    #[tokio::test]
    async fn vscode_exchange_403_names_the_login_command() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path(EXCHANGE_URL_PATH))
            .respond_with(ResponseTemplate::new(403).set_body_string("denied"))
            .mount(&server)
            .await;
        let auth = vscode_auth(&server);
        let cache = vscode_cache(&auth);

        let err = cache
            .get_at(&reqwest::Client::new(), PROFILE, &auth, NOW)
            .await
            .unwrap_err();

        assert!(err.contains("403"), "{err}");
        assert!(
            err.contains("llm-hub copilot login --profile copilot"),
            "{err}"
        );
        assert!(!err.contains(LOGIN_TOKEN), "{err}");
    }

    #[tokio::test]
    async fn vscode_exchange_500_reports_the_status_only() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path(EXCHANGE_URL_PATH))
            .respond_with(ResponseTemplate::new(500).set_body_string("internal detail"))
            .mount(&server)
            .await;
        let auth = vscode_auth(&server);
        let cache = vscode_cache(&auth);

        let err = cache
            .get_at(&reqwest::Client::new(), PROFILE, &auth, NOW)
            .await
            .unwrap_err();

        assert!(err.contains("500"), "{err}");
        assert!(!err.contains("internal detail"), "{err}");
    }

    #[tokio::test]
    async fn opencode_uses_the_gh_token_and_caches_the_user_base_for_the_ttl() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path(USER_URL_PATH))
            .and(header(
                "authorization",
                format!("Bearer {GH_TOKEN}").as_str(),
            ))
            .and(|request: &Request| !request.headers.contains_key(HEADER_API_VERSION))
            .respond_with(business_user())
            .expect(2)
            .mount(&server)
            .await;
        let auth = opencode_auth(&server);
        let gh = Arc::new(FakeGhCli::new(GhOutcome::Token(GH_TOKEN.to_string())));
        let cache = cache_with(&auth, Arc::clone(&gh));
        let client = reqwest::Client::new();
        let ttl = COPILOT_USER_TTL.as_secs();

        let first = cache.get_at(&client, PROFILE, &auth, NOW).await.unwrap();
        cache
            .get_at(&client, PROFILE, &auth, NOW + ttl - 1)
            .await
            .unwrap();
        cache
            .get_at(&client, PROFILE, &auth, NOW + ttl)
            .await
            .unwrap();

        assert_eq!(
            first,
            (GH_TOKEN.to_string(), Some(BUSINESS_API.to_string()))
        );
        assert_eq!(gh.calls(), 2);
    }

    #[tokio::test]
    async fn opencode_login_source_uses_the_stored_token() {
        let server = MockServer::start().await;
        mount_user(&server, business_user(), 1).await;
        let auth = auth(
            CopilotFlow::Opencode,
            GithubTokenSource::Login,
            &server.uri(),
        );
        let cache = cache_with(&auth, Arc::new(FakeGhCli::new(GhOutcome::NotInstalled)));

        let (bearer, _) = cache
            .get_at(&reqwest::Client::new(), PROFILE, &auth, NOW)
            .await
            .unwrap();

        assert_eq!(bearer, LOGIN_TOKEN);
    }

    #[tokio::test]
    async fn opencode_chat_disabled_is_an_error() {
        let server = MockServer::start().await;
        let disabled = ResponseTemplate::new(200).set_body_json(json!({"chat_enabled": false}));
        mount_user(&server, disabled, 1).await;
        let auth = opencode_auth(&server);
        let cache = cache_with(
            &auth,
            Arc::new(FakeGhCli::new(GhOutcome::Token(GH_TOKEN.to_string()))),
        );

        let err = cache
            .get_at(&reqwest::Client::new(), PROFILE, &auth, NOW)
            .await
            .unwrap_err();

        assert!(err.contains("does not have GitHub Copilot access"), "{err}");
    }

    #[tokio::test]
    async fn opencode_user_401_is_an_error() {
        let server = MockServer::start().await;
        mount_user(&server, ResponseTemplate::new(401), 1).await;
        let auth = opencode_auth(&server);
        let cache = cache_with(
            &auth,
            Arc::new(FakeGhCli::new(GhOutcome::Token(GH_TOKEN.to_string()))),
        );

        let err = cache
            .get_at(&reqwest::Client::new(), PROFILE, &auth, NOW)
            .await
            .unwrap_err();

        assert!(err.contains("401"), "{err}");
        assert!(!err.contains(GH_TOKEN), "{err}");
    }

    #[tokio::test]
    async fn opencode_user_500_falls_back_and_retries_after_the_retry_delay() {
        let server = MockServer::start().await;
        mount_user(&server, ResponseTemplate::new(500), 2).await;
        let auth = opencode_auth(&server);
        let cache = cache_with(
            &auth,
            Arc::new(FakeGhCli::new(GhOutcome::Token(GH_TOKEN.to_string()))),
        );
        let client = reqwest::Client::new();
        let retry = COPILOT_USER_RETRY.as_secs();

        let first = cache.get_at(&client, PROFILE, &auth, NOW).await.unwrap();
        cache
            .get_at(&client, PROFILE, &auth, NOW + retry - 1)
            .await
            .unwrap();
        cache
            .get_at(&client, PROFILE, &auth, NOW + retry)
            .await
            .unwrap();

        assert_eq!(first, (GH_TOKEN.to_string(), None));
    }

    #[tokio::test]
    async fn opencode_invalidate_asks_gh_again() {
        let server = MockServer::start().await;
        mount_user(&server, business_user(), 2).await;
        let auth = opencode_auth(&server);
        let gh = Arc::new(FakeGhCli::new(GhOutcome::Token(GH_TOKEN.to_string())));
        let cache = cache_with(&auth, Arc::clone(&gh));
        let client = reqwest::Client::new();

        cache.get_at(&client, PROFILE, &auth, NOW).await.unwrap();
        cache.invalidate(PROFILE, GH_TOKEN).await;
        cache.get_at(&client, PROFILE, &auth, NOW).await.unwrap();

        assert_eq!(gh.calls(), 2);
    }

    #[tokio::test]
    async fn both_flows_reject_a_remote_http_endpoint() {
        let insecure = "http://api.githubcopilot.example";
        let server = MockServer::start().await;
        mount_exchange(
            &server,
            json!({"token": COPILOT_TOKEN, "expires_at": NOW + 1800, "endpoints": {"api": insecure}}),
            1,
        )
        .await;
        let user = ResponseTemplate::new(200)
            .set_body_json(json!({"chat_enabled": true, "endpoints": {"api": insecure}}));
        mount_user(&server, user, 1).await;
        let client = reqwest::Client::new();

        for auth in [vscode_auth(&server), opencode_auth(&server)] {
            let cache = cache_with(
                &auth,
                Arc::new(FakeGhCli::new(GhOutcome::Token(GH_TOKEN.to_string()))),
            );
            let err = cache
                .get_at(&client, PROFILE, &auth, NOW)
                .await
                .unwrap_err();
            assert!(err.contains(insecure), "{:?}: {err}", auth.flow);
        }
    }

    #[test]
    fn reported_api_base_accepts_copilot_hosts_and_loopback() {
        for (api, expected) in [
            (
                "https://api.githubcopilot.com/",
                "https://api.githubcopilot.com",
            ),
            ("https://githubcopilot.com", "https://githubcopilot.com"),
            (
                "https://API.Business.GithubCopilot.com",
                "https://API.Business.GithubCopilot.com",
            ),
            ("http://127.0.0.1:4141/", "http://127.0.0.1:4141"),
            ("http://localhost:4141", "http://localhost:4141"),
            ("http://[::1]:4141", "http://[::1]:4141"),
            ("https://127.0.0.1:4141", "https://127.0.0.1:4141"),
        ] {
            assert_eq!(
                reported_api_base(PROFILE, GITHUB_DOMAIN, Some(api.to_string())),
                Ok(Some(expected.to_string())),
                "{api}"
            );
        }
        assert_eq!(reported_api_base(PROFILE, GITHUB_DOMAIN, None), Ok(None));
    }

    #[test]
    fn reported_api_base_rejects_remote_http_and_non_urls() {
        for api in [
            "http://10.0.0.1",
            "http://api.githubcopilot.com",
            "ftp://api.githubcopilot.com",
            "not a url",
        ] {
            assert!(
                reported_api_base(PROFILE, GITHUB_DOMAIN, Some(api.to_string())).is_err(),
                "{api}"
            );
        }
    }

    #[test]
    fn reported_api_base_rejects_other_https_hosts_and_names_the_host() {
        for (api, host) in [
            ("https://evil.example/v1", "evil.example"),
            ("https://evilgithubcopilot.com", "evilgithubcopilot.com"),
            (
                "https://githubcopilot.com.evil.example",
                "githubcopilot.com.evil.example",
            ),
            (
                "https://copilot-api.acme.ghe.com",
                "copilot-api.acme.ghe.com",
            ),
            ("https://10.0.0.1", "10.0.0.1"),
        ] {
            let err = reported_api_base(PROFILE, GITHUB_DOMAIN, Some(api.to_string())).unwrap_err();
            assert!(err.contains(&format!("the host {host}")), "{api}: {err}");
        }
    }

    #[test]
    fn reported_api_base_accepts_only_the_own_ghe_tenant_host() {
        let ghe_domain = "acme.ghe.com";
        for api in [
            "https://copilot-api.acme.ghe.com",
            "https://Copilot-API.Acme.GHE.com/",
            "https://api.githubcopilot.com",
        ] {
            assert!(
                reported_api_base(PROFILE, ghe_domain, Some(api.to_string())).is_ok(),
                "{api}"
            );
        }
        for api in [
            "https://copilot-api.other.ghe.com",
            "https://api.acme.ghe.com",
            "https://copilot-api.acme.ghe.com.evil.example",
        ] {
            assert!(
                reported_api_base(PROFILE, ghe_domain, Some(api.to_string())).is_err(),
                "{api}"
            );
        }
        assert!(
            reported_api_base(
                PROFILE,
                "github.acme.com",
                Some("https://copilot-api.github.acme.com".to_string())
            )
            .is_err()
        );
    }

    #[tokio::test]
    async fn both_flows_reject_an_https_endpoint_outside_the_allowlist() {
        let foreign = "https://copilot.evil.example";
        let server = MockServer::start().await;
        mount_exchange(
            &server,
            json!({"token": COPILOT_TOKEN, "expires_at": NOW + 1800, "endpoints": {"api": foreign}}),
            1,
        )
        .await;
        let user = ResponseTemplate::new(200)
            .set_body_json(json!({"chat_enabled": true, "endpoints": {"api": foreign}}));
        mount_user(&server, user, 1).await;
        let client = reqwest::Client::new();

        for auth in [vscode_auth(&server), opencode_auth(&server)] {
            let cache = cache_with(
                &auth,
                Arc::new(FakeGhCli::new(GhOutcome::Token(GH_TOKEN.to_string()))),
            );
            let err = cache
                .get_at(&client, PROFILE, &auth, NOW)
                .await
                .unwrap_err();
            assert!(
                err.contains("copilot.evil.example"),
                "{:?}: {err}",
                auth.flow
            );
        }
    }

    #[tokio::test]
    async fn reconcile_keeps_only_profiles_with_unchanged_auth() {
        let server = MockServer::start().await;
        mount_user(&server, business_user(), 3).await;
        let auth = opencode_auth(&server);
        let cache = cache_with(
            &auth,
            Arc::new(FakeGhCli::new(GhOutcome::Token(GH_TOKEN.to_string()))),
        );
        let client = reqwest::Client::new();
        for profile in ["kept", "changed", "removed"] {
            cache.get_at(&client, profile, &auth, NOW).await.unwrap();
        }
        let vars: HashMap<String, String> = [
            ("LLM_HUB_KEPT_BASE_URL", BUSINESS_API),
            ("LLM_HUB_CHANGED_BASE_URL", BUSINESS_API),
        ]
        .into_iter()
        .map(|(k, v)| (k.to_string(), v.to_string()))
        .collect();
        let mut config = HubConfig::from_map(&vars).unwrap();
        for profile in &mut config.profiles {
            let mut profile_auth = auth.clone();
            if profile.name == "changed" {
                profile_auth.gh_user = Some("someone-else".to_string());
            }
            profile.copilot = Some(profile_auth);
        }

        cache.reconcile(&config);

        let mut remaining: Vec<String> = cache.entries.iter().map(|e| e.key().clone()).collect();
        remaining.sort();
        assert_eq!(remaining, vec!["kept".to_string()]);
    }
}
