use std::collections::{HashMap, HashSet};
use std::sync::Mutex;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use serde::Serialize;

use crate::schemas::model_context::strip_window_suffix;
use crate::services::rate_limit::HINT_CAP;

/// Why an attempt counts against a model.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum FailureKind {
    /// No first byte inside the time limit.
    Timeout,
    /// The request never got an answer: connect, TLS or read error.
    Transport,
    /// The upstream answered with a status that describes the model, not the request.
    Status(u16),
    /// A success status with an empty body or an error event before any content.
    BrokenReply,
}

impl FailureKind {
    /// The attempt sender reports a timeout as text that starts with `timeout`, and the first
    /// event check a broken reply as [`BROKEN_REPLY_PREFIXES`].
    #[must_use]
    pub fn from_reason(reason: &str) -> FailureKind {
        if reason.starts_with("timeout") {
            FailureKind::Timeout
        } else if BROKEN_REPLY_PREFIXES
            .iter()
            .any(|prefix| reason.starts_with(prefix))
        {
            FailureKind::BrokenReply
        } else {
            FailureKind::Transport
        }
    }

    /// 404, 408, 429 and 5xx. A 400, 401, 403 or 422 describes the request or the account, so
    /// another model would answer it the same way.
    #[must_use]
    pub fn from_status(status: u16) -> Option<FailureKind> {
        let model_side = matches!(status, 404 | 408 | 429) || (500..=599).contains(&status);
        model_side.then_some(FailureKind::Status(status))
    }
}

/// The reason prefixes of a 2xx reply that carried no answer.
pub const BROKEN_REPLY_PREFIXES: [&str; 2] = ["empty reply", "error event"];

#[derive(Debug, Default)]
struct Entry {
    failures: u32,
    cooled_until: Option<Instant>,
    /// Why the model is cooled, when an upstream hint set the wait. `None` for the doubling backoff.
    cooled_reason: Option<String>,
    last_failure: Option<FailureKind>,
    error_rate: f64,
    outcomes: u32,
    last_outcome: Option<Instant>,
    ttft_ewma_ms: Option<f64>,
    samples: u32,
    last_sample: Option<Instant>,
}

/// One `(model, endpoint)` as the health API shows it.
#[derive(Debug, Clone, Serialize)]
pub struct HealthRow {
    pub model: String,
    pub endpoint: String,
    /// Failures in a row. 0 after an answer.
    pub failures: u32,
    pub last_failure: Option<FailureKind>,
    /// Milliseconds left before Smart Auto picks the model again. 0 when it is not cooled.
    pub cooled_for_ms: u64,
    /// Unix milliseconds when the cooldown ends. `None` when it is not cooled.
    pub cooled_until_ms: Option<u64>,
    /// Why the model is cooled, when an upstream said how long: a rate limit reset. `None` for the doubling backoff.
    pub cooled_reason: Option<String>,
    /// Share of recent attempts that failed on the model's side, 0 to 1.
    pub error_rate: f64,
    pub outcomes: u32,
    /// Average time to the first byte of a streamed answer.
    pub ttft_ms: Option<u64>,
    pub samples: u32,
    /// 0 (slow) to 10 (fast), the scale of the built-in speed seed. Set once enough samples exist.
    pub speed: Option<u8>,
}

/// A profile the hub skips as a whole, such as one out of credits.
#[derive(Debug, Clone, Serialize)]
pub struct ProfileCooldownRow {
    pub profile: String,
    pub cooled_for_ms: u64,
    pub cooled_until_ms: u64,
    pub reason: String,
}

#[derive(Debug)]
struct ProfileCooldown {
    until: Instant,
    reason: String,
}

/// What the router reads for one endpoint.
#[derive(Debug, Default)]
pub struct HealthView {
    /// Models Smart Auto skips now.
    pub cooled: HashSet<String>,
    /// Profiles Smart Auto skips now, with every model.
    pub cooled_profiles: HashSet<String>,
    /// Measured speed scores, 0 to 10.
    pub speeds: HashMap<String, u8>,
    /// Recent failure share, 0 to 1.
    pub error_rates: HashMap<String, f64>,
}

