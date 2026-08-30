use std::sync::Mutex;
use std::sync::atomic::{AtomicU64, Ordering};

use dashmap::DashMap;
use hdrhistogram::Histogram;
use serde::Serialize;

use crate::consts::{STATS_MAX_MODEL_KEYS, STATS_OVERFLOW_KEY};

#[derive(Default)]
pub struct EntryStats {
    requests: AtomicU64,
    errors: AtomicU64,
    tokens_in: AtomicU64,
    tokens_out: AtomicU64,
    latency_ms: Mutex<Option<Histogram<u64>>>,
}

#[derive(Serialize, Clone)]
pub struct EntrySnapshot {
    pub key: String,
    pub requests: u64,
    pub errors: u64,
    pub tokens_in: u64,
    pub tokens_out: u64,
    pub p50_ms: u64,
    pub p95_ms: u64,
}

#[derive(Serialize)]
pub struct StatsSnapshot {
    pub profiles: Vec<EntrySnapshot>,
    pub models: Vec<EntrySnapshot>,
}

/// In-memory, lock-free-on-read stats. Model keys come from caller input, so
/// the map is capped: past STATS_MAX_MODEL_KEYS new names bucket into "other".
#[derive(Default)]
pub struct StatsRegistry {
    by_profile: DashMap<String, EntryStats>,
    by_model: DashMap<String, EntryStats>,
}

pub struct RequestOutcome {
    pub profile: String,
    pub model_key: String,
    pub status: u16,
    pub latency_ms: u64,
    pub tokens_in: u64,
    pub tokens_out: u64,
}

impl StatsRegistry {
    pub fn record(&self, outcome: &RequestOutcome) {
        record_into(&self.by_profile, &outcome.profile, outcome, usize::MAX);
        record_into(
            &self.by_model,
            &outcome.model_key,
            outcome,
            STATS_MAX_MODEL_KEYS,
        );
    }

    pub fn snapshot(&self) -> StatsSnapshot {
        StatsSnapshot {
            profiles: snapshot_map(&self.by_profile),
            models: snapshot_map(&self.by_model),
        }
    }
}

fn record_into(map: &DashMap<String, EntryStats>, key: &str, outcome: &RequestOutcome, cap: usize) {
    let effective_key = if map.len() >= cap && !map.contains_key(key) {
        STATS_OVERFLOW_KEY
    } else {
        key
    };
    let entry = map.entry(effective_key.to_string()).or_default();
    entry.requests.fetch_add(1, Ordering::Relaxed);
    if outcome.status >= 400 {
        entry.errors.fetch_add(1, Ordering::Relaxed);
    }
    entry
        .tokens_in
        .fetch_add(outcome.tokens_in, Ordering::Relaxed);
    entry
        .tokens_out
        .fetch_add(outcome.tokens_out, Ordering::Relaxed);
    if let Ok(mut guard) = entry.latency_ms.lock() {
        let histogram = guard.get_or_insert_with(new_histogram);
        let _ = histogram.record(outcome.latency_ms.max(1));
    }
}

fn new_histogram() -> Histogram<u64> {
    // 1ms..1h, 3 significant digits — a few KB per entry.
    Histogram::new_with_bounds(1, 3_600_000, 3).expect("static histogram bounds are valid")
}

fn snapshot_map(map: &DashMap<String, EntryStats>) -> Vec<EntrySnapshot> {
    let mut rows: Vec<EntrySnapshot> = map
        .iter()
        .map(|entry| {
            let stats = entry.value();
            let (p50_ms, p95_ms) = match stats.latency_ms.lock() {
                Ok(guard) => guard
                    .as_ref()
                    .map(|h| (h.value_at_quantile(0.5), h.value_at_quantile(0.95)))
                    .unwrap_or((0, 0)),
                Err(_) => (0, 0),
            };
            EntrySnapshot {
                key: entry.key().clone(),
                requests: stats.requests.load(Ordering::Relaxed),
                errors: stats.errors.load(Ordering::Relaxed),
                tokens_in: stats.tokens_in.load(Ordering::Relaxed),
                tokens_out: stats.tokens_out.load(Ordering::Relaxed),
                p50_ms,
                p95_ms,
            }
        })
        .collect();
    rows.sort_by_key(|row| std::cmp::Reverse(row.requests));
    rows
}

/// Scrapes token usage out of a response tail — works for both plain JSON
/// bodies and the terminal SSE usage chunk, without full JSON parsing of
/// arbitrarily large streams.
pub fn scrape_usage(tail: &[u8]) -> (u64, u64) {
    let text = String::from_utf8_lossy(tail);
    let Some(usage_pos) = text.rfind("\"usage\"") else {
        return (0, 0);
    };
    let after = &text[usage_pos..];
    (
        extract_int(after, "\"prompt_tokens\""),
        extract_int(after, "\"completion_tokens\""),
    )
}

fn extract_int(text: &str, field: &str) -> u64 {
    let Some(pos) = text.find(field) else {
        return 0;
    };
    text[pos + field.len()..]
        .chars()
        .skip_while(|c| *c == ':' || c.is_whitespace())
        .take_while(char::is_ascii_digit)
        .collect::<String>()
        .parse()
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn outcome(profile: &str, model: &str, status: u16) -> RequestOutcome {
        RequestOutcome {
            profile: profile.into(),
            model_key: model.into(),
            status,
            latency_ms: 100,
            tokens_in: 10,
            tokens_out: 20,
        }
    }

    #[test]
    fn records_and_snapshots() {
        let registry = StatsRegistry::default();
        registry.record(&outcome("openai", "openai/gpt-4o", 200));
        registry.record(&outcome("openai", "openai/gpt-4o", 500));
        let snapshot = registry.snapshot();
        assert_eq!(snapshot.profiles[0].requests, 2);
        assert_eq!(snapshot.profiles[0].errors, 1);
        assert_eq!(snapshot.profiles[0].tokens_in, 20);
        assert!(snapshot.profiles[0].p50_ms >= 100);
    }

    #[test]
    fn caps_model_cardinality() {
        let registry = StatsRegistry::default();
        for i in 0..(STATS_MAX_MODEL_KEYS + 50) {
            registry.record(&outcome("p", &format!("p/model-{i}"), 200));
        }
        let snapshot = registry.snapshot();
        assert!(snapshot.models.len() <= STATS_MAX_MODEL_KEYS + 1);
        assert!(
            snapshot
                .models
                .iter()
                .any(|entry| entry.key == STATS_OVERFLOW_KEY)
        );
    }

    #[test]
    fn scrapes_json_usage() {
        let body =
            br#"{"id":"x","usage":{"prompt_tokens":12,"completion_tokens":34,"total_tokens":46}}"#;
        assert_eq!(scrape_usage(body), (12, 34));
    }

    #[test]
    fn scrapes_sse_terminal_usage_chunk() {
        let body = b"data: {\"choices\":[]}\n\ndata: {\"choices\":[],\"usage\":{\"prompt_tokens\":7,\"completion_tokens\":9}}\n\ndata: [DONE]\n\n";
        assert_eq!(scrape_usage(body), (7, 9));
    }

    #[test]
    fn no_usage_yields_zero() {
        assert_eq!(scrape_usage(b"data: [DONE]"), (0, 0));
    }
}
