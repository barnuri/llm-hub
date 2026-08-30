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
    cache_read_tokens: AtomicU64,
    cache_write_tokens: AtomicU64,
    latency_ms: Mutex<Option<Histogram<u64>>>,
    ttft_ms: Mutex<Option<Histogram<u64>>>,
    /// Tokens/sec × 100 (centi-tps) so hdrhistogram can store them as u64.
    tokens_per_sec_x100: Mutex<Option<Histogram<u64>>>,
    tokens_per_sec_sum_x100: AtomicU64,
    tokens_per_sec_samples: AtomicU64,
}

#[derive(Serialize, Clone, Debug)]
pub struct EntrySnapshot {
    pub key: String,
    pub requests: u64,
    pub errors: u64,
    pub tokens_in: u64,
    pub tokens_out: u64,
    pub cache_read_tokens: u64,
    pub cache_write_tokens: u64,
    pub p50_ms: u64,
    pub p95_ms: u64,
    pub ttft_p50_ms: u64,
    pub ttft_p95_ms: u64,
    pub tokens_per_sec_p50: f64,
    pub tokens_per_sec_avg: f64,
}

#[derive(Serialize, Clone, Debug, Default)]
pub struct OverviewSnapshot {
    pub requests: u64,
    pub errors: u64,
    pub tokens_in: u64,
    pub tokens_out: u64,
    pub cache_read_tokens: u64,
    pub cache_write_tokens: u64,
    pub cache_hit_rate_pct: f64,
    pub p50_ms: u64,
    pub p95_ms: u64,
    pub ttft_p50_ms: u64,
    pub ttft_p95_ms: u64,
    pub tokens_per_sec_p50: f64,
    pub tokens_per_sec_avg: f64,
}

#[derive(Serialize)]
pub struct StatsSnapshot {
    pub range: String,
    pub persistent: bool,
    pub filterable: bool,
    pub overview: OverviewSnapshot,
    pub profiles: Vec<EntrySnapshot>,
    pub models: Vec<EntrySnapshot>,
}

/// Token counts scraped from an upstream usage object.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct ScrapedUsage {
    pub tokens_in: u64,
    pub tokens_out: u64,
    pub cache_read_tokens: u64,
    pub cache_write_tokens: u64,
}

/// In-memory, lock-free-on-read stats. Model keys come from caller input, so
/// the map is capped: past `STATS_MAX_MODEL_KEYS` new names bucket into "other".
#[derive(Default)]
pub struct StatsRegistry {
    by_profile: DashMap<String, EntryStats>,
    by_model: DashMap<String, EntryStats>,
    /// Combined latency/ttft/tps histograms for the overview tiles.
    overall: EntryStats,
}

pub struct RequestOutcome {
    pub profile: String,
    pub model_key: String,
    pub status: u16,
    pub latency_ms: u64,
    /// Time until first upstream byte. `None` for buffered/non-stream replies.
    pub ttft_ms: Option<u64>,
    pub tokens_in: u64,
    pub tokens_out: u64,
    pub cache_read_tokens: u64,
    pub cache_write_tokens: u64,
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
        record_entry(&self.overall, outcome);
    }

    pub fn snapshot(&self, range: &str, persistent: bool) -> StatsSnapshot {
        let profiles = snapshot_map(&self.by_profile);
        let models = snapshot_map(&self.by_model);
        let overview = overview_from_entry(&self.overall);
        StatsSnapshot {
            range: range.to_string(),
            persistent,
            filterable: persistent,
            overview,
            profiles,
            models,
        }
    }
}

/// Build a snapshot by folding individual request rows (store-backed filters).
pub fn snapshot_from_outcomes(
    outcomes: &[RequestOutcome],
    range: &str,
    persistent: bool,
) -> StatsSnapshot {
    let registry = StatsRegistry::default();
    for outcome in outcomes {
        registry.record(outcome);
    }
    registry.snapshot(range, persistent)
}

fn record_into(map: &DashMap<String, EntryStats>, key: &str, outcome: &RequestOutcome, cap: usize) {
    let effective_key = if map.len() >= cap && !map.contains_key(key) {
        STATS_OVERFLOW_KEY
    } else {
        key
    };
    let entry = map.entry(effective_key.to_string()).or_default();
    record_entry(&entry, outcome);
}

fn record_entry(entry: &EntryStats, outcome: &RequestOutcome) {
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
    entry
        .cache_read_tokens
        .fetch_add(outcome.cache_read_tokens, Ordering::Relaxed);
    entry
        .cache_write_tokens
        .fetch_add(outcome.cache_write_tokens, Ordering::Relaxed);
    if let Ok(mut guard) = entry.latency_ms.lock() {
        let histogram = guard.get_or_insert_with(new_histogram);
        let _ = histogram.record(outcome.latency_ms.max(1));
    }
    if let Some(ttft) = outcome.ttft_ms
        && let Ok(mut guard) = entry.ttft_ms.lock()
    {
        let histogram = guard.get_or_insert_with(new_histogram);
        let _ = histogram.record(ttft.max(1));
    }
    if let Some(tps) = tokens_per_sec(outcome)
        && let Ok(mut guard) = entry.tokens_per_sec_x100.lock()
    {
        let centi = (tps * 100.0).round().clamp(1.0, 1_000_000_000.0) as u64;
        let histogram = guard.get_or_insert_with(new_tps_histogram);
        let _ = histogram.record(centi);
        entry
            .tokens_per_sec_sum_x100
            .fetch_add(centi, Ordering::Relaxed);
        entry
            .tokens_per_sec_samples
            .fetch_add(1, Ordering::Relaxed);
    }
}

