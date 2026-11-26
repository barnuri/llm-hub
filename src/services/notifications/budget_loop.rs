//! The budget tick: provider quotas the hub already holds and its own monthly spend, checked against
//! the operator's steps every five minutes.

use std::collections::HashMap;
use std::time::Duration;

use crate::dependencies::state::AppState;
use crate::schemas::notification::Notification;
use crate::services::notifications::budget::{self, BudgetReading};
use crate::services::store::now_ms;

/// The wait before the first check, so the boot quota poll fills the usage cache first.
const FIRST_RUN_DELAY: Duration = Duration::from_mins(1);
/// The wait between checks.
const INTERVAL: Duration = Duration::from_mins(5);

/// Runs [`run_once`] one minute after boot and every five minutes after that.
pub fn spawn_budget_loop(state: AppState) -> tokio::task::JoinHandle<()> {
    spawn_every(state, FIRST_RUN_DELAY, INTERVAL)
}

fn spawn_every(state: AppState, first: Duration, every: Duration) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        let start = tokio::time::Instant::now() + first;
        let mut ticker = tokio::time::interval_at(start, every);
        ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        loop {
            ticker.tick().await;
            run_once(&state, now_ms()).await;
        }
    })
}

/// Hands every crossed budget step to the notifier and returns what the inbox holds for each.
/// The inbox keeps one notification per id, so a step already raised is never delivered again.
pub async fn run_once(state: &AppState, now_ms: u64) -> Vec<Notification> {
    let config = state.config();
    if !config.notify.enabled {
        return Vec::new();
    }
    let mut readings = quota_readings(state, now_ms);
    readings.extend(spend_readings(state, now_ms).await);
    budget::evaluate(&readings, &config.notify.budget_steps, now_ms)
        .into_iter()
        .filter_map(|notification| state.notifier().notify(notification))
        .collect()
}

/// The quotas of the last usage snapshot of each enabled profile. Never calls an upstream.
fn quota_readings(state: &AppState, now_ms: u64) -> Vec<BudgetReading> {
    let config = state.config();
    config
        .profiles
        .iter()
        .filter(|profile| profile.enabled)
        .filter_map(|profile| {
            let usage = state.profile_usage().cached(&profile.name)?;
            Some(
                usage
                    .quotas
                    .iter()
                    .map(|quota| BudgetReading::from_usage_quota(&profile.name, quota, now_ms))
                    .collect::<Vec<_>>(),
            )
        })
        .flatten()
        .collect()
}

/// The hub's spend this month against the global and per-profile budgets. Reads the store off
/// the runtime, and only when a budget is set.
async fn spend_readings(state: &AppState, now_ms: u64) -> Vec<BudgetReading> {
    let config = state.config();
    let profile_limits: Vec<(String, f64)> = config
        .profiles
        .iter()
        .filter_map(|profile| Some((profile.name.clone(), profile.budget_monthly_usd?)))
        .collect();
    if config.budget_monthly_usd.is_none() && profile_limits.is_empty() {
        return Vec::new();
    }
    let reader = state.clone();
    let since_ms = budget::month_start_ms(now_ms);
    let spend = tokio::task::spawn_blocking(move || {
        let store = reader.store().ok_or("budgets need a store")?;
        store.month_spend_usd(since_ms, &reader.pricing_book())
    })
    .await;
    let spend: HashMap<String, f64> = match spend {
        Ok(Ok(spend)) => spend,
        Ok(Err(error)) => {
            tracing::warn!("budget spend not read: {error}");
            return Vec::new();
        }
        Err(error) => {
            tracing::warn!("budget spend task failed: {error}");
            return Vec::new();
        }
    };
    budget::spend_readings(config.budget_monthly_usd, &profile_limits, &spend, now_ms)
}

#[cfg(test)]
mod tests {
    use std::sync::{Arc, Mutex};

