#[cfg(test)]
use axum::http::HeaderValue;
use std::collections::{BTreeMap, HashMap, VecDeque};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::time::{Duration, Instant};

use bytes::Bytes;

use crate::configs::{KeepWarmConfig, PriceCard, TokenRates};
use crate::consts::{
    KEEP_WARM_MAX_BODY_BYTES, KEEP_WARM_MAX_STREAMS, KEEP_WARM_MAX_TOTAL_BYTES,
    KEEP_WARM_MIN_CACHED_TOKENS, KEEP_WARM_PING_INTERVAL, KEEP_WARM_PINGS_PER_MINUTE,
    KEEP_WARM_TICK, PROMPT_CACHE_TTL_MS,
};
use crate::dependencies::state::AppState;
use crate::schemas::keep_warm_summary::KeepWarmSummary;
use crate::schemas::model_id::ModelId;
use crate::services::stats::{RequestOutcome, cache_write_label};

mod api_shape;
mod lease;
mod ping_plan;
mod skip;
mod warm_request;

pub(crate) use api_shape::{ApiShape, PingTarget};

pub(crate) use lease::KeepWarmLease;
pub(crate) use ping_plan::PingPlan;
use skip::SkipLog;
pub(crate) use skip::{Economics, SkipEvent, SkipReason};
pub(crate) use warm_request::WarmRequest;

/// Idle gaps kept per model for the stop rule.
const KEEP_WARM_MAX_GAPS: usize = 64;
/// Gaps a model needs before the stop rule applies.
const KEEP_WARM_MIN_GAPS: usize = 8;
/// A cache miss costs about nine reads when no price is known: input at 1.0 against a read at 0.1.
const KEEP_WARM_MISS_TO_PING_RATIO: f64 = 9.0;
/// Token prices are per this many tokens.
const PER_MILLION: f64 = 1_000_000.0;

type StreamKey = (String, String);

/// The prompt a completed response cached, and the price card it is billed at.
#[derive(Debug, Clone)]
pub(crate) struct CachedPrompt {
    pub(crate) tokens: u64,
    pub(crate) card: PriceCard,
}

/// Whether a ping pays, with the prices and the return odds it was decided on.
#[derive(Debug, Clone, Copy, PartialEq)]
struct PingCheck {
    pays: bool,
    /// The odds stay 0 when the model has too few gaps to judge.
    economics: Economics,
}

/// Keeps the last request body of each idle stream on a profile that caches prompts, so a ping
/// can replay it and refresh the cache. Everything here is in memory and bounded.
#[derive(Default)]
pub struct KeepWarm {
    state: Mutex<State>,
}

#[derive(Default)]
struct State {
    streams: HashMap<StreamKey, Stream>,
    total_bytes: usize,
    clock: u64,
    in_flight: HashMap<String, usize>,
    bucket: TokenBucket,
    pings: u64,
    ping_cost_usd: f64,
    /// Recent idle gaps between user requests of one stream, per model, newest last.
    gaps: HashMap<String, VecDeque<Duration>>,
    /// Streams not kept, dropped or held back, counted since startup.
    skips: SkipLog,
}

struct Stream {
    /// The ping body: the accepted request with `stream` false and the output capped.
    body: Bytes,
    model_id: ModelId,
    /// Where and how the ping goes.
    target: PingTarget,
    last_user: Instant,
    last_ping: Option<Instant>,
    ping_in_flight: bool,
    /// LRU clock value of the last touch or ping.
    used: u64,
    /// Prompt tokens the last user response cached, for the skip log.
    cached_tokens: u64,
    /// The price card the last user response was billed at. The ping cost uses its tier for the prompt size.
    card: PriceCard,
    /// Why a due ping is held back, and since when. Only the change is logged.
    held: Option<(SkipReason, Instant)>,
}

impl Stream {
    fn last_touch(&self) -> Instant {
        self.last_ping
            .map_or(self.last_user, |ping| ping.max(self.last_user))
    }
}

/// Global ping budget: a burst of `KEEP_WARM_PINGS_PER_MINUTE`, refilled evenly over a minute.
struct TokenBucket {
    tokens: f64,
    refilled_at: Option<Instant>,
}

impl Default for TokenBucket {
    fn default() -> Self {
        Self {
            tokens: f64::from(KEEP_WARM_PINGS_PER_MINUTE),
            refilled_at: None,
        }
    }
}

impl TokenBucket {
    fn refill(&mut self, now: Instant) {
        let capacity = f64::from(KEEP_WARM_PINGS_PER_MINUTE);
        if let Some(at) = self.refilled_at {
            let elapsed = now.saturating_duration_since(at).as_secs_f64();
            self.tokens = (self.tokens + elapsed * capacity / 60.0).min(capacity);
        }
        self.refilled_at = Some(now);
    }

    fn take(&mut self) -> bool {
        if self.tokens < 1.0 {
            return false;
        }
        self.tokens -= 1.0;
        true
    }
}

