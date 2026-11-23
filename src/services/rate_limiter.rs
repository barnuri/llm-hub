//! A token bucket per string key, with a bounded key map. One shared instance limits every
//! notification source: the ingest route and extension `ctx.notify(..)` calls.

use std::collections::HashMap;
use std::sync::{LazyLock, Mutex, PoisonError};

/// Notifications each source may send per minute.
pub const NOTIFICATIONS_PER_MINUTE: u32 = 60;
/// Keys a limiter tracks before it forgets the ones with a full bucket.
const MAX_TRACKED_KEYS: usize = 1_024;

/// The limiter shared by the ingest route (key = source) and extension delivery
/// (key = `ext:<name>`).
pub static NOTIFICATIONS: LazyLock<RateLimiter> = LazyLock::new(RateLimiter::notifications);

/// What [`RateLimiter::take`] decided for one call.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Take {
    /// The bucket had a token; the caller goes ahead.
    Allowed,
    /// The bucket was empty. `first_in_window` is true for the first refusal of the key in
    /// one window, so a caller logs once per window.
    Limited { first_in_window: bool },
}

/// One key's tokens, the last refill time and when it last reported a first refusal.
#[derive(Debug, Clone, Copy)]
struct Bucket {
    tokens: f64,
    last_ms: u64,
    limited_since_ms: Option<u64>,
}

/// A token bucket per key: `capacity` tokens that refill evenly over `window_ms`.
#[derive(Debug)]
pub struct RateLimiter {
    capacity: f64,
    window_ms: u64,
    max_keys: usize,
    buckets: Mutex<HashMap<String, Bucket>>,
}

impl RateLimiter {
    /// A limiter with `capacity` tokens per key that refill over `window_ms`.
    #[must_use]
    pub fn new(capacity: u32, window_ms: u64) -> RateLimiter {
        RateLimiter {
            capacity: f64::from(capacity),
            window_ms: window_ms.max(1),
            max_keys: MAX_TRACKED_KEYS,
            buckets: Mutex::new(HashMap::new()),
        }
    }

    /// The notification limiter: [`NOTIFICATIONS_PER_MINUTE`] per key per minute.
    #[must_use]
    pub fn notifications() -> RateLimiter {
        RateLimiter::new(NOTIFICATIONS_PER_MINUTE, 60_000)
    }

    /// Takes one token for `key` at `now_ms`; false when its bucket is empty.
    pub fn allow(&self, key: &str, now_ms: u64) -> bool {
        self.take(key, now_ms) == Take::Allowed
    }

    /// Takes one token for `key` at `now_ms` and says whether this is the key's first refusal
    /// in the current window.
    pub fn take(&self, key: &str, now_ms: u64) -> Take {
        let mut buckets = self.buckets.lock().unwrap_or_else(PoisonError::into_inner);
        if buckets.len() >= self.max_keys && !buckets.contains_key(key) {
            buckets.retain(|_, bucket| self.refill(bucket, now_ms) < self.capacity);
            if buckets.len() >= self.max_keys {
                // Every bucket is in use; the limiter evicts the fullest to stay bounded.
                let fullest = buckets
                    .iter()
                    .max_by(|a, b| {
                        self.refill(a.1, now_ms)
                            .total_cmp(&self.refill(b.1, now_ms))
                    })
                    .map(|(k, _)| k.clone());
                if let Some(fullest) = fullest {
                    buckets.remove(&fullest);
                }
            }
        }
        let bucket = buckets.entry(key.to_owned()).or_insert(Bucket {
            tokens: self.capacity,
            last_ms: now_ms,
            limited_since_ms: None,
        });
        bucket.tokens = self.refill(bucket, now_ms);
        bucket.last_ms = now_ms;
        if bucket.tokens >= 1.0 {
            bucket.tokens -= 1.0;
            return Take::Allowed;
        }
        let first_in_window = bucket
            .limited_since_ms
            .is_none_or(|since| now_ms.saturating_sub(since) >= self.window_ms);
        if first_in_window {
            bucket.limited_since_ms = Some(now_ms);
        }
        Take::Limited { first_in_window }
    }

    fn refill(&self, bucket: &Bucket, now_ms: u64) -> f64 {
        #[allow(
            clippy::cast_precision_loss,
            reason = "Elapsed ms and the window are far below 2^52"
        )]
        let (elapsed, window) = (
            now_ms.saturating_sub(bucket.last_ms) as f64,
            self.window_ms as f64,
        );
        (bucket.tokens + elapsed * self.capacity / window).min(self.capacity)
    }

    #[cfg(test)]
    fn tracked(&self) -> usize {
        self.buckets
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .len()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_bucket_empties_and_refills_over_the_window() {
        let limiter = RateLimiter::new(2, 1_000);
        assert!(limiter.allow("a", 0));
        assert!(limiter.allow("a", 0));
        assert!(!limiter.allow("a", 0));
        assert!(!limiter.allow("a", 400));
        assert!(limiter.allow("a", 900));
        assert!(limiter.allow("b", 900), "keys have their own bucket");
    }

    #[test]
    fn a_refusal_reports_first_in_window_once_per_window() {
        let limiter = RateLimiter::new(1, 1_000);
        assert_eq!(limiter.take("a", 0), Take::Allowed);
        assert_eq!(
            limiter.take("a", 10),
            Take::Limited {
                first_in_window: true
            }
        );
        assert_eq!(
            limiter.take("a", 20),
            Take::Limited {
                first_in_window: false
            }
        );
        // One token refills by 1_020 ms; it is spent, so the next call is a new window's first.
        assert_eq!(limiter.take("a", 1_020), Take::Allowed);
        assert_eq!(
            limiter.take("a", 1_030),
            Take::Limited {
                first_in_window: true
            }
        );
    }

    #[test]
    fn a_full_map_forgets_full_buckets() {
        let limiter = RateLimiter::new(2, 1_000);
        for i in 0..MAX_TRACKED_KEYS {
            limiter.allow(&format!("s{i}"), 10_000);
        }
        assert_eq!(limiter.tracked(), MAX_TRACKED_KEYS);
        // Every bucket is full again at 20 s, so a new key clears them.
        assert!(limiter.allow("new", 20_000));
        assert_eq!(limiter.tracked(), 1);
    }

    #[test]
    fn a_full_map_of_busy_buckets_evicts_one_and_stays_bounded() {
        let limiter = RateLimiter::new(2, 1_000);
        for i in 0..MAX_TRACKED_KEYS {
            limiter.allow(&format!("s{i}"), 0);
        }
        assert!(limiter.allow("new", 0));
        assert_eq!(limiter.tracked(), MAX_TRACKED_KEYS);
    }

    #[test]
    fn the_notification_limiter_allows_sixty_a_minute() {
        let limiter = RateLimiter::notifications();
        for _ in 0..NOTIFICATIONS_PER_MINUTE {
            assert!(limiter.allow("src", 0));
        }
        assert!(!limiter.allow("src", 0));
        assert!(limiter.allow("src", 1_000));
    }
}