/// Tokens generated per second during the "generation" window.
///
/// Streaming: wall time after the first token. Non-stream: full request latency.
#[must_use]
pub fn tokens_per_sec(outcome: &RequestOutcome) -> Option<f64> {
    if outcome.tokens_out == 0 || outcome.latency_ms == 0 {
        return None;
    }
    let gen_ms = match outcome.ttft_ms {
        Some(ttft) if outcome.latency_ms > ttft => outcome.latency_ms - ttft,
        _ => outcome.latency_ms,
    };
    if gen_ms == 0 {
        return None;
    }
    Some(outcome.tokens_out as f64 * 1000.0 / gen_ms as f64)
}

fn new_histogram() -> Histogram<u64> {
    // 1ms..1h, 3 significant digits — a few KB per entry.
    Histogram::new_with_bounds(1, 3_600_000, 3).expect("static histogram bounds are valid")
}

fn new_tps_histogram() -> Histogram<u64> {
    // 0.01 tok/s .. 100_000 tok/s stored as centi-tps.
    Histogram::new_with_bounds(1, 10_000_000, 3).expect("static tps histogram bounds are valid")
}

fn snapshot_map(map: &DashMap<String, EntryStats>) -> Vec<EntrySnapshot> {
    let mut rows: Vec<EntrySnapshot> = map
        .iter()
        .map(|entry| entry_snapshot(entry.key(), entry.value()))
        .collect();
    rows.sort_by_key(|row| std::cmp::Reverse(row.requests));
    rows
}

fn entry_snapshot(key: &str, stats: &EntryStats) -> EntrySnapshot {
    let (p50_ms, p95_ms) = latency_percentiles(&stats.latency_ms);
    let (ttft_p50_ms, ttft_p95_ms) = latency_percentiles(&stats.ttft_ms);
    let (tokens_per_sec_p50, tokens_per_sec_avg) = tps_summary(stats);
    EntrySnapshot {
        key: key.to_string(),
        requests: stats.requests.load(Ordering::Relaxed),
        errors: stats.errors.load(Ordering::Relaxed),
        tokens_in: stats.tokens_in.load(Ordering::Relaxed),
        tokens_out: stats.tokens_out.load(Ordering::Relaxed),
        cache_read_tokens: stats.cache_read_tokens.load(Ordering::Relaxed),
        cache_write_tokens: stats.cache_write_tokens.load(Ordering::Relaxed),
        p50_ms,
        p95_ms,
        ttft_p50_ms,
        ttft_p95_ms,
        tokens_per_sec_p50,
        tokens_per_sec_avg,
    }
}

fn overview_from_entry(stats: &EntryStats) -> OverviewSnapshot {
    let snap = entry_snapshot("overview", stats);
    let cache_hit_rate_pct = if snap.tokens_in == 0 {
        0.0
    } else {
        (snap.cache_read_tokens as f64 / snap.tokens_in as f64) * 100.0
    };
    OverviewSnapshot {
        requests: snap.requests,
        errors: snap.errors,
        tokens_in: snap.tokens_in,
        tokens_out: snap.tokens_out,
        cache_read_tokens: snap.cache_read_tokens,
        cache_write_tokens: snap.cache_write_tokens,
        cache_hit_rate_pct,
        p50_ms: snap.p50_ms,
        p95_ms: snap.p95_ms,
        ttft_p50_ms: snap.ttft_p50_ms,
        ttft_p95_ms: snap.ttft_p95_ms,
        tokens_per_sec_p50: snap.tokens_per_sec_p50,
        tokens_per_sec_avg: snap.tokens_per_sec_avg,
    }
}

fn latency_percentiles(lock: &Mutex<Option<Histogram<u64>>>) -> (u64, u64) {
    match lock.lock() {
        Ok(guard) => guard.as_ref().map_or((0, 0), |h| {
            (h.value_at_quantile(0.5), h.value_at_quantile(0.95))
        }),
        Err(_) => (0, 0),
    }
}

fn tps_summary(stats: &EntryStats) -> (f64, f64) {
    let p50 = match stats.tokens_per_sec_x100.lock() {
        Ok(guard) => guard
            .as_ref()
            .map_or(0.0, |h| h.value_at_quantile(0.5) as f64 / 100.0),
        Err(_) => 0.0,
    };
    let samples = stats.tokens_per_sec_samples.load(Ordering::Relaxed);
    let avg = if samples == 0 {
        0.0
    } else {
        stats.tokens_per_sec_sum_x100.load(Ordering::Relaxed) as f64 / (samples as f64 * 100.0)
    };
    (p50, avg)
}