impl Stream {
    /// A stream is due once it has been quiet for the ping interval, with no request in flight for its
    /// worker and no ping already out.
    fn is_due(&self, now: Instant, worker_busy: bool) -> bool {
        !worker_busy
            && !self.ping_in_flight
            && now.saturating_duration_since(self.last_touch()) >= KEEP_WARM_PING_INTERVAL
    }

    /// Why a stream quiet for the ping interval still gets no ping, or `None` when it is not
    /// quiet yet or nothing holds it back.
    fn hold_reason(&self, now: Instant, worker_busy: bool) -> Option<SkipReason> {
        if now.saturating_duration_since(self.last_touch()) < KEEP_WARM_PING_INTERVAL {
            return None;
        }
        if worker_busy {
            Some(SkipReason::HeldBusyWorker)
        } else if self.ping_in_flight {
            Some(SkipReason::HeldPingInFlight)
        } else {
            None
        }
    }

    fn skip_event(&self, reason: SkipReason, key: &StreamKey) -> SkipEvent {
        SkipEvent::new(reason, &key.0, Some(&key.1), &self.model_id)
            .tokens(self.cached_tokens)
            .bytes(self.body.len())
    }
}

impl State {
    /// Records the idle gap that ended when a user request returned to a kept stream.
    fn record_gap(&mut self, model: &str, gap: Duration) {
        let gaps = self.gaps.entry(model.to_string()).or_default();
        if gaps.len() >= KEEP_WARM_MAX_GAPS {
            gaps.pop_front();
        }
        gaps.push_back(gap);
    }

    /// Whether the next ping is worth it. After `idle` quiet time, a ping pays when the chance
    /// that the user returns before the following ping, times the miss cost, beats the ping cost.
    /// Both costs use the stream's tier rate for its prompt size. Without a known price the miss
    /// costs `KEEP_WARM_MISS_TO_PING_RATIO` pings. A model with too few recorded gaps always pays.
    #[allow(
        clippy::cast_precision_loss,
        reason = "Prompt and gap counts stay far below 2^53"
    )]
    fn ping_pays(&self, stream: &Stream, idle: Duration) -> PingCheck {
        let rates = stream
            .card
            .rates_for_prompt(stream.cached_tokens)
            .with_cache_defaults();
        let prompt = stream.cached_tokens as f64;
        let ping_usd = prompt * rates.cache_read_per_1m / PER_MILLION;
        let miss_usd = prompt * rates.cache_write_per_1m / PER_MILLION;
        let ratio = if ping_usd > 0.0 {
            miss_usd / ping_usd
        } else {
            KEEP_WARM_MISS_TO_PING_RATIO
        };
        let Some(gaps) = self
            .gaps
            .get(&stream.model_id.qualified())
            .filter(|g| g.len() >= KEEP_WARM_MIN_GAPS)
        else {
            let economics = Economics {
                ping_usd,
                miss_usd,
                odds: 0.0,
            };
            return PingCheck {
                pays: true,
                economics,
            };
        };
        let later = gaps.iter().filter(|gap| **gap > idle).count();
        let soon = gaps
            .iter()
            .filter(|gap| **gap > idle && **gap <= idle + KEEP_WARM_PING_INTERVAL)
            .count();
        let odds = if later == 0 {
            0.0
        } else {
            soon as f64 / later as f64
        };
        PingCheck {
            pays: later > 0 && odds * ratio > 1.0,
            economics: Economics {
                ping_usd,
                miss_usd,
                odds,
            },
        }
    }

    fn take(&mut self, key: &StreamKey) -> Option<Stream> {
        let stream = self.streams.remove(key)?;
        self.total_bytes -= stream.body.len();
        Some(stream)
    }

    fn next_clock(&mut self) -> u64 {
        self.clock += 1;
        self.clock
    }

    /// Drops a kept stream and logs why. `now` adds its idle time since the last user request.
    fn drop_stream(&mut self, key: &StreamKey, reason: SkipReason, now: Option<Instant>) {
        self.drop_with(key, reason, now, None);
    }

    /// Drops a kept stream and logs why, with the `economics` that decided it when a price did.
    fn drop_with(
        &mut self,
        key: &StreamKey,
        reason: SkipReason,
        now: Option<Instant>,
        economics: Option<Economics>,
    ) {
        let Some(stream) = self.take(key) else {
            return;
        };
        let mut event = stream.skip_event(reason, key);
        if let Some(now) = now {
            event = event.idle(now.saturating_duration_since(stream.last_user));
        }
        if let Some(economics) = economics {
            event = event.economics(economics);
        }
        self.skips.note(event);
    }

    /// Logs once when a stream starts or stops being held back. `limited` are the due streams the
    /// ping budget ran out for.
    fn update_holds(&mut self, now: Instant, limited: &[StreamKey]) {
        for (key, stream) in &mut self.streams {
            let busy = self.in_flight.get(&key.0).is_some_and(|count| *count > 0);
            let held = if limited.contains(key) {
                Some(SkipReason::HeldRateLimit)
            } else {
                stream.hold_reason(now, busy)
            };
            if stream.held.map(|(reason, _)| reason) == held {
                continue;
            }
            if let Some((reason, since)) = stream.held {
                let event = stream
                    .skip_event(reason, key)
                    .released(now.saturating_duration_since(since));
                self.skips.note(event);
            }
            stream.held = held.map(|reason| (reason, now));
            if let Some(reason) = held {
                self.skips.note(stream.skip_event(reason, key));
            }
        }
    }

    /// Drops least recently used streams until both the stream and byte bounds hold.
    fn evict_to_bounds(&mut self) {
        while self.streams.len() > KEEP_WARM_MAX_STREAMS
            || self.total_bytes > KEEP_WARM_MAX_TOTAL_BYTES
        {
            let Some(oldest) = self
                .streams
                .iter()
                .min_by_key(|(_, stream)| stream.used)
                .map(|(key, _)| key.clone())
            else {
                return;
            };
            self.drop_stream(&oldest, SkipReason::EvictedLru, None);
        }
    }
}

