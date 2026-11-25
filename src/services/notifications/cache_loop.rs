//! The prompt-cache watch: burst and hub-wide hit-rate notifications checked every five minutes.

use std::time::Duration;

use crate::dependencies::state::AppState;
use crate::schemas::notification::Notification;
use crate::services::cache_stats;
use crate::services::store::{Store, now_ms};

const FIRST_RUN_DELAY: Duration = Duration::from_mins(1);
const INTERVAL: Duration = Duration::from_mins(5);

pub fn spawn_cache_loop(state: AppState) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        let start = tokio::time::Instant::now() + FIRST_RUN_DELAY;
        let mut ticker = tokio::time::interval_at(start, INTERVAL);
        ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        loop {
            ticker.tick().await;
            run_once(&state, now_ms()).await;
        }
    })
}

pub async fn run_once(state: &AppState, now_ms: u64) -> Vec<Notification> {
    let config = state.config();
    if !config.notify.enabled || !config.notify.cache_enabled {
        return Vec::new();
    }
    let reader = state.clone();
    let result = tokio::task::spawn_blocking(move || {
        let Some(store) = reader.store() else {
            return Err("cache watch needs a store".to_owned());
        };
        if !matches!(store, Store::Sqlite(_)) {
            return Err("cache watch needs the sqlite store".to_owned());
        }
        cache_stats::CacheStats::read(store, &reader.pricing_book(), now_ms, "1h")
    })
    .await;
    let analysis = match result {
        Ok(Ok(found)) => found,
        Ok(Err(error)) => {
            tracing::warn!("cache watch read failed: {error}");
            return Vec::new();
        }
        Err(error) => {
            tracing::warn!("cache watch task failed: {error}");
            return Vec::new();
        }
    };
    let notify = &config.notify;
    let mut notifications = cache_stats::burst_notifications(
        &analysis,
        now_ms,
        notify.cache_miss_burst_count,
        notify.cache_miss_burst_usd,
    );
    if let Some(notification) = cache_stats::low_hit_notification(
        &analysis,
        now_ms,
        notify.cache_hit_low_pct,
        notify.cache_hit_min_requests,
    ) {
        notifications.push(notification);
    }
    notifications
        .into_iter()
        .filter_map(|notification| state.notifier().notify(notification))
        .collect()
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;
    use std::sync::Arc;

    use super::*;
    use crate::configs::HubConfig;
    use crate::services::cache_stats::{
        CacheCauseStats, CacheMissCause, CacheStats, CacheTopSession,
    };
    use crate::services::fake_gh_cli::FakeGhCli;
    use crate::services::gh_cli::GhOutcome;
    use crate::services::memory_credential_store::MemoryCredentialStore;

    fn state_with(vars: &[(&str, &str)], store: Option<Store>) -> AppState {
        let vars: HashMap<String, String> = vars
            .iter()
            .map(|(key, value)| ((*key).to_owned(), (*value).to_owned()))
            .collect();
        AppState::with_seams(
            HubConfig::from_map(&vars).unwrap(),
            store,
            Arc::new(MemoryCredentialStore::default()),
            Arc::new(FakeGhCli::new(GhOutcome::NotInstalled)),
        )
        .unwrap()
    }

    fn sqlite_store(dir: &tempfile::TempDir) -> Store {
        Store::open(
            "sqlite",
            Some(dir.path().join("cache-loop.db").to_str().unwrap()),
        )
        .unwrap()
    }

    fn stats(hit: f64, requests: u64, misses: u64, usd: f64) -> CacheStats {
        CacheStats {
            range: "1h".to_owned(),
            hit_rate_pct: hit,
            requests,
            big_misses: misses,
            miss_usd: usd,
            by_cause: vec![CacheCauseStats {
                cause: CacheMissCause::PrefixChanged,
                count: misses,
                tokens: 100_000 * misses,
                usd,
            }],
            top_sessions: vec![CacheTopSession {
                worker: "abcdefgh012345".to_owned(),
                client: Some("claude-client".to_owned()),
                model: Some("demo/model".to_owned()),
                big_misses: misses,
                miss_usd: usd,
                hit_rate_pct: hit,
            }],
        }
    }

    #[test]
    fn burst_count_threshold_fires_at_and_above() {
        assert_eq!(
            cache_stats::burst_notifications(&stats(50.0, 20, 3, 1.0), 3_600_000, 3, 5.0).len(),
            1
        );
        assert_eq!(
            cache_stats::burst_notifications(&stats(50.0, 20, 4, 1.0), 3_600_000, 3, 5.0).len(),
            1
        );
        assert!(
            cache_stats::burst_notifications(&stats(50.0, 20, 2, 1.0), 3_600_000, 3, 5.0)
                .is_empty()
        );
    }

    #[test]
    fn burst_usd_threshold_fires_at_and_above() {
        assert_eq!(
            cache_stats::burst_notifications(&stats(50.0, 20, 1, 5.0), 3_600_000, 3, 5.0).len(),
            1
        );
        assert_eq!(
            cache_stats::burst_notifications(&stats(50.0, 20, 1, 6.0), 3_600_000, 3, 5.0).len(),
            1
        );
        assert!(
            cache_stats::burst_notifications(&stats(50.0, 20, 1, 4.9), 3_600_000, 3, 5.0)
                .is_empty()
        );
    }

    #[test]
    fn low_hit_threshold_needs_min_requests_and_below_pct() {
        assert!(
            cache_stats::low_hit_notification(&stats(79.9, 20, 1, 1.0), 3_600_000, 80.0, 20)
                .is_some()
        );
        assert!(
            cache_stats::low_hit_notification(&stats(80.0, 20, 1, 1.0), 3_600_000, 80.0, 20)
                .is_none()
        );
        assert!(
            cache_stats::low_hit_notification(&stats(79.9, 19, 1, 1.0), 3_600_000, 80.0, 20)
                .is_none()
        );
    }

    #[test]
    fn same_hour_dedupe_and_next_hour_new_id() {
        let first = cache_stats::burst_notifications(&stats(50.0, 20, 3, 6.0), 3_600_000, 3, 5.0);
        let same =
            cache_stats::burst_notifications(&stats(50.0, 20, 3, 6.0), 3_600_000 + 60_000, 3, 5.0);
        let next = cache_stats::burst_notifications(&stats(50.0, 20, 3, 6.0), 7_200_000, 3, 5.0);
        assert_eq!(first[0].id, same[0].id);
        assert_ne!(first[0].id, next[0].id);
        assert_eq!(first[0].id, "cache:abcdefgh012345:miss_burst:1");
    }

    #[test]
    fn notification_text_contains_worker_model_count_usd_and_top_cause() {
        let note = cache_stats::burst_notifications(&stats(50.0, 20, 3, 6.25), 3_600_000, 3, 5.0)
            .remove(0);
        assert!(note.title.contains("abcdefgh"));
        assert!(note.body.contains("abcdefgh"));
        assert!(note.body.contains("demo/model"));
        assert!(note.body.contains("3 big misses"));
        assert!(note.body.contains("$6.25"));
        assert!(note.body.contains("prefix_changed"));
    }

    fn record_big_misses(store: &Store, now_ms: u64, worker: &str, count: usize) {
        for index in 0..count {
            store
                .record(&crate::services::stats::RequestOutcome {
                    profile: "demo".to_owned(),
                    model_key: "demo/model".to_owned(),
                    status: 200,
                    ts_ms: now_ms.saturating_sub(index as u64 * 60_000),
                    latency_ms: 1,
                    ttft_ms: None,
                    tokens_in: 150_000,
                    tokens_out: 1,
                    cache_read_tokens: 10_000,
                    cache_write_tokens: 0,
                    error: None,
                    client: Some("claude-client".to_owned()),
                    session_id: None,
                    worker: Some(worker.to_owned()),
                    routed_by: None,
                    attempt: None,
                    role: None,
                })
                .unwrap();
        }
        if let Store::Sqlite(connection) = store {
            let guard = connection.lock().unwrap();
            let start = now_ms.saturating_sub(count as u64 * 60_000);
            guard
                .execute(
                    "UPDATE requests SET ts_ms = ?1",
                    [i64::try_from(start).unwrap_or(i64::MAX)],
                )
                .unwrap();
        }
    }

    #[tokio::test]
    async fn state_loop_count_threshold_and_hour_dedupes() {
        let dir = tempfile::tempdir().unwrap();
        let store = sqlite_store(&dir);
        let now = 3_600_000;
        record_big_misses(&store, now, "abcdefgh012345", 3);
        let state = state_with(
            &[
                ("LLM_HUB_PROFILES", "demo"),
                ("LLM_HUB_DEMO_BASE_URL", "http://127.0.0.1:9"),
                ("LLM_HUB_CACHE_MISS_BURST_COUNT", "3"),
                ("LLM_HUB_CACHE_MISS_BURST_USD", "100"),
            ],
            Some(store),
        );

        let first = run_once(&state, now).await;
        let same_hour = run_once(&state, now + 60_000).await;
        assert_eq!(first.len(), 1);
        assert_eq!(same_hour.len(), 1);
        assert_eq!(same_hour[0].id, first[0].id);
    }

    #[tokio::test]
    async fn state_loop_low_hit_and_disabled_branches() {
        let dir = tempfile::tempdir().unwrap();
        let store = sqlite_store(&dir);
        let now = 3_600_000;
        for _index in 0..20 {
            store
                .record(&crate::services::stats::RequestOutcome {
                    profile: "demo".to_owned(),
                    model_key: "demo/model".to_owned(),
                    status: 200,
                    ts_ms: now,
                    latency_ms: 1,
                    ttft_ms: None,
                    tokens_in: 1_000,
                    tokens_out: 1,
                    cache_read_tokens: 500,
                    cache_write_tokens: 0,
                    error: None,
                    client: Some("claude-client".to_owned()),
                    session_id: None,
                    worker: Some("worker-a".to_owned()),
                    routed_by: None,
                    attempt: None,
                    role: None,
                })
                .unwrap();
        }
        let state = state_with(
            &[
                ("LLM_HUB_PROFILES", "demo"),
                ("LLM_HUB_DEMO_BASE_URL", "http://127.0.0.1:9"),
                ("LLM_HUB_CACHE_HIT_LOW_PCT", "80"),
                ("LLM_HUB_CACHE_HIT_MIN_REQUESTS", "20"),
            ],
            Some(store),
        );
        assert_eq!(run_once(&state, now).await.len(), 1);

        let disabled = state_with(&[("LLM_HUB_NOTIFY_CACHE", "false")], None);
        assert!(run_once(&disabled, now).await.is_empty());
    }

    #[tokio::test]
    async fn cache_disabled_returns_nothing() {
        let dir = tempfile::tempdir().unwrap();
        let state = state_with(
            &[
                ("LLM_HUB_PROFILES", "demo"),
                ("LLM_HUB_DEMO_BASE_URL", "http://127.0.0.1:9"),
                ("LLM_HUB_NOTIFY_CACHE", "false"),
            ],
            Some(sqlite_store(&dir)),
        );
        assert!(run_once(&state, 3_600_000).await.is_empty());
    }
}
