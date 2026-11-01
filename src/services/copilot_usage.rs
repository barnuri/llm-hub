//! Each Copilot profile's quota from `copilot_internal/user`, cached and
//! normalised to the quota shape the Claude Code status line reads.

use std::sync::{Arc, Mutex, MutexGuard, PoisonError};

use dashmap::DashMap;
use futures_util::FutureExt;
use futures_util::future::{BoxFuture, Shared};
use serde::Serialize;

use crate::configs::{CopilotAuth, CopilotFlow, HubConfig};
use crate::consts::COPILOT_USAGE_TTL;
use crate::dependencies::state::AppState;
use crate::schemas::copilot_quota_snapshot::CopilotQuotaSnapshot;
use crate::schemas::copilot_user_response::CopilotUserResponse;
use crate::services::copilot_sessions::fetch_user;
use crate::services::credential_store::CredentialStore;
use crate::services::gh_cli::GhCli;
use crate::services::github_token_resolver;
use crate::services::store::now_ms;
use crate::utils::time::utc_iso8601;

const USAGE_SOURCE: &str = "copilot_internal/user";
const AIC_PER_USD: f64 = 100.0;
const FULL_PERCENT: f64 = 100.0;
const DATE_ONLY_LEN: usize = "2026-11-01".len();
const MIDNIGHT_UTC: &str = "T00:00:00Z";

type PendingRefresh = Shared<BoxFuture<'static, Option<Arc<CopilotUsage>>>>;

/// Quota snapshots per Copilot profile, served stale-while-revalidate so a
/// poller never waits on GitHub once a first snapshot exists.
pub struct CopilotUsageCache {
    credentials: Arc<dyn CredentialStore>,
    gh: Arc<dyn GhCli>,
    entries: DashMap<String, Arc<UsageEntry>>,
}