/// In-memory failure, error-rate and speed record of each `(model, endpoint)`.
/// A failure cools the model for a doubling time. An answer clears the cooldown. A streamed
/// answer adds a time-to-first-byte sample. Nothing here reads a disk or a network: the
/// request path takes one short lock.
#[derive(Debug, Default)]
pub struct ModelHealth {
    entries: Mutex<HashMap<(String, String), Entry>>,
    profiles: Mutex<HashMap<String, ProfileCooldown>>,
}

#[allow(
    clippy::cast_possible_truncation,
    clippy::cast_sign_loss,
    clippy::cast_precision_loss,
    reason = "Times are milliseconds and scores run 0 to 10, so the float casts cannot overflow"
)]
impl ModelHealth {
    const COOLDOWN_BASE: Duration = Duration::from_secs(30);
    const COOLDOWN_CAP: Duration = Duration::from_mins(10);
    const MAX_DOUBLINGS: u32 = 5;
    /// Samples needed before a measured speed replaces the seed.
    const MIN_SAMPLES: u32 = 3;
    /// Attempts needed before an error rate counts.
    const MIN_OUTCOMES: u32 = 5;
    /// A reading older than this is stale: a model that is not picked is never re-measured.
    const TTL: Duration = Duration::from_mins(30);
    const SPEED_WEIGHT: f64 = 0.3;
    const ERROR_WEIGHT: f64 = 0.1;
    /// Upper bound of each speed score. Slower than the last bound scores 0.
    const SPEED_BOUNDS_MS: [u64; 7] = [1_000, 2_000, 3_000, 5_000, 8_000, 12_000, 20_000];
    const SPEED_STEPS: [u8; 7] = [10, 9, 8, 7, 5, 3, 1];

    /// The speed score of a time to first byte.
    #[must_use]
    pub fn speed_score(ttft_ms: u64) -> u8 {
        Self::SPEED_BOUNDS_MS
            .iter()
            .position(|bound| ttft_ms <= *bound)
            .map_or(0, |index| Self::SPEED_STEPS[index])
    }

    /// Cools `model` on `endpoint`. Each failure in a row doubles the wait, up to ten minutes.
    pub fn record_failure(&self, model: &str, endpoint: &str, kind: FailureKind, now: Instant) {
        let mut entries = self.lock();
        let entry = entries.entry(Self::key(model, endpoint)).or_default();
        entry.failures = entry.failures.saturating_add(1);
        entry.last_failure = Some(kind);
        let doublings = (entry.failures - 1).min(Self::MAX_DOUBLINGS);
        let wait = (Self::COOLDOWN_BASE * 2u32.pow(doublings)).min(Self::COOLDOWN_CAP);
        entry.cooled_until = Some(now + wait);
        entry.cooled_reason = None;
        Self::count_outcome(entry, 1.0, now);
    }

    /// Counts a failure like [`ModelHealth::record_failure`], but cools `model` for the `wait`
    /// the upstream named, at most six hours, instead of the doubling backoff.
    pub fn record_failure_until(
        &self,
        model: &str,
        endpoint: &str,
        kind: FailureKind,
        wait: Duration,
        reason: String,
        now: Instant,
    ) {
        let mut entries = self.lock();
        let entry = entries.entry(Self::key(model, endpoint)).or_default();
        entry.failures = entry.failures.saturating_add(1);
        entry.last_failure = Some(kind);
        entry.cooled_until = Some(now + wait.min(HINT_CAP));
        entry.cooled_reason = Some(reason);
        Self::count_outcome(entry, 1.0, now);
    }

    /// Sets the cooldown of a failure already counted to the `wait` an error body named.
    pub fn set_cooldown(
        &self,
        model: &str,
        endpoint: &str,
        wait: Duration,
        reason: String,
        now: Instant,
    ) {
        let mut entries = self.lock();
        let entry = entries.entry(Self::key(model, endpoint)).or_default();
        entry.cooled_until = Some(now + wait.min(HINT_CAP));
        entry.cooled_reason = Some(reason);
    }