impl KeepWarm {
    /// Marks one user request in flight for `worker`. Its streams are not pinged until the lease drops.
    pub(crate) fn begin(self: &Arc<Self>, worker: &str) -> KeepWarmLease {
        {
            let mut state = self.lock();
            *state.in_flight.entry(worker.to_string()).or_default() += 1;
        }
        KeepWarmLease::new(Arc::clone(self), worker)
    }

    fn finish(&self, worker: &str) {
        let mut state = self.lock();
        let Some(count) = state.in_flight.get_mut(worker) else {
            return;
        };
        *count = count.saturating_sub(1);
        if *count == 0 {
            state.in_flight.remove(worker);
        }
    }

    /// Keeps a native Messages stream for the next ping. See [`Self::touch_with`].
    #[cfg(test)]
    pub(crate) fn touch(
        &self,
        key: StreamKey,
        body: &[u8],
        model_id: &ModelId,
        started: Instant,
        cached_tokens: u64,
        beta: Option<HeaderValue>,
    ) {
        let prompt = CachedPrompt {
            tokens: cached_tokens,
            card: PriceCard::default(),
        };
        self.touch_with(
            key,
            body,
            model_id,
            started,
            prompt,
            PingTarget::messages(beta),
        );
    }

    /// The prompt a completed response cached, billed at `card`. Copilot reports no cache write, so
    /// its uncached input counts as the prompt it just wrote.
    #[must_use]
    pub(crate) fn cached_prompt(outcome: &RequestOutcome, card: PriceCard) -> CachedPrompt {
        let written = if outcome.model_key.starts_with("copilot/") {
            cache_write_label(outcome).1
        } else {
            0
        };
        CachedPrompt {
            tokens: outcome.cache_read_tokens + outcome.cache_write_tokens + written,
            card,
        }
    }

    /// Keeps the stream for the next ping. A response that cached too few prompt tokens, or a body
    /// over the size cap, drops the stream instead.
    pub(crate) fn touch_with(
        &self,
        key: StreamKey,
        body: &[u8],
        model_id: &ModelId,
        started: Instant,
        prompt: CachedPrompt,
        target: PingTarget,
    ) {
        let cached_tokens = prompt.tokens;
        let refused = if cached_tokens < KEEP_WARM_MIN_CACHED_TOKENS {
            Some(SkipReason::NotKeptLowCache)
        } else if body.len() > KEEP_WARM_MAX_BODY_BYTES {
            Some(SkipReason::NotKeptBodyCap)
        } else {
            None
        };
        let ping_body = refused
            .is_none()
            .then(|| target.shape.ping_body(body))
            .flatten();
        let mut state = self.lock();
        let Some(ping_body) = ping_body else {
            state.take(&key);
            let reason = refused.unwrap_or(SkipReason::NotKeptPingBody);
            let event = SkipEvent::new(reason, &key.0, Some(&key.1), model_id)
                .tokens(cached_tokens)
                .bytes(body.len());
            state.skips.note(event);
            Self::flush(state);
            return;
        };
        let previous = state.take(&key);
        if let Some(old) = &previous
            && let Some(gap) = started.checked_duration_since(old.last_user)
        {
            state.record_gap(&model_id.qualified(), gap);
        }
        let (last_user, last_ping, ping_in_flight, held) =
            previous.map_or((started, None, false, None), |old| {
                (
                    old.last_user.max(started),
                    old.last_ping,
                    old.ping_in_flight,
                    old.held,
                )
            });
        let used = state.next_clock();
        state.total_bytes += ping_body.len();
        state.streams.insert(
            key,
            Stream {
                body: ping_body,
                model_id: model_id.clone(),
                target,
                last_user,
                last_ping,
                ping_in_flight,
                used,
                cached_tokens,
                card: prompt.card,
                held,
            },
        );
        state.evict_to_bounds();
        Self::flush(state);
    }