struct UsageEntry {
    auth: CopilotAuth,
    snapshot: Mutex<Option<Arc<CopilotUsage>>>,
    /// The refresh in flight. Every caller that needs it awaits the same task.
    refresh: Mutex<Option<PendingRefresh>>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum QuotaUnit {
    /// AI credits: 100 AIC = $1.
    Aic,
    Requests,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct CopilotUsage {
    source: &'static str,
    /// Fetch time, unix ms.
    updated_at: u64,
    fetched_at: String,
    quotas: Vec<CopilotUsageQuota>,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct CopilotUsageQuota {
    id: String,
    unit: QuotaUnit,
    used: f64,
    entitlement: f64,
    #[serde(skip_serializing_if = "Option::is_none")]
    remaining: Option<f64>,
    remaining_percentage: f64,
    #[serde(skip_serializing_if = "Option::is_none")]
    overage_permitted: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    reset_date: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    used_usd: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    limit_usd: Option<f64>,
}

impl CopilotUsageCache {
    pub fn new(credentials: Arc<dyn CredentialStore>, gh: Arc<dyn GhCli>) -> CopilotUsageCache {
        CopilotUsageCache {
            credentials,
            gh,
            entries: DashMap::new(),
        }
    }

    /// Drops the snapshots of profiles that `config` removed or whose Copilot auth changed.
    pub fn reconcile(&self, config: &HubConfig) {
        self.entries.retain(|profile, entry| {
            config.profile(profile).and_then(|p| p.copilot.as_ref()) == Some(&entry.auth)
        });
    }

    fn entry_for(&self, profile: &str, auth: &CopilotAuth) -> Arc<UsageEntry> {
        let mut slot = self
            .entries
            .entry(profile.to_string())
            .or_insert_with(|| Arc::new(UsageEntry::new(auth)));
        if slot.auth != *auth {
            *slot.value_mut() = Arc::new(UsageEntry::new(auth));
        }
        Arc::clone(slot.value())
    }
}

impl UsageEntry {
    fn new(auth: &CopilotAuth) -> UsageEntry {
        UsageEntry {
            auth: auth.clone(),
            snapshot: Mutex::new(None),
            refresh: Mutex::new(None),
        }
    }
}

/// Returns `profile`'s quota and refreshes a stale snapshot in the background.
/// Returns `None` when no snapshot exists and the fetch failed.
pub async fn usage(
    state: &AppState,
    client: &reqwest::Client,
    profile: &str,
    auth: &CopilotAuth,
) -> Option<Arc<CopilotUsage>> {
    usage_at(state, client, profile, auth, now_ms()).await
}

/// [`usage`] with the clock passed in as unix milliseconds.
pub(crate) async fn usage_at(
    state: &AppState,
    client: &reqwest::Client,
    profile: &str,
    auth: &CopilotAuth,
    now_ms: u64,
) -> Option<Arc<CopilotUsage>> {
    let entry = state.copilot_usage().entry_for(profile, auth);
    let cached = lock(&entry.snapshot).clone();
    let ttl_ms = u64::try_from(COPILOT_USAGE_TTL.as_millis()).unwrap_or(u64::MAX);
    if let Some(usage) = &cached
        && now_ms.saturating_sub(usage.updated_at) < ttl_ms
    {
        return cached;
    }
    let pending = start_refresh(state, client, profile, &entry, now_ms);
    match cached {
        Some(usage) => Some(usage),
        None => pending.await,
    }
}

/// The refresh already in flight for `entry`, or a newly spawned one.
fn start_refresh(
    state: &AppState,
    client: &reqwest::Client,
    profile: &str,
    entry: &Arc<UsageEntry>,
    now_ms: u64,
) -> PendingRefresh {
    let mut slot = lock(&entry.refresh);
    if let Some(pending) = slot.as_ref() {
        return pending.clone();
    }
    let task = tokio::spawn(refresh(
        state.clone(),
        client.clone(),
        profile.to_string(),
        Arc::clone(entry),
        now_ms,
    ));
    let pending = task.map(|joined| joined.ok().flatten()).boxed().shared();
    *slot = Some(pending.clone());
    pending
}

/// Fetches and stores a new snapshot. A failed fetch keeps the old one.
async fn refresh(
    state: AppState,
    client: reqwest::Client,
    profile: String,
    entry: Arc<UsageEntry>,
    now_ms: u64,
) -> Option<Arc<CopilotUsage>> {
    let usage = fetch(&state, &client, &profile, &entry.auth, now_ms)
        .await
        .map(Arc::new);
    if let Some(usage) = &usage {
        *lock(&entry.snapshot) = Some(Arc::clone(usage));
    }
    *lock(&entry.refresh) = None;
    usage
}

/// Flow B reuses the session bearer, which is the GitHub token. Flow A's
/// session holds the exchanged Copilot token, so the GitHub token is resolved again.
async fn fetch(
    state: &AppState,
    client: &reqwest::Client,
    profile: &str,
    auth: &CopilotAuth,
    now_ms: u64,
) -> Option<CopilotUsage> {
    let github_token = match auth.flow {
        CopilotFlow::Opencode => state
            .copilot_sessions()
            .get(client, profile, auth)
            .await
            .map(|(bearer, _)| bearer),
        CopilotFlow::Vscode => {
            let cache = state.copilot_usage();
            github_token_resolver::resolve(
                profile,
                auth,
                cache.gh.as_ref(),
                Arc::clone(&cache.credentials),
            )
            .await
        }
    };
    let github_token = github_token
        .inspect_err(|e| {
            tracing::warn!("profile {profile}: the Copilot usage refresh has no GitHub token: {e}");
        })
        .ok()?;
    match fetch_user(client, profile, auth, &github_token).await {
        Ok(user) => normalise(&user?, now_ms),
        Err(e) => {
            tracing::warn!("{e}");
            if auth.flow == CopilotFlow::Opencode {
                state
                    .copilot_sessions()
                    .invalidate(profile, &github_token)
                    .await;
            }
            None
        }
    }
}

/// The limited quotas in `user`, sorted by id. `None` when it reports none.
fn normalise(user: &CopilotUserResponse, fetched_at_ms: u64) -> Option<CopilotUsage> {
    let reset_date = user
        .quota_reset_date_utc
        .as_deref()
        .or(user.quota_reset_date.as_deref())
        .map(utc_reset_date);
    let mut quotas: Vec<CopilotUsageQuota> = user
        .quota_snapshots
        .as_ref()?
        .iter()
        .filter_map(|(id, snapshot)| normalise_quota(id, snapshot, reset_date.as_deref()))
        .collect();
    if quotas.is_empty() {
        return None;
    }
    quotas.sort_by(|a, b| a.id.cmp(&b.id));
    Some(CopilotUsage {
        source: USAGE_SOURCE,
        updated_at: fetched_at_ms,
        fetched_at: utc_iso8601(fetched_at_ms),
        quotas,
    })
}

/// `credits_used` and `percent_remaining` are kept as reported: the API's
/// `entitlement - remaining` does not match them.
fn normalise_quota(
    id: &str,
    snapshot: &CopilotQuotaSnapshot,
    reset_date: Option<&str>,
) -> Option<CopilotUsageQuota> {
    if snapshot.unlimited == Some(true) {
        return None;
    }
    let entitlement = snapshot.entitlement.filter(|limit| *limit > 0.0)?;
    let used = snapshot
        .credits_used
        .or_else(|| snapshot.remaining.map(|remaining| entitlement - remaining))?;
    let remaining_percentage = snapshot
        .percent_remaining
        .unwrap_or((entitlement - used) / entitlement * FULL_PERCENT)
        .clamp(0.0, FULL_PERCENT);
    let unit = if snapshot.token_based_billing == Some(true) {
        QuotaUnit::Aic
    } else {
        QuotaUnit::Requests
    };
    let in_usd = |amount: f64| (unit == QuotaUnit::Aic).then_some(amount / AIC_PER_USD);
    Some(CopilotUsageQuota {
        id: id.to_string(),
        unit,
        used,
        entitlement,
        remaining: snapshot.remaining,
        remaining_percentage,
        overage_permitted: snapshot.overage_permitted,
        reset_date: reset_date.map(str::to_string),
        used_usd: in_usd(used),
        limit_usd: in_usd(entitlement),
    })
}

/// A date-only value becomes midnight UTC, so every reset date is a full ISO-8601 instant.
fn utc_reset_date(raw: &str) -> String {
    if raw.len() == DATE_ONLY_LEN && !raw.contains('T') {
        format!("{raw}{MIDNIGHT_UTC}")
    } else {
        raw.to_string()
    }
}

fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(PoisonError::into_inner)
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;
    use std::time::Duration;

    use serde_json::{Value, json};
    use wiremock::matchers::{header, method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    use super::*;
    use crate::services::fake_gh_cli::FakeGhCli;
    use crate::services::gh_cli::GhOutcome;
    use crate::services::memory_credential_store::MemoryCredentialStore;

    const PROFILE: &str = "copilot";
    const GH_TOKEN: &str = "gho_from_gh";
    const LOGIN_TOKEN: &str = "gho_from_login";
    const USER_URL_PATH: &str = "/copilot_internal/user";
    const NOW_MS: u64 = 1_793_476_800_000;
    const SLOW_UPSTREAM: Duration = Duration::from_millis(300);

    fn live_user_body() -> Value {
        json!({
            "chat_enabled": true,
            "quota_reset_date": "2026-11-01",
            "quota_reset_date_utc": "2026-11-01T00:00:00Z",
            "quota_snapshots": {
                "chat": {"entitlement": 0, "unlimited": true, "percent_remaining": 100.0},
                "completions": {"entitlement": 0, "unlimited": true, "percent_remaining": 100.0},
                "premium_interactions": {
                    "credits_used": 12626, "entitlement": 33000, "remaining": 20489,
                    "quota_remaining": 20489.9, "percent_remaining": 62.0,
                    "overage_permitted": true, "token_based_billing": true, "unlimited": false,
                },
            },
        })
    }

    fn body_with_used(credits_used: u64) -> Value {
        let mut body = live_user_body();
        body["quota_snapshots"]["premium_interactions"]["credits_used"] = json!(credits_used);
        body
    }

    fn parse(body: Value) -> CopilotUserResponse {
        serde_json::from_value(body).unwrap()
    }

    fn quotas(usage: &CopilotUsage) -> Value {
        serde_json::to_value(usage).unwrap()["quotas"].clone()
    }

    fn state_with(auth_value: &str, api_url: &str) -> (AppState, CopilotAuth) {
        let vars: HashMap<String, String> = [
            ("LLM_HUB_PROFILES", PROFILE),
            ("LLM_HUB_COPILOT_BASE_URL", "http://127.0.0.1:9"),
            ("LLM_HUB_COPILOT_AUTH", auth_value),
            ("LLM_HUB_COPILOT_GITHUB_API_URL", api_url),
        ]
        .into_iter()
        .map(|(key, value)| (key.to_string(), value.to_string()))
        .collect();
        let config = HubConfig::from_map(&vars).unwrap();
        let auth = config.profile(PROFILE).unwrap().copilot.clone().unwrap();
        let store = MemoryCredentialStore::default();
        store
            .set(
                &auth.flow.credential_account(&auth.github_domain),
                LOGIN_TOKEN,
            )
            .unwrap();
        let state = AppState::with_seams(
            config,
            None,
            Arc::new(store),
            Arc::new(FakeGhCli::new(GhOutcome::Token(GH_TOKEN.to_string()))),
        )
        .unwrap();
        (state, auth)
    }

    fn vscode_state(server: &MockServer) -> (AppState, CopilotAuth) {
        state_with("github-copilot-vscode", &server.uri())
    }

    async fn mount_user_once(server: &MockServer, response: ResponseTemplate) {
        Mock::given(method("GET"))
            .and(path(USER_URL_PATH))
            .respond_with(response)
            .up_to_n_times(1)
            .expect(1)
            .mount(server)
            .await;
    }

    async fn settle(state: &AppState, auth: &CopilotAuth) {
        let entry = state.copilot_usage().entry_for(PROFILE, auth);
        let pending = lock(&entry.refresh).clone();
        if let Some(pending) = pending {
            pending.await;
        }
    }

    fn used(usage: &CopilotUsage) -> Value {
        quotas(usage)[0]["used"].clone()
    }

    #[test]
    fn normalise_live_token_billed_seat() {
        let usage = normalise(&parse(live_user_body()), NOW_MS).unwrap();

        assert_eq!(
            serde_json::to_value(&usage).unwrap(),
            json!({
                "source": "copilot_internal/user",
                "updatedAt": NOW_MS,
                "fetchedAt": utc_iso8601(NOW_MS),
                "quotas": [{
                    "id": "premium_interactions",
                    "unit": "aic",
                    "used": 12626.0,
                    "entitlement": 33000.0,
                    "remaining": 20489.0,
                    "remainingPercentage": 62.0,
                    "overagePermitted": true,
                    "resetDate": "2026-11-01T00:00:00Z",
                    "usedUsd": 126.26,
                    "limitUsd": 330.0,
                }],
            })
        );
    }

    #[test]
    fn normalise_request_seat_has_no_usd() {
        let usage = normalise(
            &parse(json!({"quota_snapshots": {"premium_interactions": {
                "entitlement": 300, "remaining": 180, "percent_remaining": 60.0,
            }}})),
            NOW_MS,
        )
        .unwrap();

        assert_eq!(
            quotas(&usage),
            json!([{
                "id": "premium_interactions",
                "unit": "requests",
                "used": 120.0,
                "entitlement": 300.0,
                "remaining": 180.0,
                "remainingPercentage": 60.0,
            }])
        );
    }

    #[test]
    fn normalise_computes_a_missing_percent() {
        let usage = normalise(
            &parse(json!({"quota_snapshots": {"premium_interactions": {
                "entitlement": 300, "credits_used": 75,
            }}})),
            NOW_MS,
        )
        .unwrap();

        assert_eq!(quotas(&usage)[0]["remainingPercentage"], json!(75.0));
    }

    #[test]
    fn normalise_drops_unlimited_and_zero_entitlement_snapshots() {
        let only_unlimited = json!({"quota_snapshots": {
            "chat": {"entitlement": 0, "unlimited": true},
            "completions": {"entitlement": 0, "unlimited": true},
            "premium_interactions": {"entitlement": 0, "remaining": 0},
        }});

        assert_eq!(normalise(&parse(only_unlimited), NOW_MS), None);
        assert_eq!(normalise(&parse(json!({})), NOW_MS), None);
    }

    #[test]
    fn date_only_reset_gets_midnight_utc() {
        let usage = normalise(
            &parse(json!({
                "quota_reset_date": "2026-12-01",
                "quota_snapshots": {"premium_interactions": {"entitlement": 300, "remaining": 300}},
            })),
            NOW_MS,
        )
        .unwrap();

        assert_eq!(
            quotas(&usage)[0]["resetDate"],
            json!("2026-12-01T00:00:00Z")
        );
    }

    #[tokio::test]
    async fn two_calls_inside_the_ttl_fetch_once() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path(USER_URL_PATH))
            .and(header(
                "authorization",
                format!("Bearer {LOGIN_TOKEN}").as_str(),
            ))
            .respond_with(ResponseTemplate::new(200).set_body_json(live_user_body()))
            .expect(1)
            .mount(&server)
            .await;
        let (state, auth) = vscode_state(&server);
        let client = reqwest::Client::new();
        let ttl_ms = u64::try_from(COPILOT_USAGE_TTL.as_millis()).unwrap();

        let first = usage_at(&state, &client, PROFILE, &auth, NOW_MS).await;
        let second = usage_at(&state, &client, PROFILE, &auth, NOW_MS + ttl_ms - 1).await;

        assert_eq!(first.unwrap().updated_at, NOW_MS);
        assert_eq!(second.unwrap().updated_at, NOW_MS);
    }

    #[tokio::test]
    async fn stale_snapshot_is_served_while_one_refresh_runs() {
        let server = MockServer::start().await;
        mount_user_once(
            &server,
            ResponseTemplate::new(200).set_body_json(body_with_used(100)),
        )
        .await;
        mount_user_once(
            &server,
            ResponseTemplate::new(200)
                .set_body_json(body_with_used(200))
                .set_delay(SLOW_UPSTREAM),
        )
        .await;
        Mock::given(method("GET"))
            .and(path(USER_URL_PATH))
            .respond_with(ResponseTemplate::new(200).set_body_json(body_with_used(300)))
            .expect(0)
            .mount(&server)
            .await;
        let (state, auth) = vscode_state(&server);
        let client = reqwest::Client::new();
        let stale_at = NOW_MS + u64::try_from(COPILOT_USAGE_TTL.as_millis()).unwrap();

        usage_at(&state, &client, PROFILE, &auth, NOW_MS).await;
        let (first, second) = tokio::join!(
            usage_at(&state, &client, PROFILE, &auth, stale_at),
            usage_at(&state, &client, PROFILE, &auth, stale_at),
        );
        settle(&state, &auth).await;
        let refreshed = usage_at(&state, &client, PROFILE, &auth, stale_at).await;

        assert_eq!(used(&first.unwrap()), json!(100.0));
        assert_eq!(used(&second.unwrap()), json!(100.0));
        assert_eq!(used(&refreshed.unwrap()), json!(200.0));
    }

    #[tokio::test]
    async fn failed_refresh_keeps_the_old_snapshot() {
        let server = MockServer::start().await;
        mount_user_once(
            &server,
            ResponseTemplate::new(200).set_body_json(live_user_body()),
        )
        .await;
        mount_user_once(&server, ResponseTemplate::new(500)).await;
        let (state, auth) = vscode_state(&server);
        let client = reqwest::Client::new();
        let stale_at = NOW_MS + u64::try_from(COPILOT_USAGE_TTL.as_millis()).unwrap();

        usage_at(&state, &client, PROFILE, &auth, NOW_MS).await;
        usage_at(&state, &client, PROFILE, &auth, stale_at).await;
        settle(&state, &auth).await;
        let entry = state.copilot_usage().entry_for(PROFILE, &auth);
        let kept = lock(&entry.snapshot).clone();

        assert_eq!(kept.unwrap().updated_at, NOW_MS);
    }

    #[tokio::test]
    async fn cold_cache_with_a_failed_fetch_is_none() {
        let server = MockServer::start().await;
        mount_user_once(&server, ResponseTemplate::new(500)).await;
        let (state, auth) = vscode_state(&server);

        let usage = usage_at(&state, &reqwest::Client::new(), PROFILE, &auth, NOW_MS).await;

        assert_eq!(usage, None);
    }

    #[tokio::test]
    async fn opencode_sends_the_session_bearer() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path(USER_URL_PATH))
            .and(header(
                "authorization",
                format!("Bearer {GH_TOKEN}").as_str(),
            ))
            .respond_with(ResponseTemplate::new(200).set_body_json(live_user_body()))
            .expect(2)
            .mount(&server)
            .await;
        // One call acquires the session, the other reads the quota.
        let (state, auth) = state_with("github-copilot", &server.uri());

        let usage = usage_at(&state, &reqwest::Client::new(), PROFILE, &auth, NOW_MS).await;

        assert_eq!(used(&usage.unwrap()), json!(12626.0));
    }

    #[tokio::test]
    async fn reconcile_drops_removed_profiles() {
        let server = MockServer::start().await;
        let (state, auth) = vscode_state(&server);
        state.copilot_usage().entry_for("removed", &auth);
        state.copilot_usage().entry_for(PROFILE, &auth);

        state.copilot_usage().reconcile(&state.config());

        let remaining: Vec<String> = state
            .copilot_usage()
            .entries
            .iter()
            .map(|e| e.key().clone())
            .collect();
        assert_eq!(remaining, vec![PROFILE.to_string()]);
    }
}