    /// Cools every model of `profile` for `wait`.
    pub fn cool_profile(&self, profile: &str, wait: Duration, reason: String, now: Instant) {
        self.lock_profiles().insert(
            profile.to_owned(),
            ProfileCooldown {
                until: now + wait,
                reason,
            },
        );
    }

    /// An answer arrived: the model, and its profile, are healthy again.
    pub fn record_success(&self, model: &str, endpoint: &str, now: Instant) {
        let mut entries = self.lock();
        let entry = entries.entry(Self::key(model, endpoint)).or_default();
        entry.failures = 0;
        entry.cooled_until = None;
        entry.cooled_reason = None;
        Self::count_outcome(entry, 0.0, now);
        drop(entries);
        if let Some((profile, _)) = model.split_once('/') {
            let mut profiles = self.lock_profiles();
            if !profiles.is_empty() {
                profiles.remove(profile);
            }
        }
    }

    /// Adds one time-to-first-byte sample of a streamed answer.
    pub fn record_ttft(&self, model: &str, endpoint: &str, ttft_ms: u64, now: Instant) {
        let mut entries = self.lock();
        let entry = entries.entry(Self::key(model, endpoint)).or_default();
        let sample = ttft_ms as f64;
        let fresh = Self::fresh(entry.last_sample, now);
        entry.ttft_ewma_ms = match entry.ttft_ewma_ms {
            Some(average) if fresh => Some(average + Self::SPEED_WEIGHT * (sample - average)),
            _ => Some(sample),
        };
        entry.samples = if fresh {
            entry.samples.saturating_add(1)
        } else {
            1
        };
        entry.last_sample = Some(now);
    }

    /// What the router reads for `endpoint`.
    #[must_use]
    pub fn view(&self, endpoint: &str, now: Instant) -> HealthView {
        let entries = self.lock();
        let mut view = HealthView::default();
        for ((model, entry_endpoint), entry) in entries.iter() {
            if entry_endpoint != endpoint {
                continue;
            }
            if entry.cooled_until.is_some_and(|until| until > now) {
                view.cooled.insert(model.clone());
            }
            if let Some(speed) = Self::speed(entry, now) {
                view.speeds.insert(model.clone(), speed);
            }
            if let Some(rate) = Self::error_rate(entry, now) {
                view.error_rates.insert(model.clone(), rate);
            }
        }
        drop(entries);
        view.cooled_profiles = self
            .lock_profiles()
            .iter()
            .filter(|(_, cooldown)| cooldown.until > now)
            .map(|(profile, _)| profile.clone())
            .collect();
        view
    }

    /// The profiles cooled now, the longest wait first.
    #[must_use]
    pub fn profile_report(&self, now: Instant) -> Vec<ProfileCooldownRow> {
        let now_ms = unix_ms_now();
        let mut rows: Vec<ProfileCooldownRow> = self
            .lock_profiles()
            .iter()
            .filter_map(|(profile, cooldown)| {
                let left = millis(cooldown.until.checked_duration_since(now)?);
                (left > 0).then(|| ProfileCooldownRow {
                    profile: profile.clone(),
                    cooled_for_ms: left,
                    cooled_until_ms: now_ms.saturating_add(left),
                    reason: cooldown.reason.clone(),
                })
            })
            .collect();
        rows.sort_by(|a, b| {
            b.cooled_for_ms
                .cmp(&a.cooled_for_ms)
                .then(a.profile.cmp(&b.profile))
        });
        rows
    }

