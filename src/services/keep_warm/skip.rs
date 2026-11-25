use std::collections::{BTreeMap, HashMap};
use std::time::Duration;

use crate::schemas::model_id::ModelId;

/// Characters of a worker or stream key a log line shows.
const SHORT_KEY_CHARS: usize = 12;

/// Why keep-warm did not keep, dropped or held back a stream. The names are stable log and stats values.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub(crate) enum SkipReason {
    /// The response cached fewer prompt tokens than the floor.
    NotKeptLowCache,
    /// The request body is over the per-stream size cap.
    NotKeptBodyCap,
    /// The request body could not be turned into a ping.
    NotKeptPingBody,
    /// The request body has no stream key.
    NotKeptNoStreamKey,
    /// The model's cache discount cannot pay for the pings.
    NotKeptUnpaid,
    /// A reviewed turn was buffered and never completed the lease.
    NotKeptReviewPath,
    /// The upstream stream did not end cleanly or ended in an error.
    NotKeptIncompleteStream,
    /// The request named no worker or session.
    NotKeptNoWorker,
    /// The stream or byte bound evicted the least recently used stream.
    EvictedLru,
    /// The stream stayed idle past the idle cap.
    ExpiredIdleCap,
    /// The model's past idle gaps say the user is unlikely to come back before the next ping.
    DroppedUnlikelyReturn,
    /// A ping failed.
    DroppedPingFailed,
    /// The profile turned keep-warm off, was disabled or was removed.
    DroppedProfileOff,
    /// Keep-warm was switched off.
    DroppedKeepWarmOff,
    /// A due ping waits for a user request in flight on the same worker.
    HeldBusyWorker,
    /// A due ping waits for the previous ping of the stream.
    HeldPingInFlight,
    /// A due ping waits for the global ping budget.
    HeldRateLimit,
}

impl SkipReason {
    pub(crate) const fn as_str(self) -> &'static str {
        match self {
            SkipReason::NotKeptLowCache => "not_kept_low_cache",
            SkipReason::NotKeptBodyCap => "not_kept_body_cap",
            SkipReason::NotKeptPingBody => "not_kept_ping_body",
            SkipReason::NotKeptNoStreamKey => "not_kept_no_stream_key",
            SkipReason::NotKeptUnpaid => "not_kept_unpaid",
            SkipReason::NotKeptReviewPath => "not_kept_review_path",
            SkipReason::NotKeptIncompleteStream => "not_kept_incomplete_stream",
            SkipReason::NotKeptNoWorker => "not_kept_no_worker",
            SkipReason::EvictedLru => "evicted_lru",
            SkipReason::ExpiredIdleCap => "expired_idle_cap",
            SkipReason::DroppedUnlikelyReturn => "dropped_unlikely_return",
            SkipReason::DroppedPingFailed => "dropped_ping_failed",
            SkipReason::DroppedProfileOff => "dropped_profile_off",
            SkipReason::DroppedKeepWarmOff => "dropped_keep_warm_off",
            SkipReason::HeldBusyWorker => "held_busy_worker",
            SkipReason::HeldPingInFlight => "held_ping_in_flight",
            SkipReason::HeldRateLimit => "held_rate_limit",
        }
    }
}

/// One skip, or the release of a held stream, ready to log once the registry lock is free.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct SkipEvent {
    pub(crate) reason: SkipReason,
    pub(crate) worker: String,
    pub(crate) stream: Option<String>,
    pub(crate) model: String,
    pub(crate) tokens: Option<u64>,
    pub(crate) bytes: Option<usize>,
    pub(crate) idle: Option<Duration>,
    /// Set when a held stream is released: how long it was held.
    pub(crate) released_after: Option<Duration>,
}

impl SkipEvent {
    pub(crate) fn new(
        reason: SkipReason,
        worker: &str,
        stream: Option<&str>,
        model: &ModelId,
    ) -> Self {
        Self {
            reason,
            worker: short(worker),
            stream: stream.map(short),
            model: model.qualified(),
            tokens: None,
            bytes: None,
            idle: None,
            released_after: None,
        }
    }

    pub(crate) fn tokens(mut self, tokens: u64) -> Self {
        self.tokens = Some(tokens);
        self
    }

    pub(crate) fn bytes(mut self, bytes: usize) -> Self {
        self.bytes = Some(bytes);
        self
    }

    pub(crate) fn idle(mut self, idle: Duration) -> Self {
        self.idle = Some(idle);
        self
    }

    pub(crate) fn released(mut self, held: Duration) -> Self {
        self.released_after = Some(held);
        self
    }

    /// Writes the event. A missing worker is common and says nothing about one stream, so it logs at debug.
    pub(crate) fn log(&self) {
        let reason = self.reason.as_str();
        let stream = self.stream.as_deref().unwrap_or("-");
        let idle_secs = self.idle.map(|idle| idle.as_secs());
        if let Some(held) = self.released_after {
            tracing::info!(
                worker = %self.worker, stream, model = %self.model, reason,
                held_secs = held.as_secs(), "keep-warm stream released"
            );
        } else if self.reason == SkipReason::NotKeptNoWorker {
            tracing::debug!(model = %self.model, reason, "keep-warm stream skipped");
        } else {
            tracing::info!(
                worker = %self.worker, stream, model = %self.model, reason,
                tokens = self.tokens, bytes = self.bytes, idle_secs, "keep-warm stream skipped"
            );
        }
    }
}

/// Skip counts since startup, and the events still to log.
#[derive(Debug, Default)]
pub(crate) struct SkipLog {
    counts: HashMap<SkipReason, u64>,
    pending: Vec<SkipEvent>,
}

