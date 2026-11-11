use tokio::sync::broadcast;

use crate::consts::LIVE_FEED_CAPACITY;
use crate::schemas::usage_row::UsageRow;
use crate::services::harness::is_keep_warm;
use crate::services::pricing::PricingBook;
use crate::services::stats::RequestOutcome;
use crate::services::store::now_ms;

pub struct LiveFeed {
    sender: broadcast::Sender<UsageRow>,
}

impl LiveFeed {
    #[must_use]
    pub fn new() -> Self {
        let (sender, _) = broadcast::channel(LIVE_FEED_CAPACITY);
        Self { sender }
    }

    #[must_use]
    pub fn subscribe(&self) -> broadcast::Receiver<UsageRow> {
        self.sender.subscribe()
    }

    /// Sends a usage row for a finished request. Without a listener it returns before it reads
    /// `pricing`, and keep-warm pings are never sent.
    pub fn publish(&self, outcome: &RequestOutcome, pricing: impl FnOnce() -> PricingBook) {
        if self.sender.receiver_count() == 0 || is_keep_warm(outcome.client.as_deref()) {
            return;
        }
        let book = pricing();
        let cost_usd = book.estimate(
            &outcome.model_key,
            outcome.tokens_in,
            outcome.tokens_out,
            outcome.cache_read_tokens,
            outcome.cache_write_tokens,
        );
        let row = UsageRow {
            ts_ms: now_ms(),
            model: outcome.model_key.clone(),
            profile: outcome.profile.clone(),
            status: outcome.status,
            latency_ms: outcome.latency_ms,
            ttft_ms: outcome.ttft_ms,
            tokens_in: outcome.tokens_in,
            tokens_out: outcome.tokens_out,
            cache_read_tokens: outcome.cache_read_tokens,
            cache_write_tokens: outcome.cache_write_tokens,
            cost_usd,
            error: outcome.error.clone(),
            client: outcome.client.clone(),
            worker: outcome.worker.clone(),
        };
        let _ = self.sender.send(row);
    }
}

impl Default for LiveFeed {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use std::cell::Cell;
    use std::collections::HashMap;

    use super::*;
    use crate::configs::HubConfig;
    use crate::consts::KEEP_WARM_CLIENT;

    fn outcome(client: Option<&str>) -> RequestOutcome {
        RequestOutcome {
            profile: "claude".into(),
            model_key: "claude/sonnet".into(),
            status: 200,
            ts_ms: 0,
            latency_ms: 42,
            ttft_ms: Some(7),
            tokens_in: 1_000,
            tokens_out: 500,
            cache_read_tokens: 0,
            cache_write_tokens: 0,
            error: None,
            client: client.map(str::to_string),
            worker: Some("worker-1".into()),
            routed_by: None,
            attempt: None,
            role: None,
        }
    }

    fn book() -> PricingBook {
        PricingBook::from_hub(&HubConfig::from_map(&HashMap::new()).unwrap())
    }

    #[test]
    fn publish_without_a_receiver_builds_nothing() {
        let feed = LiveFeed::new();
        let priced = Cell::new(false);
        feed.publish(&outcome(None), || {
            priced.set(true);
            book()
        });
        assert!(!priced.get());
    }

    #[test]
    fn a_receiver_gets_the_finished_request_as_a_usage_row() {
        let feed = LiveFeed::new();
        let mut receiver = feed.subscribe();
        feed.publish(&outcome(Some("claude-code")), book);
        let row = receiver.try_recv().unwrap();
        assert_eq!(row.model, "claude/sonnet");
        assert_eq!(row.profile, "claude");
        assert_eq!(row.status, 200);
        assert_eq!(row.latency_ms, 42);
        assert_eq!(row.ttft_ms, Some(7));
        assert_eq!(row.tokens_in, 1_000);
        assert_eq!(row.tokens_out, 500);
        assert_eq!(row.client.as_deref(), Some("claude-code"));
        assert_eq!(row.worker.as_deref(), Some("worker-1"));
        assert!(row.ts_ms > 0);
    }

    #[test]
    fn keep_warm_pings_are_not_published() {
        let feed = LiveFeed::new();
        let mut receiver = feed.subscribe();
        feed.publish(&outcome(Some(KEEP_WARM_CLIENT)), book);
        assert!(receiver.try_recv().is_err());
    }
}