    /// Every record, cooled models first, then the slowest.
    #[must_use]
    pub fn report(&self, now: Instant) -> Vec<HealthRow> {
        let now_ms = unix_ms_now();
        let entries = self.lock();
        let mut rows: Vec<HealthRow> = entries
            .iter()
            .map(|((model, endpoint), entry)| {
                let cooled_for_ms = entry
                    .cooled_until
                    .and_then(|until| until.checked_duration_since(now))
                    .map_or(0, millis);
                let cooled = cooled_for_ms > 0;
                HealthRow {
                    model: model.clone(),
                    endpoint: endpoint.clone(),
                    failures: entry.failures,
                    last_failure: entry.last_failure,
                    cooled_for_ms,
                    cooled_until_ms: cooled.then(|| now_ms.saturating_add(cooled_for_ms)),
                    cooled_reason: entry.cooled_reason.clone().filter(|_| cooled),
                    error_rate: Self::error_rate(entry, now).unwrap_or(0.0),
                    outcomes: entry.outcomes,
                    ttft_ms: entry
                        .ttft_ewma_ms
                        .filter(|_| Self::fresh(entry.last_sample, now))
                        .map(|average| average as u64),
                    samples: entry.samples,
                    speed: Self::speed(entry, now),
                }
            })
            .collect();
        rows.sort_by(|a, b| {
            b.cooled_for_ms
                .cmp(&a.cooled_for_ms)
                .then(b.ttft_ms.cmp(&a.ttft_ms))
                .then(a.model.cmp(&b.model))
        });
        rows
    }

    fn count_outcome(entry: &mut Entry, failed: f64, now: Instant) {
        let fresh = Self::fresh(entry.last_outcome, now);
        entry.error_rate = if fresh && entry.outcomes > 0 {
            entry.error_rate + Self::ERROR_WEIGHT * (failed - entry.error_rate)
        } else {
            failed
        };
        entry.outcomes = if fresh {
            entry.outcomes.saturating_add(1)
        } else {
            1
        };
        entry.last_outcome = Some(now);
    }

    fn fresh(last: Option<Instant>, now: Instant) -> bool {
        last.is_some_and(|last| now.duration_since(last) <= Self::TTL)
    }

    fn speed(entry: &Entry, now: Instant) -> Option<u8> {
        if !Self::fresh(entry.last_sample, now) || entry.samples < Self::MIN_SAMPLES {
            return None;
        }
        Some(Self::speed_score(entry.ttft_ewma_ms? as u64))
    }

    fn error_rate(entry: &Entry, now: Instant) -> Option<f64> {
        (Self::fresh(entry.last_outcome, now) && entry.outcomes >= Self::MIN_OUTCOMES)
            .then_some(entry.error_rate)
    }

    fn key(model: &str, endpoint: &str) -> (String, String) {
        (strip_window_suffix(model).0.to_owned(), endpoint.to_owned())
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, HashMap<(String, String), Entry>> {
        self.entries
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    fn lock_profiles(&self) -> std::sync::MutexGuard<'_, HashMap<String, ProfileCooldown>> {
        self.profiles
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }
}

fn millis(duration: Duration) -> u64 {
    u64::try_from(duration.as_millis()).unwrap_or(u64::MAX)
}

fn unix_ms_now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, millis)
}

#[cfg(test)]
mod tests {
    use super::*;

    const MODEL: &str = "llmgw/azure/gpt-5.4-mini";
    const MESSAGES: &str = "/v1/messages";

    fn cooled_ms(health: &ModelHealth, now: Instant) -> u64 {
        health
            .report(now)
            .into_iter()
            .find(|row| row.model == MODEL)
            .map_or(0, |row| row.cooled_for_ms)
    }

    #[test]
    fn a_failure_cools_the_model_and_a_success_clears_it() {
        let health = ModelHealth::default();
        let now = Instant::now();
        health.record_failure(MODEL, MESSAGES, FailureKind::Timeout, now);
        assert!(health.view(MESSAGES, now).cooled.contains(MODEL));
        health.record_success(MODEL, MESSAGES, now);
        assert!(health.view(MESSAGES, now).cooled.is_empty());
    }

    #[test]
    fn the_cooldown_ends_by_itself() {
        let health = ModelHealth::default();
        let now = Instant::now();
        health.record_failure(MODEL, MESSAGES, FailureKind::Timeout, now);
        let later = now + ModelHealth::COOLDOWN_BASE + Duration::from_millis(1);
        assert!(health.view(MESSAGES, later).cooled.is_empty());
    }