    use futures_util::future::BoxFuture;
    use serde_json::json;
    use wiremock::matchers::{method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    use super::*;
    use crate::configs::HubConfig;
    use crate::schemas::notification::NotificationSeverity;
    use crate::services::credential_store::CredentialStore;
    use crate::services::fake_gh_cli::FakeGhCli;
    use crate::services::gh_cli::GhOutcome;
    use crate::services::memory_credential_store::MemoryCredentialStore;
    use crate::services::notifications::dispatch::Sink;
    use crate::services::profile_usage::{self, UsageKey};
    use crate::services::stats::RequestOutcome;
    use crate::services::store::Store;

    const LOGIN_TOKEN: &str = "gho_from_login";

    fn state_with(vars: &[(&str, &str)], store: Option<Store>) -> AppState {
        let vars: HashMap<String, String> = vars
            .iter()
            .map(|(key, value)| ((*key).to_owned(), (*value).to_owned()))
            .collect();
        let config = HubConfig::from_map(&vars).unwrap();
        let credentials = MemoryCredentialStore::default();
        if let Some(auth) = config
            .profiles
            .iter()
            .find_map(|profile| profile.copilot.clone())
        {
            credentials
                .set(
                    &auth.flow.credential_account(&auth.github_domain),
                    LOGIN_TOKEN,
                )
                .unwrap();
        }
        AppState::with_seams(
            config,
            store,
            Arc::new(credentials),
            Arc::new(FakeGhCli::new(GhOutcome::Token("gho_test".to_owned()))),
        )
        .unwrap()
    }

    fn sqlite_store(dir: &tempfile::TempDir) -> Store {
        let path = dir.path().join("budget");
        Store::open("sqlite", Some(path.to_str().unwrap())).unwrap()
    }

    /// Records one request of `tokens_in` input tokens for `profile`.
    fn record(store: &Store, profile: &str, tokens_in: u64) {
        store
            .record(&RequestOutcome {
                profile: profile.into(),
                model_key: format!("{profile}/m"),
                status: 200,
                ts_ms: 0,
                latency_ms: 1,
                ttft_ms: None,
                tokens_in,
                tokens_out: 0,
                cache_read_tokens: 0,
                cache_write_tokens: 0,
                error: None,
                client: None,
                session_id: None,
                worker: None,
                routed_by: None,
                attempt: None,
                role: None,
            })
            .unwrap();
    }

    /// Two priced profiles: `a` at $1 and `b` at $2 per 1M input tokens.
    const PRICED: [(&str, &str); 5] = [
        ("LLM_HUB_PROFILES", "a,b"),
        ("LLM_HUB_A_BASE_URL", "http://127.0.0.1:9"),
        ("LLM_HUB_A_INPUT_USD_PER_1M", "1"),
        ("LLM_HUB_B_BASE_URL", "http://127.0.0.1:9"),
        ("LLM_HUB_B_INPUT_USD_PER_1M", "2"),
    ];

    #[derive(Default)]
    struct Recorder(Mutex<Vec<String>>);

    impl Sink for Recorder {
        fn name(&self) -> &'static str {
            "recorder"
        }

        fn send<'a>(&'a self, notification: &'a Notification) -> BoxFuture<'a, Result<(), String>> {
            self.0.lock().unwrap().push(notification.id.clone());
            Box::pin(async { Ok(()) })
        }
    }

    fn ids(notes: &[Notification]) -> Vec<(String, NotificationSeverity)> {
        let mut ids: Vec<_> = notes.iter().map(|n| (n.id.clone(), n.severity)).collect();
        ids.sort();
        ids
    }