    /// Counts and logs a stream the proxy did not keep. A kept stream with the same key stays.
    pub(crate) fn note(&self, event: SkipEvent) {
        let mut state = self.lock();
        state.skips.note(event);
        Self::flush(state);
    }

    /// Drops the kept stream at `key`, as a touch with no cached tokens did, and logs `event`.
    pub(crate) fn forget(&self, key: &StreamKey, event: SkipEvent) {
        let mut state = self.lock();
        state.take(key);
        state.skips.note(event);
        Self::flush(state);
    }

    /// Releases the lock, then logs the queued skip events, so a log write never holds up a request.
    fn flush(mut state: MutexGuard<'_, State>) {
        let events = state.skips.take_pending();
        drop(state);
        for event in &events {
            event.log();
        }
    }

    /// The ping body for an accepted Messages request: the same body with `stream` false and `max_tokens` 1.
    #[cfg(test)]
    pub(crate) fn ping_body(body: &[u8]) -> Option<Bytes> {
        ApiShape::Messages.ping_body(body)
    }

    /// Whether keeping a cache warm for `idle_cap` costs less than rewriting it. A ping reads the
    /// cached prompt at the cache-read rate, and one comes every ping interval. Resuming without
    /// a ping rewrites the prompt at the cache-write rate, or at the input rate when the card has
    /// no write price. A model with no known price gets the benefit of the doubt.
    #[must_use]
    pub(crate) fn pays_off(rates: TokenRates, idle_cap: Duration) -> bool {
        let rates = rates.with_cache_defaults();
        let (read, rewrite) = (rates.cache_read_per_1m, rates.cache_write_per_1m);
        if rewrite <= 0.0 {
            return true;
        }
        let pings = idle_cap.as_secs_f64() / KEEP_WARM_PING_INTERVAL.as_secs_f64();
        pings * read < rewrite - read
    }

    /// Drops the streams of every profile `keep` refuses: turned off, disabled or removed.
    pub fn retain_profiles(&self, keep: impl Fn(&str) -> bool) {
        let mut state = self.lock();
        let dropped: Vec<StreamKey> = state
            .streams
            .iter()
            .filter(|(_, stream)| !keep(&stream.model_id.profile))
            .map(|(key, _)| key.clone())
            .collect();
        for key in &dropped {
            state.drop_stream(key, SkipReason::DroppedProfileOff, None);
        }
        Self::flush(state);
    }

    /// Takes the streams due at `now`, most overdue first, within the global ping budget. A stream
    /// idle for `idle_cap` since its last user request is dropped.
    pub fn take_due(&self, now: Instant, idle_cap: Duration) -> Vec<PingPlan> {
        let mut state = self.lock();
        state.bucket.refill(now);
        let expired: Vec<StreamKey> = state
            .streams
            .iter()
            .filter(|(_, stream)| now.saturating_duration_since(stream.last_user) >= idle_cap)
            .map(|(key, _)| key.clone())
            .collect();
        for key in &expired {
            state.drop_stream(key, SkipReason::ExpiredIdleCap, Some(now));
        }
        let unprofitable: Vec<(StreamKey, Economics)> = state
            .streams
            .iter()
            .filter(|(key, stream)| stream.is_due(now, state.is_busy(&key.0)))
            .filter_map(|(key, stream)| {
                let idle = now.saturating_duration_since(stream.last_user);
                let check = state.ping_pays(stream, idle);
                (!check.pays).then(|| (key.clone(), check.economics))
            })
            .collect();
        for (key, economics) in &unprofitable {
            state.drop_with(
                key,
                SkipReason::DroppedUnlikelyReturn,
                Some(now),
                Some(*economics),
            );
        }
        let mut due: Vec<(Instant, StreamKey)> = state
            .streams
            .iter()
            .filter(|(key, stream)| stream.is_due(now, state.is_busy(&key.0)))
            .map(|(key, stream)| (stream.last_touch(), key.clone()))
            .collect();
        due.sort_by_key(|(touched, _)| *touched);
        let mut plans = Vec::new();
        let mut limited = Vec::new();
        for (_, key) in due {
            if !limited.is_empty() || !state.bucket.take() {
                limited.push(key);
                continue;
            }
            let used = state.next_clock();
            let Some(stream) = state.streams.get_mut(&key) else {
                continue;
            };
            stream.ping_in_flight = true;
            stream.last_ping = Some(now);
            stream.used = used;
            plans.push(PingPlan {
                worker: key.0.clone(),
                stream_key: key.1.clone(),
                model_id: stream.model_id.clone(),
                body: stream.body.clone(),
                target: stream.target.clone(),
            });
        }
        state.update_holds(now, &limited);
        Self::flush(state);
        plans
    }