    #[test]
    fn failures_in_a_row_double_the_wait_up_to_the_cap() {
        let health = ModelHealth::default();
        let now = Instant::now();
        let mut waits = Vec::new();
        for _ in 0..8 {
            health.record_failure(MODEL, MESSAGES, FailureKind::Status(503), now);
            waits.push(cooled_ms(&health, now));
        }
        assert_eq!(waits[..3], [30_000, 60_000, 120_000]);
        assert_eq!(waits[7], 600_000);
    }

    #[test]
    fn a_failure_on_one_endpoint_leaves_the_others_alone() {
        let health = ModelHealth::default();
        let now = Instant::now();
        health.record_failure(MODEL, "/v1/responses", FailureKind::Status(404), now);
        assert!(health.view(MESSAGES, now).cooled.is_empty());
        assert!(health.view("/v1/responses", now).cooled.contains(MODEL));
    }

    #[test]
    fn the_one_m_suffix_does_not_split_a_model() {
        let health = ModelHealth::default();
        let now = Instant::now();
        let long = "copilot/claude-opus-4.6[1m]";
        health.record_failure(long, MESSAGES, FailureKind::Timeout, now);
        assert!(
            health
                .view(MESSAGES, now)
                .cooled
                .contains("copilot/claude-opus-4.6")
        );
    }

    #[test]
    fn a_hinted_wait_replaces_the_backoff_and_shows_its_reason() {
        let health = ModelHealth::default();
        let now = Instant::now();
        health.record_failure_until(
            MODEL,
            MESSAGES,
            FailureKind::Status(429),
            Duration::from_secs(90),
            "rate limited: retry-after".into(),
            now,
        );
        let row = &health.report(now)[0];
        assert_eq!(row.cooled_for_ms, 90_000);
        assert_eq!(
            row.cooled_reason.as_deref(),
            Some("rate limited: retry-after")
        );
        assert!(row.cooled_until_ms.unwrap() > 90_000);
        assert!(
            health
                .view(MESSAGES, now + Duration::from_secs(89))
                .cooled
                .contains(MODEL)
        );
        assert!(
            health
                .view(MESSAGES, now + Duration::from_secs(91))
                .cooled
                .is_empty()
        );

        health.set_cooldown(
            MODEL,
            MESSAGES,
            Duration::from_hours(100),
            "long".into(),
            now,
        );
        assert_eq!(
            cooled_ms(&health, now),
            6 * 3_600_000,
            "capped at six hours"
        );

        health.record_failure(MODEL, MESSAGES, FailureKind::Status(429), now);
        let row = &health.report(now)[0];
        assert_eq!(row.cooled_reason, None, "the backoff has no reason");
        assert_eq!(row.failures, 2);
        let later = now + Duration::from_hours(1);
        let row = &health.report(later)[0];
        assert_eq!(
            (row.cooled_until_ms, row.cooled_reason.as_ref()),
            (None, None)
        );
    }

    #[test]
    fn a_cooled_profile_is_skipped_until_it_answers_again() {
        let health = ModelHealth::default();
        let now = Instant::now();
        health.cool_profile(
            "llmgw",
            Duration::from_hours(1),
            "out of credits".into(),
            now,
        );

        assert!(health.view(MESSAGES, now).cooled_profiles.contains("llmgw"));
        let rows = health.profile_report(now);
        assert_eq!(rows.len(), 1);
        assert_eq!(
            (
                rows[0].profile.as_str(),
                rows[0].cooled_for_ms,
                rows[0].reason.as_str()
            ),
            ("llmgw", 3_600_000, "out of credits")
        );
        assert!(
            health
                .profile_report(now + Duration::from_hours(2))
                .is_empty()
        );
        assert!(
            health
                .view(MESSAGES, now + Duration::from_hours(2))
                .cooled_profiles
                .is_empty()
        );

        health.record_success("other/m", MESSAGES, now);
        assert_eq!(health.profile_report(now).len(), 1);
        health.record_success(MODEL, MESSAGES, now);
        assert!(health.profile_report(now).is_empty());
    }