/// Scrapes token usage out of a response tail — works for both plain JSON
/// bodies and the terminal SSE usage chunk, without full JSON parsing of
/// arbitrarily large streams.
pub fn scrape_usage(tail: &[u8]) -> ScrapedUsage {
    let text = String::from_utf8_lossy(tail);
    let Some(usage_pos) = text.rfind("\"usage\"") else {
        return ScrapedUsage::default();
    };
    let after = &text[usage_pos..];
    // Prefer OpenAI names; fall back to Anthropic names when present.
    let tokens_in = first_nonzero(
        extract_int(after, "\"prompt_tokens\""),
        extract_int(after, "\"input_tokens\""),
    );
    let tokens_out = first_nonzero(
        extract_int(after, "\"completion_tokens\""),
        extract_int(after, "\"output_tokens\""),
    );
    let cache_read_tokens = first_nonzero(
        extract_int(after, "\"cached_tokens\""),
        extract_int(after, "\"cache_read_input_tokens\""),
    );
    let cache_write_tokens = extract_int(after, "\"cache_creation_input_tokens\"");
    ScrapedUsage {
        tokens_in,
        tokens_out,
        cache_read_tokens,
        cache_write_tokens,
    }
}

fn first_nonzero(a: u64, b: u64) -> u64 {
    if a > 0 { a } else { b }
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
            ttft_ms: Some(40),
            tokens_in: 10,
            tokens_out: 20,
            cache_read_tokens: 4,
            cache_write_tokens: 0,
        }
    }

    #[test]
    fn records_and_snapshots() {
        let registry = StatsRegistry::default();
        registry.record(&outcome("openai", "openai/gpt-4o", 200));
        registry.record(&outcome("openai", "openai/gpt-4o", 500));
        let snapshot = registry.snapshot("live", false);
        assert_eq!(snapshot.profiles[0].requests, 2);
        assert_eq!(snapshot.profiles[0].errors, 1);
        assert_eq!(snapshot.profiles[0].tokens_in, 20);
        assert_eq!(snapshot.profiles[0].cache_read_tokens, 8);
        assert!(snapshot.profiles[0].p50_ms >= 100);
        assert!(snapshot.profiles[0].ttft_p50_ms >= 40);
        assert!(snapshot.profiles[0].tokens_per_sec_avg > 0.0);
        assert_eq!(snapshot.overview.requests, 2);
    }

    #[test]
    fn caps_model_cardinality() {
        let registry = StatsRegistry::default();
        for i in 0..(STATS_MAX_MODEL_KEYS + 50) {
            registry.record(&outcome("p", &format!("p/model-{i}"), 200));
        }
        let snapshot = registry.snapshot("live", false);
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
        assert_eq!(
            scrape_usage(body),
            ScrapedUsage {
                tokens_in: 12,
                tokens_out: 34,
                ..ScrapedUsage::default()
            }
        );
    }

    #[test]
    fn scrapes_cache_tokens_openai_and_anthropic() {
        let openai = br#"{"usage":{"prompt_tokens":100,"completion_tokens":10,"prompt_tokens_details":{"cached_tokens":80}}}"#;
        assert_eq!(scrape_usage(openai).cache_read_tokens, 80);

        let anthropic = br#"{"usage":{"input_tokens":100,"output_tokens":10,"cache_read_input_tokens":60,"cache_creation_input_tokens":15}}"#;
        let scraped = scrape_usage(anthropic);
        assert_eq!(scraped.tokens_in, 100);
        assert_eq!(scraped.tokens_out, 10);
        assert_eq!(scraped.cache_read_tokens, 60);
        assert_eq!(scraped.cache_write_tokens, 15);
    }

    #[test]
    fn scrapes_sse_terminal_usage_chunk() {
        let body = b"data: {\"choices\":[]}\n\ndata: {\"choices\":[],\"usage\":{\"prompt_tokens\":7,\"completion_tokens\":9}}\n\ndata: [DONE]\n\n";
        assert_eq!(
            scrape_usage(body),
            ScrapedUsage {
                tokens_in: 7,
                tokens_out: 9,
                ..ScrapedUsage::default()
            }
        );
    }

    #[test]
    fn no_usage_yields_zero() {
        assert_eq!(scrape_usage(b"data: [DONE]"), ScrapedUsage::default());
    }

    #[test]
    fn tokens_per_sec_uses_post_ttft_window() {
        let mut o = outcome("p", "p/m", 200);
        o.latency_ms = 140;
        o.ttft_ms = Some(40);
        o.tokens_out = 50;
        // 50 tokens in 100ms => 500 tok/s
        assert!((tokens_per_sec(&o).unwrap() - 500.0).abs() < 0.01);
    }
}