    /// Records a finished ping. A failed ping drops its stream until the next user request.
    pub(crate) fn ping_finished(&self, worker: &str, stream_key: &str, ok: bool, cost_usd: f64) {
        let key = (worker.to_string(), stream_key.to_string());
        let mut state = self.lock();
        state.pings += 1;
        state.ping_cost_usd += cost_usd;
        if !ok {
            state.drop_stream(&key, SkipReason::DroppedPingFailed, None);
            Self::flush(state);
            return;
        }
        if let Some(stream) = state.streams.get_mut(&key) {
            stream.ping_in_flight = false;
        }
    }

    /// Summarises one session's requests. A user request counts as returning cached when it came
    /// back after an idle gap of at least the cache lifetime, read the cache, and had a ping in the gap.
    #[allow(
        clippy::cast_precision_loss,
        reason = "Cache-read token counts stay far below 2^53"
    )]
    #[must_use]
    pub(crate) fn summarize(requests: &[WarmRequest]) -> KeepWarmSummary {
        let mut ordered: Vec<&WarmRequest> = requests.iter().collect();
        ordered.sort_by_key(|request| request.ts_ms);
        let mut summary = KeepWarmSummary::default();
        let mut last_user_ms: Option<u64> = None;
        let mut last_ping_ms: Option<u64> = None;
        for request in ordered {
            if request.ping {
                summary.keepwarm_pings += 1;
                summary.keepwarm_cost_usd += request.ping_cost_usd;
                last_ping_ms = Some(request.ts_ms);
                continue;
            }
            if let Some(previous) = last_user_ms
                && request.ok
                && request.cache_read > 0
                && request.ts_ms.saturating_sub(previous) >= PROMPT_CACHE_TTL_MS
                && last_ping_ms.is_some_and(|ping| ping > previous)
            {
                summary.idle_returns_cached += 1;
                summary.est_write_saved_usd +=
                    request.cache_read as f64 * request.saving_per_token_usd;
            }
            last_user_ms = Some(request.ts_ms);
        }
        summary
    }

    /// Pings sent since startup and their summed cost in USD. Nothing here is persisted.
    pub fn ping_totals(&self) -> (u64, f64) {
        let state = self.lock();
        (state.pings, state.ping_cost_usd)
    }

    /// Streams not kept, dropped or held back since startup, per reason name. Nothing here is persisted.
    pub fn skip_counts(&self) -> BTreeMap<String, u64> {
        self.lock().skips.counts()
    }

    /// Drops every kept stream, for when keep-warm is switched off.
    pub fn clear(&self) {
        let mut state = self.lock();
        let keys: Vec<StreamKey> = state.streams.keys().cloned().collect();
        for key in &keys {
            state.drop_stream(key, SkipReason::DroppedKeepWarmOff, None);
        }
        state.total_bytes = 0;
        state.gaps.clear();
        Self::flush(state);
    }

    fn lock(&self) -> MutexGuard<'_, State> {
        self.state.lock().unwrap_or_else(PoisonError::into_inner)
    }
}

impl State {
    fn is_busy(&self, worker: &str) -> bool {
        self.in_flight.get(worker).is_some_and(|count| *count > 0)
    }
}