    #[test]
    fn only_model_side_statuses_count_as_failures() {
        for status in [404, 408, 429, 500, 502, 503, 529] {
            assert!(FailureKind::from_status(status).is_some(), "{status}");
        }
        for status in [200, 400, 401, 403, 422] {
            assert!(FailureKind::from_status(status).is_none(), "{status}");
        }
        assert_eq!(FailureKind::from_reason("timeout"), FailureKind::Timeout);
        assert_eq!(
            FailureKind::from_reason("connect error: refused"),
            FailureKind::Transport
        );
        assert_eq!(
            FailureKind::from_reason("empty reply: no content"),
            FailureKind::BrokenReply
        );
        assert_eq!(
            FailureKind::from_reason("error event: overloaded"),
            FailureKind::BrokenReply
        );
    }

    #[test]
    fn speed_scores_fall_as_the_first_byte_gets_slower() {
        let scores: Vec<u8> = [500, 1_500, 2_500, 4_000, 7_000, 10_000, 15_000, 40_000]
            .into_iter()
            .map(ModelHealth::speed_score)
            .collect();
        assert_eq!(scores, vec![10, 9, 8, 7, 5, 3, 1, 0]);
    }

    #[test]
    fn a_speed_needs_three_samples() {
        let health = ModelHealth::default();
        let now = Instant::now();
        health.record_ttft(MODEL, MESSAGES, 800, now);
        health.record_ttft(MODEL, MESSAGES, 900, now);
        assert!(health.view(MESSAGES, now).speeds.is_empty());
        health.record_ttft(MODEL, MESSAGES, 700, now);
        assert_eq!(health.view(MESSAGES, now).speeds.get(MODEL), Some(&10));
    }

    #[test]
    fn a_slow_run_pulls_the_average_down() {
        let health = ModelHealth::default();
        let now = Instant::now();
        for _ in 0..3 {
            health.record_ttft(MODEL, MESSAGES, 900, now);
        }
        for _ in 0..6 {
            health.record_ttft(MODEL, MESSAGES, 15_000, now);
        }
        let speed = *health.view(MESSAGES, now).speeds.get(MODEL).unwrap();
        assert!(speed <= 3, "speed {speed}");
    }

    #[test]
    fn a_stale_speed_is_ignored_and_restarts_the_average() {
        let health = ModelHealth::default();
        let now = Instant::now();
        for _ in 0..3 {
            health.record_ttft(MODEL, MESSAGES, 15_000, now);
        }
        let later = now + ModelHealth::TTL + Duration::from_secs(1);
        assert!(health.view(MESSAGES, later).speeds.is_empty());
        health.record_ttft(MODEL, MESSAGES, 500, later);
        let row = &health.report(later)[0];
        assert_eq!((row.samples, row.ttft_ms), (1, Some(500)));
    }

    #[test]
    fn the_error_rate_counts_failures_against_answers() {
        let health = ModelHealth::default();
        let now = Instant::now();
        for _ in 0..4 {
            health.record_success(MODEL, MESSAGES, now);
        }
        assert!(health.view(MESSAGES, now).error_rates.is_empty());
        health.record_success(MODEL, MESSAGES, now);
        assert_eq!(
            health.view(MESSAGES, now).error_rates.get(MODEL),
            Some(&0.0)
        );
        health.record_failure(MODEL, MESSAGES, FailureKind::Status(404), now);
        let rate = health.view(MESSAGES, now).error_rates[MODEL];
        assert!((rate - 0.1).abs() < 1e-9, "rate {rate}");
    }

    #[test]
    fn a_healthy_run_wears_an_old_error_rate_away() {
        let health = ModelHealth::default();
        let now = Instant::now();
        for _ in 0..6 {
            health.record_failure(MODEL, MESSAGES, FailureKind::Timeout, now);
        }
        let high = health.view(MESSAGES, now).error_rates[MODEL];
        for _ in 0..30 {
            health.record_success(MODEL, MESSAGES, now);
        }
        let low = health.view(MESSAGES, now).error_rates[MODEL];
        assert!(high > 0.9 && low < 0.1, "high {high} low {low}");
    }
}