    async fn delivered(recorder: &Recorder, want: usize) -> Vec<String> {
        for _ in 0..200 {
            if recorder.0.lock().unwrap().len() >= want {
                break;
            }
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
        let mut ids = recorder.0.lock().unwrap().clone();
        ids.sort();
        ids
    }

    #[tokio::test]
    async fn spend_against_the_global_and_profile_budgets_raises_each_highest_step() {
        let dir = tempfile::tempdir().unwrap();
        let store = sqlite_store(&dir);
        record(&store, "a", 4_000_000);
        record(&store, "b", 3_000_000);
        let mut vars = PRICED.to_vec();
        vars.extend([
            ("LLM_HUB_BUDGET_MONTHLY_USD", "12"),
            ("LLM_HUB_A_BUDGET_MONTHLY_USD", "5"),
            ("LLM_HUB_B_BUDGET_MONTHLY_USD", "4"),
        ]);
        let state = state_with(&vars, Some(store));
        let now = now_ms();
        let period = budget::period_of(now);

        let notes = run_once(&state, now).await;

        assert_eq!(
            ids(&notes),
            vec![
                (
                    format!("budget:a:spend_usd:{period}:80"),
                    NotificationSeverity::Warning
                ),
                (
                    format!("budget:b:spend_usd:{period}:100"),
                    NotificationSeverity::Critical
                ),
                (
                    format!("budget:global:spend_usd:{period}:80"),
                    NotificationSeverity::Warning
                ),
            ]
        );
        assert_eq!(state.notifier().list().len(), 3);
    }

    #[tokio::test]
    async fn no_budgets_and_no_quotas_raise_nothing() {
        let dir = tempfile::tempdir().unwrap();
        let store = sqlite_store(&dir);
        record(&store, "a", 50_000_000);
        let state = state_with(&PRICED, Some(store));

        assert!(run_once(&state, now_ms()).await.is_empty());
        assert!(state.notifier().list().is_empty());
    }

    #[tokio::test]
    async fn a_budget_without_a_store_raises_nothing() {
        let mut vars = PRICED.to_vec();
        vars.push(("LLM_HUB_BUDGET_MONTHLY_USD", "1"));
        let state = state_with(&vars, None);

        assert!(run_once(&state, now_ms()).await.is_empty());
    }

    #[tokio::test]
    async fn notifications_off_raise_nothing() {
        let dir = tempfile::tempdir().unwrap();
        let store = sqlite_store(&dir);
        record(&store, "a", 50_000_000);
        let mut vars = PRICED.to_vec();
        vars.extend([
            ("LLM_HUB_BUDGET_MONTHLY_USD", "1"),
            ("LLM_HUB_NOTIFICATIONS", "false"),
        ]);
        let state = state_with(&vars, Some(store));

        assert!(run_once(&state, now_ms()).await.is_empty());
    }

    #[tokio::test]
    async fn a_second_run_delivers_nothing_new() {
        let dir = tempfile::tempdir().unwrap();
        let store = sqlite_store(&dir);
        record(&store, "a", 9_000_000);
        let mut vars = PRICED.to_vec();
        vars.push(("LLM_HUB_A_BUDGET_MONTHLY_USD", "10"));
        let state = state_with(&vars, Some(store));
        let recorder = Arc::new(Recorder::default());
        state
            .notifier()
            .spawn_delivery(vec![Box::new(Arc::clone(&recorder))]);
        let now = now_ms();

        let first = run_once(&state, now).await;
        let second = run_once(&state, now + 1).await;

        assert_eq!(ids(&first), ids(&second));
        let id = format!("budget:a:spend_usd:{}:85", budget::period_of(now));
        assert_eq!(delivered(&recorder, 1).await, vec![id]);
        assert_eq!(state.notifier().list().len(), 1);
    }

    #[tokio::test]
    async fn a_cached_quota_past_80_percent_raises_one_warning() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/copilot_internal/user"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "quota_reset_date_utc": "2026-12-01T00:00:00Z",
                "quota_snapshots": {"premium_interactions": {
                    "entitlement": 300, "remaining": 54, "percent_remaining": 18.0,
                }},
            })))
            .expect(1)
            .mount(&server)
            .await;
        let uri = server.uri();
        let state = state_with(
            &[
                ("LLM_HUB_PROFILES", "copilot"),
                ("LLM_HUB_COPILOT_BASE_URL", "http://127.0.0.1:9"),
                ("LLM_HUB_COPILOT_AUTH", "github-copilot-vscode"),
                ("LLM_HUB_COPILOT_GITHUB_API_URL", &uri),
            ],
            None,
        );
        let key = UsageKey::for_profile(&state.config(), "copilot").unwrap();
        let fetched = profile_usage::usage(&state, &crate::services::http_client::client(), "copilot", &key).await;
        assert!(fetched.is_some());

        let notes = run_once(&state, now_ms()).await;

        assert_eq!(
            ids(&notes),
            vec![(
                "budget:copilot:premium_interactions:2026-12:80".to_owned(),
                NotificationSeverity::Warning
            )]
        );
    }

    #[tokio::test]
    async fn the_loop_waits_for_its_first_check_then_repeats() {
        let dir = tempfile::tempdir().unwrap();
        let store = sqlite_store(&dir);
        record(&store, "a", 9_000_000);
        let mut vars = PRICED.to_vec();
        vars.push(("LLM_HUB_A_BUDGET_MONTHLY_USD", "10"));
        let state = state_with(&vars, Some(store));
        let handle = spawn_every(
            state.clone(),
            Duration::from_millis(200),
            Duration::from_millis(5),
        );

        assert!(state.notifier().list().is_empty());
        for _ in 0..400 {
            if !state.notifier().list().is_empty() {
                break;
            }
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
        tokio::time::sleep(Duration::from_millis(30)).await;
        assert_eq!(state.notifier().list().len(), 1);
        handle.abort();
    }
}