impl KeepWarm {
    /// Runs the keep-warm loop. Each tick starts one background ping per due stream, so a slow ping never delays the loop.
    pub fn spawn_refresh_loop(state: AppState) {
        tokio::spawn(async move {
            let mut ticker = tokio::time::interval(KEEP_WARM_TICK);
            loop {
                ticker.tick().await;
                let settings: KeepWarmConfig = state.config().keep_warm;
                if !settings.enabled {
                    state.keep_warm().clear();
                    continue;
                }
                let config = state.config();
                state.keep_warm().retain_profiles(|name| {
                    config
                        .profile(name)
                        .is_some_and(|profile| profile.enabled && profile.keep_warm)
                });
                for ping in state
                    .keep_warm()
                    .take_due(Instant::now(), settings.idle_cap)
                {
                    let state = state.clone();
                    tokio::spawn(async move {
                        crate::routes::proxy::send_keep_warm_ping(&state, ping).await;
                    });
                }
            }
        });
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;
    use std::time::{Duration, Instant};

    use bytes::Bytes;
    use serde_json::{Value, json};

    use super::*;
    use crate::consts::KEEP_WARM_MAX_BODY_BYTES;

    const IDLE_CAP: Duration = Duration::from_mins(30);

    fn model() -> ModelId {
        ModelId::parse("copilot/claude-haiku-5-5").unwrap()
    }

    fn request_body(pad: usize) -> Bytes {
        // Built by hand: `json!` would escape an 8 MiB string one character at a time in a debug build (about 1 s per body).
        // "x" needs no escaping, so the bytes are the same as the serialized object.
        let system = "x".repeat(pad);
        Bytes::from(format!(
            r#"{{"max_tokens":1024,"model":"claude-haiku-5-5","stream":true,"system":"{system}"}}"#
        ))
    }

    fn keys(registry: &KeepWarm) -> Vec<StreamKey> {
        let mut keys: Vec<StreamKey> = registry.lock().streams.keys().cloned().collect();
        keys.sort();
        keys
    }

    fn key(worker: &str, stream: &str) -> StreamKey {
        (worker.to_string(), stream.to_string())
    }

    fn rates(input: f64, cache_read: f64, cache_write: f64) -> TokenRates {
        TokenRates {
            input_per_1m: input,
            output_per_1m: 0.0,
            cache_read_per_1m: cache_read,
            cache_write_per_1m: cache_write,
        }
    }

    #[test]
    fn pings_pay_off_when_a_rewrite_costs_far_more_than_a_read() {
        // Claude-style prices: reads at a tenth of input, writes at 1.25 times.
        let claude = rates(1.0, 0.1, 1.25);
        assert!(KeepWarm::pays_off(claude, IDLE_CAP));
        // Over a week of pings the reads cost more than one rewrite.
        assert!(!KeepWarm::pays_off(claude, Duration::from_hours(168)));
    }

    #[test]
    fn pings_do_not_pay_off_when_the_cache_discount_is_small() {
        // A 75% discount with no write premium: seven and a half reads cost more than one rewrite.
        assert!(!KeepWarm::pays_off(rates(2.0, 0.5, 0.0), IDLE_CAP));
        // A 90% discount with no write premium still pays for 30 minutes.
        assert!(KeepWarm::pays_off(rates(2.0, 0.2, 0.0), IDLE_CAP));
    }

    #[test]
    fn a_model_with_no_known_price_is_kept() {
        assert!(KeepWarm::pays_off(TokenRates::default(), IDLE_CAP));
    }

    #[test]
    fn streams_of_a_profile_that_turned_keep_warm_off_are_dropped() {
        let registry = Arc::new(KeepWarm::default());
        let t0 = Instant::now();
        let other = ModelId::parse("groq/llama-3").unwrap();
        registry.touch(
            key("w", "kept"),
            &request_body(10),
            &model(),
            t0,
            4096,
            None,
        );
        registry.touch(key("w", "gone"), &request_body(10), &other, t0, 4096, None);

        registry.retain_profiles(|profile| profile != "groq");

        assert_eq!(keys(&registry), vec![key("w", "kept")]);
    }

    fn gap_stream(registry: &KeepWarm, t0: Instant, gaps: &[u64]) -> Instant {
        // Each touch comes `gap` seconds after the last one, so the registry records that gap.
        let mut at = t0;
        registry.touch(key("w", "s"), &request_body(10), &model(), at, 4096, None);
        for gap in gaps {
            at += Duration::from_secs(*gap);
            registry.touch(key("w", "s"), &request_body(10), &model(), at, 4096, None);
        }
        at
    }

    #[test]
    fn a_stream_with_a_returning_gap_history_keeps_pinging() {
        let registry = Arc::new(KeepWarm::default());
        let last = gap_stream(&registry, Instant::now(), &[300; 8]);
        // Every past gap ended within the next ping interval of 5 minutes of idle time.
        assert_eq!(
            registry
                .take_due(last + Duration::from_mins(4), IDLE_CAP)
                .len(),
            1
        );
    }

    #[test]
    fn a_stream_idle_past_every_past_gap_stops_pinging() {
        let registry = Arc::new(KeepWarm::default());
        let last = gap_stream(&registry, Instant::now(), &[60; 8]);
        // The user always came back within a minute, and 4 minutes have passed.
        let plans = registry.take_due(last + Duration::from_mins(4), IDLE_CAP);
        assert!(plans.is_empty());
        assert!(keys(&registry).is_empty());
    }

    #[test]
    fn a_stream_with_few_gaps_keeps_pinging_until_the_cap() {
        let registry = Arc::new(KeepWarm::default());
        let last = gap_stream(&registry, Instant::now(), &[60; 3]);
        assert_eq!(
            registry
                .take_due(last + Duration::from_mins(4), IDLE_CAP)
                .len(),
            1
        );
    }

    #[test]
    fn the_gap_history_is_bounded_and_cleared_with_the_streams() {
        let registry = Arc::new(KeepWarm::default());
        gap_stream(&registry, Instant::now(), &[10; 100]);
        assert_eq!(
            registry.lock().gaps.values().next().map(VecDeque::len),
            Some(KEEP_WARM_MAX_GAPS)
        );
        registry.clear();
        assert!(registry.lock().gaps.is_empty());
    }

    #[test]
    fn ping_due_at_240_seconds_not_before() {
        let registry = Arc::new(KeepWarm::default());
        let t0 = Instant::now();
        registry.touch(
            ("w".to_string(), "s".to_string()),
            &request_body(10),
            &model(),
            t0,
            4096,
            None,
        );

        let early = t0 + Duration::from_secs(239);
        assert!(registry.take_due(early, IDLE_CAP).is_empty());
        let due = t0 + Duration::from_mins(4);
        assert_eq!(registry.take_due(due, IDLE_CAP).len(), 1);
    }

    #[test]
    fn ping_body_sets_stream_false_and_one_token_and_keeps_the_rest() {
        let body = json!({
            "model": "claude-haiku-5-5",
            "max_tokens": 1024,
            "stream": true,
            "thinking": {"type": "adaptive"},
            "messages": [{"role": "user", "content": "hi"}],
        })
        .to_string();
        let ping: Value =
            serde_json::from_slice(&KeepWarm::ping_body(body.as_bytes()).unwrap()).unwrap();
        let original: Value = serde_json::from_str(&body).unwrap();
        assert_eq!(ping["stream"], Value::Bool(false));
        assert_eq!(ping["max_tokens"], json!(1));
        for field in ["model", "thinking", "messages"] {
            assert_eq!(ping[field], original[field], "{field}");
        }
    }

    #[test]
    fn in_flight_request_blocks_the_ping_until_its_lease_drops() {
        let registry = Arc::new(KeepWarm::default());
        let t0 = Instant::now();
        registry.touch(
            ("w".to_string(), "s".to_string()),
            &request_body(10),
            &model(),
            t0,
            4096,
            None,
        );
        let lease = registry.begin("w");

        let due = t0 + Duration::from_mins(4);
        assert!(registry.take_due(due, IDLE_CAP).is_empty());
        drop(lease);
        assert_eq!(registry.take_due(due, IDLE_CAP).len(), 1);
    }

    #[test]
    fn stream_is_dropped_after_the_idle_cap_and_pings_do_not_extend_it() {
        let registry = Arc::new(KeepWarm::default());
        let t0 = Instant::now();
        registry.touch(
            ("w".to_string(), "s".to_string()),
            &request_body(10),
            &model(),
            t0,
            4096,
            None,
        );

        let ping_at = t0 + Duration::from_mins(4);
        assert_eq!(registry.take_due(ping_at, IDLE_CAP).len(), 1);
        registry.ping_finished("w", "s", true, 0.0);
        assert!(registry.take_due(t0 + IDLE_CAP, IDLE_CAP).is_empty());
        assert!(keys(&registry).is_empty());
    }

    #[test]
    fn rate_cap_starts_ten_pings_at_one_instant_then_refills_one_every_six_seconds() {
        let registry = Arc::new(KeepWarm::default());
        let t0 = Instant::now();
        for index in 0..11 {
            registry.touch(
                ("w".to_string(), format!("s{index}").to_string()),
                &request_body(10),
                &model(),
                t0,
                4096,
                None,
            );
        }

        let due = t0 + Duration::from_mins(4);
        assert_eq!(registry.take_due(due, IDLE_CAP).len(), 10);
        let later = due + Duration::from_secs(6);
        assert_eq!(registry.take_due(later, IDLE_CAP).len(), 1);
    }

    #[test]
    fn seventeenth_stream_evicts_the_least_recently_used_one() {
        let registry = Arc::new(KeepWarm::default());
        let t0 = Instant::now();
        for index in 0..17 {
            let at = t0 + Duration::from_secs(index);
            registry.touch(
                ("w".to_string(), format!("s{index:02}")),
                &request_body(10),
                &model(),
                at,
                4096,
                None,
            );
        }

        let kept = keys(&registry);
        assert_eq!(kept.len(), KEEP_WARM_MAX_STREAMS);
        assert!(!kept.contains(&key("w", "s00")));
        assert!(kept.contains(&key("w", "s16")));
    }

    #[test]
    fn body_over_the_size_cap_is_not_kept() {
        let registry = Arc::new(KeepWarm::default());
        let t0 = Instant::now();
        let oversized = request_body(KEEP_WARM_MAX_BODY_BYTES + 1);
        registry.touch(
            ("w".to_string(), "s".to_string()),
            &oversized,
            &model(),
            t0,
            4096,
            None,
        );
        assert!(keys(&registry).is_empty());
    }

    #[test]
    fn total_bytes_stay_within_sixty_four_mib_by_evicting_the_oldest() {
        // Streams go in directly: parsing 9 bodies of 8 MiB through `touch` takes over a second in debug.
        let registry = Arc::new(KeepWarm::default());
        let t0 = Instant::now();
        {
            let mut state = registry.lock();
            for index in 0..9 {
                let body = Bytes::from(vec![b'x'; KEEP_WARM_MAX_BODY_BYTES - 1_000]);
                let used = state.next_clock();
                state.total_bytes += body.len();
                state.streams.insert(
                    key("w", &format!("s{index}")),
                    Stream {
                        body,
                        model_id: model(),
                        target: PingTarget::messages(None),
                        last_user: t0 + Duration::from_secs(index),
                        last_ping: None,
                        ping_in_flight: false,
                        used,
                        cached_tokens: 4096,
                        card: PriceCard::default(),
                        held: None,
                    },
                );
            }
            state.evict_to_bounds();
            assert_eq!(state.streams.len(), 8);
            assert!(state.total_bytes <= crate::consts::KEEP_WARM_MAX_TOTAL_BYTES);
        }
        assert!(!keys(&registry).contains(&key("w", "s0")));
    }

    #[test]
    fn response_under_the_cached_token_floor_is_not_kept() {
        let registry = Arc::new(KeepWarm::default());
        let t0 = Instant::now();
        registry.touch(
            ("w".to_string(), "s".to_string()),
            &request_body(10),
            &model(),
            t0,
            2047,
            None,
        );
        assert!(keys(&registry).is_empty());
        registry.touch(
            ("w".to_string(), "s".to_string()),
            &request_body(10),
            &model(),
            t0,
            2048,
            None,
        );
        assert_eq!(keys(&registry), vec![key("w", "s")]);
    }

    #[test]
    fn a_copilot_response_with_no_reported_write_keeps_its_prompt_as_cached() {
        let outcome = RequestOutcome {
            model_key: "copilot/claude-haiku-5-5".to_owned(),
            status: 200,
            tokens_in: 130_000,
            ..RequestOutcome::default()
        };
        let prompt = KeepWarm::cached_prompt(&outcome, PriceCard::default());
        assert_eq!(prompt.tokens, 130_000);

        let registry = Arc::new(KeepWarm::default());
        registry.touch_with(
            key("w", "s"),
            &request_body(10),
            &model(),
            Instant::now(),
            prompt,
            PingTarget::messages(None),
        );
        assert_eq!(keys(&registry), vec![key("w", "s")]);
    }

    #[test]
    fn a_response_from_another_provider_with_no_reported_write_is_not_counted_as_cached() {
        let outcome = RequestOutcome {
            model_key: "groq/llama-3".to_owned(),
            status: 200,
            tokens_in: 130_000,
            ..RequestOutcome::default()
        };
        assert_eq!(
            KeepWarm::cached_prompt(&outcome, PriceCard::default()).tokens,
            0
        );
    }

    /// Haiku-style prices: the base is 1.0 input, and above 100K prompt tokens the tier is 5x.
    fn haiku_card() -> PriceCard {
        PriceCard {
            base: rates(1.0, 0.1, 1.25),
            long_context: Some(crate::configs::LongContextTier {
                threshold_tokens: 100_000,
                rates: rates(5.0, 0.5, 6.25),
            }),
        }
    }

    #[test]
    fn a_haiku_stream_above_the_tier_pings_at_the_tier_price_and_the_ping_pays() {
        let registry = Arc::new(KeepWarm::default());
        let model = model();
        let prompt = || CachedPrompt {
            tokens: 130_000,
            card: haiku_card(),
        };
        // Eight 5-minute gaps: the user always returned within the next ping interval.
        let mut at = Instant::now();
        registry.touch_with(
            key("w", "s"),
            &request_body(10),
            &model,
            at,
            prompt(),
            PingTarget::messages(None),
        );
        for _ in 0..8 {
            at += Duration::from_mins(5);
            registry.touch_with(
                key("w", "s"),
                &request_body(10),
                &model,
                at,
                prompt(),
                PingTarget::messages(None),
            );
        }
        {
            let state = registry.lock();
            let stream = state.streams.get(&key("w", "s")).unwrap();
            let check = state.ping_pays(stream, Duration::from_mins(4));
            // 130K tokens at the tier: 0.5 per 1M for a read, 6.25 per 1M for a rewrite.
            assert!((check.economics.ping_usd - 0.065).abs() < 1e-9);
            assert!((check.economics.miss_usd - 0.8125).abs() < 1e-9);
            assert!((check.economics.odds - 1.0).abs() < 1e-9);
            assert!(check.pays);
        }
        let plans = registry.take_due(at + Duration::from_mins(4), IDLE_CAP);
        assert_eq!(plans.len(), 1);
    }

    #[test]
    fn failed_ping_drops_the_stream_and_counts_the_ping() {
        let registry = Arc::new(KeepWarm::default());
        let t0 = Instant::now();
        registry.touch(
            ("w".to_string(), "s".to_string()),
            &request_body(10),
            &model(),
            t0,
            4096,
            None,
        );
        assert_eq!(
            registry
                .take_due(t0 + Duration::from_mins(4), IDLE_CAP)
                .len(),
            1
        );

        registry.ping_finished("w", "s", false, 0.25);
        assert!(keys(&registry).is_empty());
        assert_eq!(registry.ping_totals(), (1, 0.25));
    }

    #[tokio::test]
    async fn completed_lease_keeps_its_stream_after_the_blocking_read() {
        let registry = Arc::new(KeepWarm::default());
        let mut lease = registry.begin("w");
        lease.record_stream(
            request_body(10),
            model(),
            Instant::now(),
            PingTarget::messages(None),
        );
        lease.complete(CachedPrompt {
            tokens: 4096,
            card: PriceCard::default(),
        });

        let deadline = Instant::now() + Duration::from_secs(5);
        while keys(&registry).is_empty() && Instant::now() < deadline {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        assert_eq!(keys(&registry).len(), 1);
        let state = registry.lock();
        assert!(!state.is_busy("w"));
    }
}
