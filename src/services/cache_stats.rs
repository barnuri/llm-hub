//! Prompt-cache hit and miss analysis over durable request rows.

use std::collections::HashMap;

use serde::Serialize;

use crate::schemas::notification::{
    Notification, NotificationKind, NotificationSeverity, NotificationStatus,
};
use crate::services::harness::is_keep_warm;
use crate::services::pricing::PricingBook;
use crate::services::stats::RequestOutcome;
use crate::services::store::{StatsFilter, Store};

const HOUR_MS: u64 = 60 * 60 * 1000;
const BIG_MISS_INPUT_TOKENS: u64 = 100_000;
const GAP_MS: u64 = 5 * 60 * 1000;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum CacheMissCause {
    First,
    ModelSwitch,
    PromptShrank,
    IdleNoPing,
    IdleOverTtl,
    PrefixChanged,
}

impl CacheMissCause {
    fn label(self) -> &'static str {
        match self {
            Self::First => "first",
            Self::ModelSwitch => "model_switch",
            Self::PromptShrank => "prompt_shrank",
            Self::IdleNoPing => "idle_no_ping",
            Self::IdleOverTtl => "idle_over_ttl",
            Self::PrefixChanged => "prefix_changed",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct CacheCauseStats {
    pub cause: CacheMissCause,
    pub count: u64,
    pub tokens: u64,
    pub usd: f64,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize)]
pub struct CacheTopSession {
    pub worker: String,
    pub client: Option<String>,
    pub model: Option<String>,
    pub big_misses: u64,
    pub miss_usd: f64,
    pub hit_rate_pct: f64,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct CacheStats {
    pub range: String,
    pub hit_rate_pct: f64,
    pub requests: u64,
    pub big_misses: u64,
    pub miss_usd: f64,
    pub by_cause: Vec<CacheCauseStats>,
    pub top_sessions: Vec<CacheTopSession>,
}

impl CacheStats {
    pub fn read(
        store: &Store,
        book: &PricingBook,
        now_ms: u64,
        range: &str,
    ) -> Result<Self, String> {
        let window_ms = match range {
            "1h" => HOUR_MS,
            "1d" => 24 * HOUR_MS,
            _ => return Err("cache range must be 1h or 1d".to_owned()),
        };
        let since_ms = now_ms.saturating_sub(window_ms);
        let rows = store.filtered_outcomes(&StatsFilter {
            since_ms: Some(since_ms),
            profile: None,
            model: None,
            routed_by_prefix: None,
            range_label: range.to_owned(),
        })?;
        Ok(analyze(&rows, book, range))
    }
}

fn successful_non_ping(rows: &[RequestOutcome]) -> impl Iterator<Item = &RequestOutcome> {
    rows.iter()
        .filter(|row| row.status < 400 && !is_keep_warm(row.client.as_deref()))
}

fn miss_usd(row: &RequestOutcome, book: &PricingBook) -> f64 {
    let card = book.price_card_for(&row.model_key);
    let missed = row.tokens_in.saturating_sub(row.cache_read_tokens);
    let rate = card.rates_for_prompt(row.tokens_in).cache_write_per_1m;
    f64::from(u32::try_from(missed).unwrap_or(u32::MAX)) * rate / 1_000_000.0
}

pub fn analyze(rows: &[RequestOutcome], book: &PricingBook, range: &str) -> CacheStats {
    let successful: Vec<&RequestOutcome> = successful_non_ping(rows).collect();
    let total_in: u64 = successful.iter().map(|row| row.tokens_in).sum();
    let cache_read: u64 = successful.iter().map(|row| row.cache_read_tokens).sum();
    let hit_rate_pct = pct(cache_read, total_in);

    let mut previous: HashMap<String, &RequestOutcome> = HashMap::new();
    let mut by_cause: HashMap<CacheMissCause, CacheCauseStats> = HashMap::new();
    let mut by_worker: HashMap<String, CacheTopSession> = HashMap::new();
    let mut worker_in: HashMap<String, u64> = HashMap::new();
    let mut worker_read: HashMap<String, u64> = HashMap::new();
    let mut misses = 0u64;
    let mut total_miss_usd = 0.0;

    for row in successful.iter().copied() {
        let worker = row.worker.clone().unwrap_or_else(|| "no-worker".to_owned());
        *worker_in.entry(worker.clone()).or_default() += row.tokens_in;
        *worker_read.entry(worker.clone()).or_default() += row.cache_read_tokens;
        let top = by_worker.entry(worker.clone()).or_default();
        top.worker.clone_from(&worker);
        top.client = row.client.clone().or(top.client.clone());
        top.model = Some(row.model_key.clone());

        if row.tokens_in >= BIG_MISS_INPUT_TOKENS && row.cache_read_tokens * 2 < row.tokens_in {
            let cause = cause_for(row, previous.get(&worker).copied(), rows);
            let usd = miss_usd(row, book);
            misses += 1;
            total_miss_usd += usd;
            top.big_misses += 1;
            top.miss_usd += usd;
            let entry = by_cause.entry(cause).or_insert(CacheCauseStats {
                cause,
                count: 0,
                tokens: 0,
                usd: 0.0,
            });
            entry.count += 1;
            entry.tokens += row.tokens_in;
            entry.usd += usd;
        }
        previous.insert(worker, row);
    }

    let mut top_sessions: Vec<_> = by_worker.into_values().collect();
    for session in &mut top_sessions {
        let input = worker_in.get(&session.worker).copied().unwrap_or(0);
        let read = worker_read.get(&session.worker).copied().unwrap_or(0);
        session.hit_rate_pct = pct(read, input);
        session.worker = session.worker.clone();
    }
    top_sessions.sort_by(|a, b| {
        b.miss_usd
            .total_cmp(&a.miss_usd)
            .then_with(|| b.big_misses.cmp(&a.big_misses))
            .then_with(|| a.worker.cmp(&b.worker))
    });
    top_sessions.truncate(10);

    let mut by_cause: Vec<_> = by_cause.into_values().collect();
    by_cause.sort_by(|a, b| {
        b.count
            .cmp(&a.count)
            .then_with(|| a.cause.label().cmp(b.cause.label()))
    });

    CacheStats {
        range: range.to_owned(),
        hit_rate_pct,
        requests: successful.len() as u64,
        big_misses: misses,
        miss_usd: total_miss_usd,
        by_cause,
        top_sessions,
    }
}

fn cause_for(
    row: &RequestOutcome,
    previous: Option<&RequestOutcome>,
    rows: &[RequestOutcome],
) -> CacheMissCause {
    let Some(previous) = previous else {
        return CacheMissCause::First;
    };
    if previous.model_key != row.model_key {
        return CacheMissCause::ModelSwitch;
    }
    if row.tokens_in * 10 < previous.tokens_in * 7 {
        return CacheMissCause::PromptShrank;
    }
    let gap = row.ts_ms.saturating_sub(previous.ts_ms);
    if gap > GAP_MS {
        let worker = row.worker.as_deref().unwrap_or("");
        let ping_before = rows.iter().any(|candidate| {
            is_keep_warm(candidate.client.as_deref())
                && candidate.worker.as_deref()
                    == (if worker.is_empty() {
                        None
                    } else {
                        Some(worker)
                    })
                && candidate.ts_ms >= previous.ts_ms
                && candidate.ts_ms <= row.ts_ms
                && candidate.ts_ms >= row.ts_ms.saturating_sub(GAP_MS)
        });
        return if ping_before {
            CacheMissCause::IdleOverTtl
        } else {
            CacheMissCause::IdleNoPing
        };
    }
    CacheMissCause::PrefixChanged
}

pub fn worker_short_id(worker: &str) -> String {
    worker.chars().take(8).collect()
}

pub fn burst_notifications(
    stats: &CacheStats,
    now_ms: u64,
    miss_count: u64,
    miss_usd_threshold: f64,
) -> Vec<Notification> {
    let hour = now_ms / HOUR_MS;
    let mut notes = Vec::new();
    for session in &stats.top_sessions {
        if session.big_misses >= miss_count || session.miss_usd >= miss_usd_threshold {
            let top_cause = stats
                .by_cause
                .first()
                .map_or("unknown", |cause| cause.cause.label());
            let id = format!("cache:{}:{}:{hour}", session.worker, "miss_burst");
            notes.push(cache_notification(
                id,
                "cache_miss_burst",
                format!(
                    "Prompt-cache misses on {}",
                    worker_short_id(&session.worker)
                ),
                format!(
                    "worker {} ({}) on model {}: {} big misses, miss estimate {}, top cause {}",
                    worker_short_id(&session.worker),
                    session.client.as_deref().unwrap_or("unknown client"),
                    session.model.as_deref().unwrap_or("unknown model"),
                    session.big_misses,
                    usd_short(session.miss_usd),
                    top_cause
                ),
                session.model.clone(),
                now_ms,
            ));
        }
    }
    notes
}

pub fn low_hit_notification(
    stats: &CacheStats,
    now_ms: u64,
    pct: f64,
    min_requests: u64,
) -> Option<Notification> {
    if stats.requests < min_requests || stats.hit_rate_pct >= pct {
        return None;
    }
    let hour = now_ms / HOUR_MS;
    Some(cache_notification(
        format!("cache:hub:hit_low:{hour}"),
        "cache_hit_low",
        format!("Hub prompt-cache hit rate low: {:.1}%", stats.hit_rate_pct),
        format!(
            "Hit rate {:.1}% across {} successful requests; {} big misses, miss estimate {}",
            stats.hit_rate_pct,
            stats.requests,
            stats.big_misses,
            usd_short(stats.miss_usd)
        ),
        None,
        now_ms,
    ))
}

fn cache_notification(
    id: String,
    rule: &str,
    title: String,
    body: String,
    model: Option<String>,
    now_ms: u64,
) -> Notification {
    Notification {
        id,
        kind: NotificationKind::Advisory,
        source: "cache".to_owned(),
        severity: NotificationSeverity::Warning,
        title,
        body,
        profile: None,
        model,
        rule: Some(rule.to_owned()),
        diff: None,
        actions: Notification::default_actions(NotificationKind::Advisory),
        status: NotificationStatus::Pending,
        created_ms: now_ms,
        updated_ms: now_ms,
        result: None,
        muted: false,
    }
}

fn pct(read: u64, input: u64) -> f64 {
    if input == 0 {
        0.0
    } else {
        let ratio = f64::from(u32::try_from(read).unwrap_or(u32::MAX))
            / f64::from(u32::try_from(input).unwrap_or(u32::MAX));
        ratio * 100.0
    }
}

fn usd_short(value: f64) -> String {
    format!("${value:.2}")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn row(
        ts_ms: u64,
        worker: &str,
        model: &str,
        tokens_in: u64,
        cache_read: u64,
        client: Option<&str>,
    ) -> RequestOutcome {
        RequestOutcome {
            profile: "demo".to_owned(),
            model_key: model.to_owned(),
            status: 200,
            ts_ms,
            latency_ms: 1,
            ttft_ms: None,
            tokens_in,
            tokens_out: 1,
            cache_read_tokens: cache_read,
            cache_write_tokens: 0,
            error: None,
            client: client.map(str::to_owned),
            session_id: None,
            worker: Some(worker.to_owned()),
            routed_by: None,
            attempt: None,
            role: None,
        }
    }

    fn ping(ts_ms: u64, worker: &str) -> RequestOutcome {
        row(
            ts_ms,
            worker,
            "demo/model",
            100_000,
            99_000,
            Some(KEEP_WARM_CLIENT_FALLBACK),
        )
    }

    const KEEP_WARM_CLIENT_FALLBACK: &str = "llm-hub/keep-warm";

    #[test]
    fn excludes_keep_warm_and_computes_hit_rate_and_big_miss() {
        let rows = vec![
            ping(1, "worker-a"),
            row(2, "worker-a", "demo/model", 100_000, 10_000, Some("claude")),
        ];
        let stats = analyze(&rows, &PricingBook::default(), "1h");
        assert_eq!(stats.requests, 1);
        assert_eq!(stats.big_misses, 1);
        assert!((stats.hit_rate_pct - 10.0).abs() < f64::EPSILON);
    }

    #[test]
    fn all_miss_causes_are_classified() {
        let minute = 60_000;
        let rows = vec![
            row(0, "first", "demo/a", 120_000, 0, Some("claude")),
            row(minute, "switch", "demo/a", 120_000, 0, Some("claude")),
            row(minute * 2, "switch", "demo/b", 120_000, 0, Some("claude")),
            row(minute, "shrink", "demo/a", 200_000, 0, Some("claude")),
            row(minute * 2, "shrink", "demo/a", 100_000, 0, Some("claude")),
            row(0, "idle-no", "demo/a", 200_000, 0, Some("claude")),
            row(minute * 10, "idle-no", "demo/a", 120_000, 0, Some("claude")),
            row(0, "idle-ttl", "demo/a", 200_000, 0, Some("claude")),
            ping(minute * 6, "idle-ttl"),
            row(
                minute * 10,
                "idle-ttl",
                "demo/a",
                120_000,
                0,
                Some("claude"),
            ),
            row(0, "prefix", "demo/a", 200_000, 0, Some("claude")),
            row(minute, "prefix", "demo/a", 120_000, 0, Some("claude")),
        ];
        assert_eq!(cause_for(&rows[0], None, &rows), CacheMissCause::First);
        let switch_previous = row(minute, "switch", "demo/a", 120_000, 0, Some("claude"));
        let switch = row(minute * 2, "switch", "demo/b", 120_000, 0, Some("claude"));
        assert_eq!(
            cause_for(&switch, Some(&switch_previous), &rows),
            CacheMissCause::ModelSwitch
        );
        let shrink_previous = row(minute, "shrink", "demo/a", 200_000, 0, Some("claude"));
        let shrink = row(minute * 2, "shrink", "demo/a", 100_000, 0, Some("claude"));
        assert_eq!(
            cause_for(&shrink, Some(&shrink_previous), &rows),
            CacheMissCause::PromptShrank
        );
        let idle_previous = row(0, "idle-no", "demo/a", 120_000, 0, Some("claude"));
        let idle = row(minute * 10, "idle-no", "demo/a", 120_000, 0, Some("claude"));
        assert_eq!(
            cause_for(&idle, Some(&idle_previous), &rows),
            CacheMissCause::IdleNoPing
        );
        let ttl_previous = row(0, "idle-ttl", "demo/a", 120_000, 0, Some("claude"));
        let ttl = row(
            minute * 10,
            "idle-ttl",
            "demo/a",
            120_000,
            0,
            Some("claude"),
        );
        assert_eq!(
            cause_for(&ttl, Some(&ttl_previous), &rows),
            CacheMissCause::IdleOverTtl
        );
        let prefix_previous = row(0, "prefix", "demo/a", 120_000, 0, Some("claude"));
        let prefix = row(minute, "prefix", "demo/a", 120_000, 0, Some("claude"));
        assert_eq!(
            cause_for(&prefix, Some(&prefix_previous), &rows),
            CacheMissCause::PrefixChanged
        );
    }

    #[test]
    fn burst_requires_count_or_usd_threshold() {
        let mut stats = CacheStats {
            range: "1h".to_owned(),
            hit_rate_pct: 50.0,
            requests: 20,
            big_misses: 3,
            miss_usd: 6.0,
            by_cause: Vec::new(),
            top_sessions: vec![CacheTopSession {
                worker: "worker-a".to_owned(),
                client: Some("claude".to_owned()),
                model: Some("demo/model".to_owned()),
                big_misses: 3,
                miss_usd: 6.0,
                hit_rate_pct: 50.0,
            }],
        };
        assert_eq!(burst_notifications(&stats, 3_600_000, 3, 5.0).len(), 1);
        stats.top_sessions[0].big_misses = 1;
        stats.top_sessions[0].miss_usd = 1.0;
        assert!(burst_notifications(&stats, 3_600_000, 3, 5.0).is_empty());
    }

    #[test]
    fn big_miss_threshold_edges_and_successful_rows_only() {
        let mut rows = vec![
            row(1, "edge", "demo/model", 99_999, 0, Some("claude")),
            row(2, "edge", "demo/model", 100_000, 50_000, Some("claude")),
            row(3, "edge", "demo/model", 100_000, 49_999, Some("claude")),
        ];
        rows.push(RequestOutcome {
            status: 500,
            ..row(4, "failed", "demo/model", 200_000, 0, Some("claude"))
        });
        let stats = analyze(&rows, &PricingBook::default(), "1h");
        assert_eq!(stats.requests, 3);
        assert_eq!(stats.big_misses, 1);

        rows[0].cache_read_tokens = 99_999;
        let stats = analyze(&rows, &PricingBook::default(), "1h");
        assert_eq!(stats.requests, 3);
        assert_eq!(stats.big_misses, 1);
    }

    #[test]
    fn ping_row_does_not_become_previous_request() {
        let rows = vec![
            ping(0, "worker-a"),
            row(61_000, "worker-a", "demo/other", 120_000, 0, Some("claude")),
        ];
        let stats = analyze(&rows, &PricingBook::default(), "1h");
        assert_eq!(stats.big_misses, 1);
        assert_eq!(stats.by_cause[0].cause, CacheMissCause::First);
    }

    #[test]
    fn miss_usd_uses_write_price_then_input_fallback() {
        use crate::configs::TokenRates;
        let mut book = PricingBook::default();
        book.by_model.insert(
            "demo/write".to_owned(),
            TokenRates {
                input_per_1m: 2.0,
                output_per_1m: 0.0,
                cache_read_per_1m: 1.0,
                cache_write_per_1m: 4.0,
            },
        );
        book.by_model.insert(
            "demo/input".to_owned(),
            TokenRates {
                input_per_1m: 3.0,
                output_per_1m: 0.0,
                cache_read_per_1m: 0.0,
                cache_write_per_1m: 0.0,
            }
            .with_cache_defaults(),
        );
        let write = row(1, "worker-write", "demo/write", 100_000, 0, Some("claude"));
        let input = row(2, "worker-input", "demo/input", 100_000, 0, Some("claude"));
        assert!((miss_usd(&write, &book) - 0.4).abs() < f64::EPSILON);
        assert!((miss_usd(&input, &book) - 0.3).abs() < f64::EPSILON);
    }

    #[test]
    fn hit_rate_counts_successful_non_ping_rows_only() {
        let mut failed = row(
            3,
            "worker-a",
            "demo/model",
            300_000,
            300_000,
            Some("claude"),
        );
        failed.status = 500;
        let rows = vec![
            row(1, "worker-a", "demo/model", 100_000, 80_000, Some("claude")),
            ping(2, "worker-a"),
            failed,
        ];
        let stats = analyze(&rows, &PricingBook::default(), "1h");
        assert_eq!(stats.requests, 1);
        assert!((stats.hit_rate_pct - 80.0).abs() < f64::EPSILON);
    }

    #[test]
    fn one_hour_and_one_day_windows_filter_by_timestamp() {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::open(
            "sqlite",
            Some(dir.path().join("cache-window.db").to_str().unwrap()),
        )
        .unwrap();
        let mut old = RequestOutcome {
            profile: "demo".to_owned(),
            model_key: "demo/model".to_owned(),
            status: 200,
            ts_ms: 1_000,
            latency_ms: 1,
            ttft_ms: None,
            tokens_in: 100_000,
            tokens_out: 1,
            cache_read_tokens: 0,
            cache_write_tokens: 0,
            error: None,
            client: Some("claude".to_owned()),
            session_id: None,
            worker: Some("worker-a".to_owned()),
            routed_by: None,
            attempt: None,
            role: None,
        };
        store.record(&old).unwrap();
        old.ts_ms = 2 * HOUR_MS;
        store.record(&old).unwrap();
        if let Store::Sqlite(connection) = &store {
            let guard = connection.lock().unwrap();
            guard
                .execute("UPDATE requests SET ts_ms = ?1 WHERE id = 1", [1_000])
                .unwrap();
            guard
                .execute(
                    "UPDATE requests SET ts_ms = ?1 WHERE id = 2",
                    [i64::try_from(2 * HOUR_MS).unwrap_or(i64::MAX)],
                )
                .unwrap();
        }
        let now = 25 * HOUR_MS;
        let hour = CacheStats::read(&store, &PricingBook::default(), now, "1h").unwrap();
        let day = CacheStats::read(&store, &PricingBook::default(), now, "1d").unwrap();
        assert_eq!(hour.requests, 0);
        assert_eq!(day.requests, 1);
    }

    #[test]
    fn low_hit_requires_request_minimum_and_below_threshold() {
        let stats = CacheStats {
            range: "1h".to_owned(),
            hit_rate_pct: 79.0,
            requests: 20,
            big_misses: 3,
            miss_usd: 1.0,
            by_cause: Vec::new(),
            top_sessions: Vec::new(),
        };
        assert!(low_hit_notification(&stats, 3_600_000, 80.0, 20).is_some());
        let stats = CacheStats {
            requests: 19,
            ..stats
        };
        assert!(low_hit_notification(&stats, 3_600_000, 80.0, 20).is_none());
        let stats = CacheStats {
            hit_rate_pct: 80.0,
            requests: 20,
            ..stats
        };
        assert!(low_hit_notification(&stats, 3_600_000, 80.0, 20).is_none());
    }
}