impl SkipLog {
    /// Counts a skip and queues it for the log. A release is logged but not counted.
    pub(crate) fn note(&mut self, event: SkipEvent) {
        if event.released_after.is_none() {
            *self.counts.entry(event.reason).or_default() += 1;
        }
        self.pending.push(event);
    }

    pub(crate) fn take_pending(&mut self) -> Vec<SkipEvent> {
        std::mem::take(&mut self.pending)
    }

    /// Counts per reason name, for the stats summary.
    pub(crate) fn counts(&self) -> BTreeMap<String, u64> {
        self.counts
            .iter()
            .map(|(reason, count)| (reason.as_str().to_owned(), *count))
            .collect()
    }
}

fn short(key: &str) -> String {
    key.chars().take(SHORT_KEY_CHARS).collect()
}

#[cfg(test)]
mod tests {
    use std::io;
    use std::sync::{Arc, Mutex};

    use super::*;

    #[derive(Clone, Default)]
    struct CapturedLogs(Arc<Mutex<Vec<u8>>>);

    impl CapturedLogs {
        fn text(&self) -> String {
            String::from_utf8(self.0.lock().unwrap().clone()).unwrap()
        }
    }

    impl io::Write for CapturedLogs {
        fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
            self.0.lock().unwrap().extend_from_slice(bytes);
            Ok(bytes.len())
        }

        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }

    impl<'a> tracing_subscriber::fmt::MakeWriter<'a> for CapturedLogs {
        type Writer = CapturedLogs;

        fn make_writer(&'a self) -> Self::Writer {
            self.clone()
        }
    }

    fn model() -> ModelId {
        ModelId::parse("copilot/claude-haiku-5-5").unwrap()
    }

    fn logged(event: &SkipEvent) -> String {
        let logs = CapturedLogs::default();
        let _guard = tracing::subscriber::set_default(
            tracing_subscriber::fmt()
                .with_writer(logs.clone())
                .with_max_level(tracing::Level::DEBUG)
                .with_ansi(false)
                .finish(),
        );
        event.log();
        logs.text()
    }

    #[test]
    fn every_reason_has_a_distinct_snake_case_name() {
        let all = [
            SkipReason::NotKeptLowCache,
            SkipReason::NotKeptBodyCap,
            SkipReason::NotKeptPingBody,
            SkipReason::NotKeptNoStreamKey,
            SkipReason::NotKeptUnpaid,
            SkipReason::NotKeptReviewPath,
            SkipReason::NotKeptIncompleteStream,
            SkipReason::NotKeptNoWorker,
            SkipReason::EvictedLru,
            SkipReason::ExpiredIdleCap,
            SkipReason::DroppedUnlikelyReturn,
            SkipReason::DroppedPingFailed,
            SkipReason::DroppedProfileOff,
            SkipReason::DroppedKeepWarmOff,
            SkipReason::HeldBusyWorker,
            SkipReason::HeldPingInFlight,
            SkipReason::HeldRateLimit,
        ];
        let names: std::collections::HashSet<&str> =
            all.iter().map(|reason| reason.as_str()).collect();
        assert_eq!(names.len(), all.len());
        for name in names {
            assert!(
                name.chars().all(|c| c.is_ascii_lowercase() || c == '_'),
                "{name}"
            );
        }
    }

    #[test]
    fn a_skip_logs_short_keys_the_model_and_its_sizes_at_info() {
        let event = SkipEvent::new(
            SkipReason::ExpiredIdleCap,
            "worker-0123456789abcdef",
            Some("0123456789abcdef"),
            &model(),
        )
        .tokens(4096)
        .bytes(512)
        .idle(Duration::from_mins(30));

        let output = logged(&event);

        assert!(output.contains("INFO"), "{output}");
        assert!(output.contains("reason=\"expired_idle_cap\""), "{output}");
        assert!(output.contains("worker=worker-01234"), "{output}");
        assert!(!output.contains("worker-0123456789abcdef"), "{output}");
        assert!(output.contains("stream=\"0123456789ab\""), "{output}");
        assert!(
            output.contains("model=copilot/claude-haiku-5-5"),
            "{output}"
        );
        assert!(output.contains("tokens=4096"), "{output}");
        assert!(output.contains("bytes=512"), "{output}");
        assert!(output.contains("idle_secs=1800"), "{output}");
    }

    #[test]
    fn a_release_logs_how_long_the_stream_was_held() {
        let event = SkipEvent::new(SkipReason::HeldBusyWorker, "w", Some("s"), &model())
            .released(Duration::from_secs(90));

        let output = logged(&event);

        assert!(output.contains("keep-warm stream released"), "{output}");
        assert!(output.contains("reason=\"held_busy_worker\""), "{output}");
        assert!(output.contains("held_secs=90"), "{output}");
    }

    #[test]
    fn a_missing_worker_logs_at_debug_only() {
        let event = SkipEvent::new(SkipReason::NotKeptNoWorker, "", None, &model());

        let output = logged(&event);

        assert!(output.contains("DEBUG"), "{output}");
        assert!(output.contains("reason=\"not_kept_no_worker\""), "{output}");
    }

    #[test]
    fn the_log_counts_skips_but_not_releases_and_drains_its_queue() {
        let mut log = SkipLog::default();
        let skip = SkipEvent::new(SkipReason::HeldRateLimit, "w", Some("s"), &model());
        log.note(skip.clone());
        log.note(skip.clone());
        log.note(skip.released(Duration::from_secs(6)));

        assert_eq!(log.take_pending().len(), 3);
        assert!(log.take_pending().is_empty());
        assert_eq!(
            log.counts(),
            BTreeMap::from([("held_rate_limit".to_owned(), 2)])
        );
    }
}
